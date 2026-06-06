//! Plugin framework (issue #95, tracking #93).
//!
//! This module defines the trait surface, registry, and typed context used by Arborist's three plugin kinds:
//!
//! * **AI plugins** ([`ai::AiPlugin`]) — Claude / Copilot today; future Cursor / Aider / codex.
//! * **Custom-Process plugins** ([`custom_process::CustomProcessPlugin`]) — VS Code / Windows Explorer today; future browser launchers, ssh helpers, etc.
//! * **Dashboard-widget plugins** ([`dashboard_widget::DashboardWidgetBackend`]) — Git Status / AI Usage today.
//!
//! The registry is append-only and populated with built-ins in [`build_registry`].
//! Today that includes AI plugins (issue #96: Claude + Copilot), custom-process plugins (issue #97: VS Code + Windows Explorer), and dashboard
//! widgets (issue #98: Git Status + AI Usage).
//!
//! ## Design constraints (kept open for out-of-tree plugins later — see #93)
//!
//! * Plugin trait methods take **borrowed** inputs and return **owned** outputs. → trait-object-friendly.
//! * Trait methods receive a typed [`PluginCtx`] (or a sub-trait-specific context struct) instead of reaching into `AppContext`. → easy to mock from a
//!   WASM or dlopen bridge later.
//! * IDs are stable `&'static str` on the trait and `String` keys in the registry index. → future manifest can reference them.
//! * The registry is `append-only at startup`; duplicate registration is an explicit `Err`. → loading a dlopen-ed plugin later is just a
//!   `register_*` call from a different code path.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use crate::types::AppConfig;

pub mod ai;
pub mod custom_process;
pub mod dashboard_widget;

/// Base trait every plugin kind inherits from.
///
/// **Implementor contract:** return a `&'static str` identifier that is stable across releases (it becomes a persistent config key / serde
/// discriminator), plus a human-readable display name used in the UI.
pub trait Plugin: Send + Sync + 'static {
    /// Stable identifier. Must be unique within the plugin's kind (an AI plugin and a custom-process plugin may share a string id, but two AI
    /// plugins may not). Treated as a `String` key in the registry indexes; chosen to be a `&'static str` on the trait so implementors don't have
    /// to allocate.
    fn id(&self) -> &'static str;

    /// Human-readable plugin name surfaced in the UI (settings list, error messages, etc.).
    fn display_name(&self) -> &'static str;

    /// Default availability when `AppConfig.plugin_settings` has no explicit entry for this plugin.
    fn default_enabled(&self) -> bool {
        true
    }
}

/// Reason a `register_*` call failed. The registry rejects duplicate ids rather than panicking so the host can surface a clear error if a future
/// dlopen path tries to register a plugin whose id collides with a built-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    /// Another plugin of the same kind is already registered with this id.
    DuplicateId { kind: &'static str, id: String },
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateId { kind, id } => write!(f, "plugin id collision: a {kind} plugin with id {id:?} is already registered"),
        }
    }
}

impl std::error::Error for RegisterError {}

/// Typed context passed into plugin trait methods.
///
/// Plugins never reach into `AppContext` directly — instead they receive a `PluginCtx` carrying the minimum surface they need. The v1 surface stays
/// deliberately tiny (worktree path + config snapshot); future plugin surfaces should add fields only when a concrete behavior needs them. Keeping
/// the surface narrow is what lets us swap in a WASM or dlopen bridge without rewriting the trait surface.
#[derive(Debug, Clone, Copy)]
pub struct PluginCtx<'a> {
    /// Absolute path to the worktree the plugin is operating against. PTY spawn / git invocations / file lookups MUST treat this as authoritative;
    /// plugins MUST NOT inject this string into shell commands (it is passed to `portable-pty` as `cwd` by the host).
    pub worktree: &'a Path,
    /// Read-only snapshot of the persisted app config. Plugins MUST NOT mutate the config; configuration updates go through `config_set` on the
    /// host side.
    pub config: &'a AppConfig,
}

/// The registry the host wires into Tauri managed state via `app.manage(Arc::new(PluginRegistry::new()))`. Holds an `Arc<dyn …Plugin>` for each
/// registered plugin keyed by `Plugin::id()`. Iteration order is the registration order so the UI can render plugins in a stable sequence; lookups
/// by id are O(log n) via a side-index `BTreeMap`.
///
/// Append-only at startup: `register_*` returns [`RegisterError::DuplicateId`] on collision rather than overwriting. There is no `unregister`.
#[derive(Default)]
pub struct PluginRegistry {
    ai: Vec<Arc<dyn ai::AiPlugin>>,
    ai_index: BTreeMap<String, usize>,
    custom_process: Vec<Arc<dyn custom_process::CustomProcessPlugin>>,
    custom_process_index: BTreeMap<String, usize>,
    widgets: Vec<Arc<dyn dashboard_widget::DashboardWidgetBackend>>,
    widgets_index: BTreeMap<String, usize>,
}

impl PluginRegistry {
    /// Construct an empty registry. The production wiring builds one of these and immediately wraps it in `Arc` so the same registry instance is
    /// shared with every Tauri command via `State<'_, Arc<PluginRegistry>>`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an AI plugin. Returns [`RegisterError::DuplicateId`] if another AI plugin with the same id is already registered.
    pub fn register_ai(&mut self, plugin: Arc<dyn ai::AiPlugin>) -> Result<(), RegisterError> {
        let id = plugin.id().to_owned();
        if self.ai_index.contains_key(&id) {
            return Err(RegisterError::DuplicateId { kind: "ai", id });
        }
        let idx = self.ai.len();
        self.ai.push(plugin);
        self.ai_index.insert(id, idx);
        Ok(())
    }

    /// Register a custom-process plugin (e.g. VS Code, Explorer). Returns [`RegisterError::DuplicateId`] on id collision.
    pub fn register_custom_process(&mut self, plugin: Arc<dyn custom_process::CustomProcessPlugin>) -> Result<(), RegisterError> {
        let id = plugin.id().to_owned();
        if self.custom_process_index.contains_key(&id) {
            return Err(RegisterError::DuplicateId { kind: "custom_process", id });
        }
        let idx = self.custom_process.len();
        self.custom_process.push(plugin);
        self.custom_process_index.insert(id, idx);
        Ok(())
    }

    /// Register a dashboard-widget backend descriptor. Returns [`RegisterError::DuplicateId`] on id collision.
    pub fn register_widget(&mut self, plugin: Arc<dyn dashboard_widget::DashboardWidgetBackend>) -> Result<(), RegisterError> {
        let id = plugin.id().to_owned();
        if self.widgets_index.contains_key(&id) {
            return Err(RegisterError::DuplicateId {
                kind: "dashboard_widget",
                id,
            });
        }
        let idx = self.widgets.len();
        self.widgets.push(plugin);
        self.widgets_index.insert(id, idx);
        Ok(())
    }

    /// All registered AI plugins in registration order.
    #[must_use]
    pub fn ai(&self) -> &[Arc<dyn ai::AiPlugin>] {
        &self.ai
    }

    /// Look up a registered AI plugin by stable id.
    #[must_use]
    pub fn ai_by_id(&self, id: &str) -> Option<Arc<dyn ai::AiPlugin>> {
        self.ai_index.get(id).map(|i| Arc::clone(&self.ai[*i]))
    }

    /// Find the custom-process plugin that claims the supplied [`crate::types::CustomProcessDef`] (by command-shape sniffing). Returns the **first**
    /// matching plugin in registration order whose [`custom_process::CustomProcessPlugin::supported_on_platform`] also returns true, or `None` if no
    /// supported built-in plugin claims the def (the generic `CustomProcessDef` runtime handles it). Filtering by platform here keeps the
    /// "unsupported plugin wins then fails at spawn time" foot-gun from #97 unreachable.
    #[must_use]
    pub fn custom_process_for_def(&self, def: &crate::types::CustomProcessDef) -> Option<Arc<dyn custom_process::CustomProcessPlugin>> {
        self.custom_process.iter().find(|p| p.supported_on_platform() && p.matches(def)).cloned()
    }

    /// All registered custom-process plugins in registration order. Mostly useful for diagnostics; routine lookups should go through
    /// [`Self::custom_process_for_def`].
    #[must_use]
    pub fn custom_processes(&self) -> &[Arc<dyn custom_process::CustomProcessPlugin>] {
        &self.custom_process
    }

    /// All registered dashboard-widget backend descriptors in registration order.
    #[must_use]
    pub fn widgets(&self) -> &[Arc<dyn dashboard_widget::DashboardWidgetBackend>] {
        &self.widgets
    }
}

/// Construct the production plugin registry.
///
/// This is the single seam sub-issues #96 / #97 / #98 extend for future plugin additions: add a `reg.register_*(Arc::new(...))?` line here.
///
/// Returns a [`RegisterError`] if two built-in plugins of the same kind ever share an id — that would be a programming error caught immediately
/// at startup rather than papered over with `expect()`.
pub fn build_registry() -> Result<PluginRegistry, RegisterError> {
    let mut reg = PluginRegistry::new();
    for builtin in ai::BUILTIN_AI {
        reg.register_ai((builtin.factory)())?;
    }
    reg.register_custom_process(Arc::new(custom_process::vscode::VsCodePlugin))?;
    reg.register_custom_process(Arc::new(custom_process::explorer::ExplorerPlugin))?;
    reg.register_widget(Arc::new(dashboard_widget::git_status::GitStatusBackend))?;
    reg.register_widget(Arc::new(dashboard_widget::ai_usage::AiUsageBackend))?;
    Ok(reg)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestAi {
        id: &'static str,
    }
    impl Plugin for TestAi {
        fn id(&self) -> &'static str {
            self.id
        }
        fn display_name(&self) -> &'static str {
            "Test AI"
        }
    }
    impl ai::AiPlugin for TestAi {
        fn default_program(&self) -> &'static str {
            "test"
        }
        fn compose(&self, _inputs: &crate::compose::ComposeInputs<'_>, _quoter: crate::compose::Quoter) -> (String, Vec<crate::types::TempFileSpec>) {
            ("test".to_owned(), Vec::new())
        }
        fn env(&self, _session_id: &crate::types::SessionId) -> Vec<(String, std::ffi::OsString)> {
            Vec::new()
        }
        fn spawn_prep(&self, _session_id: &crate::types::SessionId) -> ai::SpawnPrep {
            ai::SpawnPrep::default()
        }
        fn metrics_watcher_kind(&self, _session_id: crate::types::SessionId, _cwd: &std::path::Path) -> Option<ai::MetricsWatcherKind> {
            None
        }
        fn starts_activity_events_watcher(&self) -> bool {
            false
        }
        fn create_ai_session_id(&self) -> Option<String> {
            None
        }
        fn restart_ai_session_policy(&self) -> ai::RestartAiSessionPolicy {
            ai::RestartAiSessionPolicy::Preserve
        }
        fn resume_requires_preflight(&self) -> bool {
            false
        }
        fn resume_args(&self, ai_session_id: &str) -> Vec<String> {
            vec!["--resume".to_owned(), ai_session_id.to_owned()]
        }
        fn ai_session_transcript_path(&self, home: &std::path::Path, _worktree_path: &std::path::Path, ai_session_id: &str) -> std::path::PathBuf {
            home.join(ai_session_id)
        }
    }

    struct TestProc {
        id: &'static str,
        matches_id: &'static str,
    }
    impl Plugin for TestProc {
        fn id(&self) -> &'static str {
            self.id
        }
        fn display_name(&self) -> &'static str {
            "Test Process"
        }
    }
    impl custom_process::CustomProcessPlugin for TestProc {
        fn matches(&self, def: &crate::types::CustomProcessDef) -> bool {
            def.id.0 == self.matches_id
        }
        fn supported_on_platform(&self) -> bool {
            true
        }
    }

    struct TestWidget {
        id: &'static str,
    }
    impl Plugin for TestWidget {
        fn id(&self) -> &'static str {
            self.id
        }
        fn display_name(&self) -> &'static str {
            "Test Widget"
        }
    }
    impl dashboard_widget::DashboardWidgetBackend for TestWidget {}

    fn make_custom_process_def(id: &str, command: &str) -> crate::types::CustomProcessDef {
        crate::types::CustomProcessDef {
            id: crate::types::CustomProcessDefId(id.to_owned()),
            name: id.to_owned(),
            kind: crate::types::CustomProcessKind::Application,
            command: command.to_owned(),
            enabled: true,
            icon: None,
            icon_data_uri: None,
        }
    }

    #[test]
    fn registers_and_lists_each_kind() {
        let mut reg = PluginRegistry::new();
        reg.register_ai(Arc::new(TestAi { id: "alpha" })).unwrap();
        reg.register_ai(Arc::new(TestAi { id: "beta" })).unwrap();
        reg.register_custom_process(Arc::new(TestProc {
            id: "vscode",
            matches_id: "vscode",
        }))
        .unwrap();
        reg.register_widget(Arc::new(TestWidget { id: "git-status" })).unwrap();

        let ids: Vec<&str> = reg.ai().iter().map(|p| p.id()).collect();
        assert_eq!(ids, vec!["alpha", "beta"], "AI plugins iterate in registration order");
        assert!(reg.ai_by_id("alpha").is_some());
        assert!(reg.ai_by_id("missing").is_none());
        assert_eq!(reg.custom_processes().len(), 1);
        assert_eq!(reg.widgets().len(), 1);
    }

    #[test]
    fn register_ai_rejects_duplicate_id() {
        let mut reg = PluginRegistry::new();
        reg.register_ai(Arc::new(TestAi { id: "claude" })).unwrap();
        let err = reg.register_ai(Arc::new(TestAi { id: "claude" })).unwrap_err();
        assert_eq!(
            err,
            RegisterError::DuplicateId {
                kind: "ai",
                id: "claude".into()
            }
        );
    }

    #[test]
    fn register_custom_process_rejects_duplicate_id() {
        let mut reg = PluginRegistry::new();
        reg.register_custom_process(Arc::new(TestProc {
            id: "vscode",
            matches_id: "vscode",
        }))
        .unwrap();
        let err = reg
            .register_custom_process(Arc::new(TestProc {
                id: "vscode",
                matches_id: "vscode",
            }))
            .unwrap_err();
        assert!(matches!(err, RegisterError::DuplicateId { kind: "custom_process", .. }));
    }

    #[test]
    fn register_widget_rejects_duplicate_id() {
        let mut reg = PluginRegistry::new();
        reg.register_widget(Arc::new(TestWidget { id: "git-status" })).unwrap();
        let err = reg.register_widget(Arc::new(TestWidget { id: "git-status" })).unwrap_err();
        assert!(matches!(
            err,
            RegisterError::DuplicateId {
                kind: "dashboard_widget",
                ..
            }
        ));
    }

    #[test]
    fn custom_process_for_def_returns_first_match_or_none() {
        let mut reg = PluginRegistry::new();
        reg.register_custom_process(Arc::new(TestProc {
            id: "vscode",
            matches_id: "vscode",
        }))
        .unwrap();
        let def = crate::types::CustomProcessDef {
            id: crate::types::CustomProcessDefId("vscode".to_owned()),
            name: "VS Code".to_owned(),
            kind: crate::types::CustomProcessKind::Application,
            command: "code .".to_owned(),
            enabled: true,
            icon: None,
            icon_data_uri: None,
        };
        assert!(reg.custom_process_for_def(&def).is_some());

        let other = crate::types::CustomProcessDef {
            id: crate::types::CustomProcessDefId("shell".to_owned()),
            ..def
        };
        assert!(reg.custom_process_for_def(&other).is_none());
    }

    struct TogglableProc {
        id: &'static str,
        matches_id: &'static str,
        supported: bool,
    }
    impl Plugin for TogglableProc {
        fn id(&self) -> &'static str {
            self.id
        }
        fn display_name(&self) -> &'static str {
            "Togglable Process"
        }
    }
    impl custom_process::CustomProcessPlugin for TogglableProc {
        fn matches(&self, def: &crate::types::CustomProcessDef) -> bool {
            def.id.0 == self.matches_id
        }
        fn supported_on_platform(&self) -> bool {
            self.supported
        }
    }

    #[test]
    fn custom_process_for_def_skips_unsupported_platform() {
        // Register an unsupported plugin first so it would "win" by registration order if the filter were absent — the supported plugin
        // registered second is the one we expect back. Mirrors the Windows-Explorer-on-Linux case from #97.
        let mut reg = PluginRegistry::new();
        reg.register_custom_process(Arc::new(TogglableProc {
            id: "explorer",
            matches_id: "shared",
            supported: false,
        }))
        .unwrap();
        reg.register_custom_process(Arc::new(TogglableProc {
            id: "fallback",
            matches_id: "shared",
            supported: true,
        }))
        .unwrap();
        let def = crate::types::CustomProcessDef {
            id: crate::types::CustomProcessDefId("shared".to_owned()),
            name: "Shared".to_owned(),
            kind: crate::types::CustomProcessKind::Application,
            command: "noop".to_owned(),
            enabled: true,
            icon: None,
            icon_data_uri: None,
        };
        let picked = reg.custom_process_for_def(&def).expect("expected the supported plugin to claim the def");
        assert_eq!(picked.id(), "fallback");
    }

    #[test]
    fn build_registry_registers_builtin_plugins() {
        let reg = build_registry().expect("build_registry must not collide on duplicate ids");
        let ai_ids: Vec<&str> = reg.ai().iter().map(|p| p.id()).collect();
        assert_eq!(ai_ids, vec!["claude", "copilot", "codex"]);
        let custom_process_ids: Vec<&str> = reg.custom_processes().iter().map(|p| p.id()).collect();
        assert_eq!(custom_process_ids, vec!["vscode", "explorer"]);
        let widget_ids: Vec<&str> = reg.widgets().iter().map(|w| w.id()).collect();
        assert_eq!(widget_ids, vec!["git-status", "ai-usage"]);
        let git_status = reg
            .widgets()
            .iter()
            .find(|w| w.id() == "git-status")
            .expect("git-status backend must be registered");
        assert_eq!(git_status.required_commands(), &["worktree_git_status", "worktree_pr_info"]);
        let ai_usage = reg
            .widgets()
            .iter()
            .find(|w| w.id() == "ai-usage")
            .expect("ai-usage backend must be registered");
        assert!(ai_usage.required_commands().is_empty());
    }

    #[test]
    fn build_registry_selects_vscode_for_code_command() {
        let reg = build_registry().expect("build_registry must not collide on duplicate ids");
        let picked = reg
            .custom_process_for_def(&make_custom_process_def("vscode", "code ."))
            .expect("expected vscode plugin for `code` command");
        assert_eq!(picked.id(), "vscode");
    }

    #[test]
    fn build_registry_applies_platform_gate_for_explorer_command() {
        let reg = build_registry().expect("build_registry must not collide on duplicate ids");
        let picked = reg.custom_process_for_def(&make_custom_process_def("explorer", "explorer ."));
        #[cfg(target_os = "windows")]
        assert_eq!(picked.map(|p| p.id()), Some("explorer"));
        #[cfg(not(target_os = "windows"))]
        assert!(picked.is_none(), "explorer plugin must be skipped on non-Windows");
    }

    #[test]
    fn build_registry_builtin_custom_process_matches_are_disjoint() {
        // The first-match-wins registry rule is only deterministic if built-ins do not overlap on command shape.
        let reg = build_registry().expect("build_registry must not collide on duplicate ids");
        let commands = [
            "code .",
            "code-insiders .",
            "env FOO=bar code .",
            "explorer .",
            "explorer.exe .",
            "notepad.exe",
            "pwsh -c code",
        ];
        for (idx, cmd) in commands.into_iter().enumerate() {
            let def = make_custom_process_def(&format!("case-{idx}"), cmd);
            let claiming_plugins: Vec<&str> = reg.custom_processes().iter().filter(|p| p.matches(&def)).map(|p| p.id()).collect();
            assert!(
                claiming_plugins.len() <= 1,
                "expected disjoint built-in matchers for command {cmd:?}, but got {:?}",
                claiming_plugins
            );
        }
    }

    #[test]
    fn registry_ai_ids_match_tool_discriminators() {
        let reg = build_registry().expect("build_registry must not collide on duplicate ids");
        let mut expected: Vec<&str> = crate::types::Tool::ALL.iter().map(|t| t.as_id()).collect();
        expected.sort_unstable();
        let mut actual: Vec<&str> = reg.ai().iter().map(|p| p.id()).collect();
        actual.sort_unstable();
        assert_eq!(actual, expected);
    }

    #[test]
    fn arc_registry_is_shareable() {
        // Mirrors the production wiring: the registry is wrapped in an `Arc` and shared between the Tauri managed-state container and any
        // background thread that needs read access. Cheap clone, shared view.
        let mut reg = PluginRegistry::new();
        reg.register_ai(Arc::new(TestAi { id: "claude" })).unwrap();
        let shared = Arc::new(reg);
        let clone = Arc::clone(&shared);
        assert_eq!(shared.ai().len(), 1);
        assert_eq!(clone.ai().len(), 1);
    }
}
