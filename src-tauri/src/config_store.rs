//! Persistence layer for Arborist (Phase 4).
//!
//! Two logical JSON stores live side-by-side in a single directory (typically the OS-specific `app_data_dir` provided by Tauri):
//!
//! * `config.json`   → [`AppConfig`]
//! * `sessions.json` → `BTreeMap<SessionId, Session>`
//!
//! Both files are written atomically using `tempfile::NamedTempFile::persist` so an interrupted write never leaves a truncated file. On Unix the
//! parent directory is `fsync`-ed after `persist` so the rename itself is durable.
//!
//! ## Crash & corruption handling
//!
//! `load_config` / `load_sessions` **never panic** on malformed input. If the JSON fails to parse (or fails schema validation), the offending file is
//! moved aside to `<name>.bad-<unix-timestamp>` and an empty/default value is returned. A `tracing::warn!` event with the
//! [`Error::ConfigQuarantined`](crate::types::Error::ConfigQuarantined) code
//! describes which file was quarantined and why.
//!
//! ## Path safety (`save_config`)
//!
//! * Relative paths in `workspaceRoot` or `worktreeRoots[]` are rejected with [`Error::InvalidPath`].
//! * The keys of `worktreePrelaunchCommands` (canonicalized worktree paths) are also rejected if relative.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tempfile::NamedTempFile;
use tracing::{debug, warn};

use crate::store_layout::StoreLayout;
use crate::types::{
    AppConfig, ChildId, CustomProcessDef, CustomProcessDefId, CustomProcessKind, Error, PartialAppConfig, PartialPluginSettingState,
    PartialPluginSettings, PluginSettingState, Session, SessionId, SessionMetricsEvent, SessionStatus, SubSessionRecord, Tool, WorktreeTab,
    WorktreeTabId, AI_LAUNCH_COMMAND_SETTING, CONFIG_VERSION_CURRENT,
};

const CONFIG_FILENAME: &str = "config.json";
const SESSIONS_FILENAME: &str = "sessions.json";

// --------------------------------------------------------------------------- ConfigStore
// ---------------------------------------------------------------------------

/// Handle to the on-disk store directory. Cheap to construct and clone.
///
/// All write paths (`save_config`, `save_session`, `remove_session`, `update_session_status`, `update_session_ai_session_id`,
/// `append_last_open_sub_session`, `remove_last_open_sub_session`) are serialized through a mutex shared by clones of the same handle. Without this,
/// load-modify-write paths called from different threads using the same `ConfigStore` instance (e.g. the PTY wait thread updating `status` while a
/// metrics watcher updates `ai_session_id`) would race and silently lose updates. Atomic file writes (`tempfile::persist`) only protect against torn
/// reads, not against lost updates.
///
/// Scope: this guard covers writes performed through clones of the same `ConfigStore` only. Separately opened `ConfigStore` instances pointing at the
/// same directory do **not** share this mutex and are therefore not serialized against each other — which is why command handlers route through the
/// managed `AppContext`'s store via `AppContext::store()` rather than calling `ConfigStore::open` per request. Concurrent access from a second
/// Arborist process **is** prevented at the `(branch, workspace)` granularity by the OS-level advisory lock acquired in
/// [`crate::boot::bind_workspace`] (held in
/// [`crate::workspace_scope::WorkspaceScope`] for the lifetime of the
/// running instance). Two binaries that bind the *same* `(branch, workspace)` tuple cannot run concurrently. A user editing `sessions.json` by hand
/// while the app is running is still not supported.
///
/// Concurrent reads (`load_config`, `load_sessions`) intentionally do **not** take the lock; if they race a writer they may observe either the pre-
/// or post-write state, which is the same guarantee `write_atomic` already provides.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    dir: PathBuf,
    /// Optional [`StoreLayout`] this store was constructed from. Set by
    /// [`ConfigStore::from_layout`] (the per-(branch, workspace) entry
    /// point used by `WorkspaceScope`); `None` when constructed via the legacy [`ConfigStore::open`] path (tests, examples, and any flat-directory
    /// caller). Callers that need the layout's auxiliary paths (lock file, seed lock, legacy seed sources) should use [`ConfigStore::layout`] and
    /// handle the `None` case.
    layout: Option<StoreLayout>,
    write_lock: Arc<Mutex<()>>,
}

impl ConfigStore {
    /// Open (or create) a store rooted at `dir`. The directory will be created if it does not yet exist.
    ///
    /// Prefer [`ConfigStore::from_layout`] in production code paths where a [`StoreLayout`] is available — it carries enough information to resolve
    /// the lock-file path, seed-lock path, and legacy fall-back seed paths. `open` remains the supported entry point for tests, examples, and other
    /// flat-directory callers.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, Error> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(Error::Io)?;
        Ok(Self {
            dir,
            layout: None,
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    /// Open (or create) a store at `layout.workspace_dir()`, retaining the [`StoreLayout`] for later access via [`Self::layout`]. This is the
    /// canonical entry point used by `WorkspaceScope` at boot and by the in-app workspace switch.
    pub fn from_layout(layout: StoreLayout) -> Result<Self, Error> {
        let dir = layout.workspace_dir();
        fs::create_dir_all(&dir).map_err(Error::Io)?;
        Ok(Self {
            dir,
            layout: Some(layout),
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    /// The [`StoreLayout`] this store was constructed from, when available. Returns `None` for stores opened via the legacy
    /// [`Self::open`] path. Use this to reach auxiliary paths
    /// (`lock_path`, `seed_lock_path`, legacy seed sources).
    #[must_use]
    pub fn layout(&self) -> Option<&StoreLayout> {
        self.layout.as_ref()
    }

    /// Filesystem directory backing this store. Mostly useful for tests and diagnostics.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn config_path(&self) -> PathBuf {
        self.dir.join(CONFIG_FILENAME)
    }

    fn sessions_path(&self) -> PathBuf {
        self.dir.join(SESSIONS_FILENAME)
    }

    // ----- AppConfig ------------------------------------------------------

    /// Load the persisted [`AppConfig`], applying semantic validation:
    ///
    /// * Quarantines and returns defaults if the file is missing or
    ///   unparseable.
    /// * Canonicalizes `workspaceRoot` and each `worktreeRoots[]`, dropping
    ///   (with a warning) any entry that no longer points at an existing
    ///   directory.
    /// * Drops per-worktree override keys whose paths don't canonicalize to an
    ///   existing directory (logged warning).
    pub fn load_config(&self) -> AppConfig {
        let path = self.config_path();
        let raw = match fs::read_to_string(&path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return default_seeded_config(),
            Err(e) => {
                warn!(
                    code = "Io",
                    path = %path.display(),
                    error = %e,
                    "config.json could not be read; returning defaults",
                );
                return default_seeded_config();
            }
        };

        let parsed: AppConfig = match serde_json::from_str(&raw) {
            Ok(c) => c,
            Err(e) => {
                let quarantined = quarantine(&path);
                warn!(
                    code = "ConfigQuarantined",
                    path = %path.display(),
                    quarantined = ?quarantined,
                    error = %e,
                    "config.json failed to parse; quarantined and returning defaults",
                );
                return default_seeded_config();
            }
        };

        // Future-version downgrade guard: if a newer build wrote this file (e.g. user downgraded to this branch), don't risk silently rewriting it
        // without the future fields. Quarantine and return defaults so the user notices.
        if parsed.config_version > CONFIG_VERSION_CURRENT {
            let quarantined = quarantine(&path);
            warn!(
                code = "ConfigQuarantined",
                path = %path.display(),
                quarantined = ?quarantined,
                found_version = parsed.config_version,
                supported_version = CONFIG_VERSION_CURRENT,
                "config.json was written by a newer build; quarantined and returning defaults",
            );
            return default_seeded_config();
        }

        let mut cfg = parsed;
        // v1/v2 → v3: `workspace_root` did not exist. If the user already had exactly one `worktree_roots` entry, treat that as the workspace so they
        // don't get pushed back through the first-boot picker for no reason. Multi-root and zero-root configs leave `workspace_root` as `None` and
        // the picker will be shown.
        if cfg.config_version < 3 && cfg.workspace_root.is_none() && cfg.worktree_roots.len() == 1 {
            cfg.workspace_root = Some(cfg.worktree_roots[0].clone());
        }
        // Bump the on-disk version stamp so the next save records the current schema explicitly. (`active_session_id` was the v1→v2 addition;
        // `workspace_root` is the v2→v3 addition; `custom_processes` and `last_open_sub_sessions` are the v3→v4 additions. All default via serde, so
        // missing fields hydrate cleanly already.)
        //
        // v3→v4: additively seed the built-in custom-process defs (`shell`, `open-folder`, `vscode`). Only IDs not already present are inserted, so a
        // user who edited / deleted a built-in does not get it silently re-injected on every launch.
        if cfg.config_version < 4 {
            seed_default_custom_processes(&mut cfg.custom_processes);
        }
        // v4→v5: drop `prelaunchCommands` + `worktreePrelaunchCommands` (handled implicitly — those JSON keys are unknown to the v5 schema and
        // serde silently ignores them on deserialize). The new `worktreePrepCommands` field comes in defaulted-empty via `#[serde(default)]`. No
        // value preservation is performed: the prior per-session-prelaunch use cases (`nvm use`, `source .env`, venv activation) are incompatible
        // with the new one-shot worktree-creation semantics, so silently re-running them once and never again would be more confusing than a clean
        // reset. (Single-user pre-1.0 cycle; explicitly approved by the owner — see issue #63.)
        //
        // v5→v6: synthesise WorktreeTab records from persisted sessions (Issue #44). Each unique canonical worktree_path gets one tab; tab order
        // mirrors the old session tab order (first occurrence of each worktree path wins). The active worktree tab is derived from the old
        // active_session_id's worktree path.
        if cfg.config_version < 6 && cfg.worktree_tabs.is_empty() {
            let sessions = self.load_sessions();
            migrate_v4_to_v5(&mut cfg, &sessions);
        }
        // v6→v7: reparent sub-session records from parent_session_id to parent_worktree_tab_id. Must run after v5→v6 because it reads the
        // synthesised worktree_tabs to resolve the mapping. Records whose parent session or matching worktree tab is missing are dropped.
        if cfg.config_version < 7 {
            let sessions = self.load_sessions();
            migrate_v5_to_v6(&mut cfg, &sessions);
        }
        // v7→v8: backfill `WorktreeTab.icon_id` for any tab still carrying the serde default (0). Tabs created via `worktree_tab_open_impl` always
        // populate this field; the backfill exists for two cases — (a) configs persisted under v6 (where the field did not exist), and (b) any
        // already-v7 record that somehow ended up with `icon_id == 0` (manual edit, partial write, a frontend that round-tripped without the field).
        // The second case is why this guard *also* runs whenever any tab has `icon_id == 0`, regardless of `config_version`: it preserves the
        // post-load invariant rather than letting a corrupted record persist forever. Walks `worktree_tab_order` so the assignment is deterministic
        // and matches the order tabs appear in the sidebar.
        if cfg.config_version < 8 || cfg.worktree_tabs.iter().any(|t| t.icon_id == 0) {
            migrate_v6_to_v7(&mut cfg);
        }
        // v9→v10: AI launch command overrides moved under plugin settings. Also sweep any non-empty legacy commands map regardless of version so a
        // hand-edited current config self-heals into the single source-of-truth field at load time.
        if cfg.config_version < 10 || !cfg.ai_launch_commands.commands.is_empty() {
            cfg.migrate_legacy_ai_launch_commands_to_plugin_settings();
        }
        if cfg.config_version < CONFIG_VERSION_CURRENT {
            cfg.config_version = CONFIG_VERSION_CURRENT;
        }
        sanitize_loaded_custom_processes(&mut cfg.custom_processes);
        sanitize_loaded_sub_session_records(&mut cfg.last_open_sub_sessions, &cfg.custom_processes, &cfg.worktree_tabs);
        validate_loaded_config(&mut cfg);

        cfg
    }

    /// Apply a partial update to the persisted [`AppConfig`] and write the merged result back to disk atomically.
    ///
    /// Each path field provided in `patch` is canonicalized; relative paths are rejected with [`Error::InvalidPath`]. Per-worktree override keys are
    /// canonicalized; keys that fail canonicalization are dropped with a warning rather than poisoning the whole call.
    pub fn save_config(&self, patch: PartialAppConfig) -> Result<AppConfig, Error> {
        self.save_config_with(patch, |_| false)
    }

    /// Variant of [`Self::save_config`] that also runs an arbitrary in-place mutation against the merged config **while holding the write lock**,
    /// then persists once. The mutation's return value is unused — we always write because the patch was already merged in. The lock spans load →
    /// merge → mutate → write, eliminating the read-modify-write race that would exist if a caller did `save_config` followed by `write_full`.
    pub fn save_config_with<F>(&self, patch: PartialAppConfig, mut mutate: F) -> Result<AppConfig, Error>
    where
        F: FnMut(&mut AppConfig) -> bool,
    {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = self.load_config();
        merge_partial(&mut cfg, patch)?;
        let _ = mutate(&mut cfg);
        cfg.config_version = CONFIG_VERSION_CURRENT;
        write_atomic(&self.config_path(), &cfg)?;
        Ok(cfg)
    }

    /// Write the supplied [`AppConfig`] verbatim, bumping the version stamp. Used by the icon backfill path which mutates the config in fields the
    /// public `PartialAppConfig` patch surface doesn't expose (`icon_data_uri` is backend-derived, not user-editable).
    ///
    /// **Caution:** holds the write lock for its own duration only; don't sandwich it with a `load_config` from a separate caller expecting an atomic
    /// read-modify-write — use
    /// [`Self::save_config_with`] for that case.
    pub fn write_full(&self, mut cfg: AppConfig) -> Result<AppConfig, Error> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        cfg.config_version = CONFIG_VERSION_CURRENT;
        write_atomic(&self.config_path(), &cfg)?;
        Ok(cfg)
    }

    // ----- Sessions -------------------------------------------------------

    /// Load all persisted [`Session`] records, keyed by ID. A missing or malformed file produces an empty map (with quarantine on parse failure).
    pub fn load_sessions(&self) -> BTreeMap<SessionId, Session> {
        let path = self.sessions_path();
        let raw = match fs::read_to_string(&path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
            Err(e) => {
                warn!(
                    code = "Io",
                    path = %path.display(),
                    error = %e,
                    "sessions.json could not be read; returning empty map",
                );
                return BTreeMap::new();
            }
        };
        match serde_json::from_str::<BTreeMap<SessionId, Session>>(&raw) {
            Ok(mut m) => {
                migrate_copilot_composed_commands(&mut m);
                m
            }
            Err(e) => {
                let quarantined = quarantine(&path);
                warn!(
                    code = "ConfigQuarantined",
                    path = %path.display(),
                    quarantined = ?quarantined,
                    error = %e,
                    "sessions.json failed to parse; quarantined and returning empty map",
                );
                BTreeMap::new()
            }
        }
    }

    /// Strict variant of [`Self::load_sessions`] for callers that perform destructive operations and cannot safely treat IO/parse failures as "no
    /// sessions exist". Returns the full session map on success or the underlying error otherwise. A missing file is still treated as an empty map (a
    /// fresh install has no `sessions.json`).
    pub fn try_load_sessions(&self) -> Result<BTreeMap<SessionId, Session>, Error> {
        let path = self.sessions_path();
        let raw = match fs::read_to_string(&path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(Error::Io(e)),
        };
        let mut m: BTreeMap<SessionId, Session> =
            serde_json::from_str(&raw).map_err(|e| Error::Internal(format!("sessions.json failed to parse: {} ({e})", path.display())))?;
        migrate_copilot_composed_commands(&mut m);
        Ok(m)
    }

    /// Persist a single session record (insert-or-replace).
    pub fn save_session(&self, session: &Session) -> Result<(), Error> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.load_sessions();
        all.insert(session.id, session.clone());
        write_atomic(&self.sessions_path(), &all)
    }

    /// Remove a session record by ID. Missing IDs are a no-op success.
    pub fn remove_session(&self, id: &SessionId) -> Result<(), Error> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.load_sessions();
        if all.remove(id).is_none() {
            return Ok(());
        }
        write_atomic(&self.sessions_path(), &all)
    }

    /// Mutate the persisted status (and optionally PID) of a session record. Used by the Phase 6 wait thread so reloaded sessions never advertise
    /// stale `running`/`pid` values.
    pub fn update_session_status(&self, id: &SessionId, status: SessionStatus, pid: Option<u32>) -> Result<(), Error> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.load_sessions();
        let Some(session) = all.get_mut(id) else {
            return Err(Error::NotFound(format!("session {id} not found")));
        };
        session.status = status;
        session.pid = pid;
        write_atomic(&self.sessions_path(), &all)
    }

    /// Mutate the persisted `ai_session_id` of a session record. Used by the metrics watchers' discovery callback so app-restart restore can resume
    /// the AI conversation. Returns `Ok(true)` when the value changed (and was therefore persisted), `Ok(false)` when the value was already current —
    /// the latter avoids a redundant disk write every poll once the watcher has converged.
    pub fn update_session_ai_session_id(&self, id: &SessionId, ai_session_id: Option<String>) -> Result<bool, Error> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.load_sessions();
        let Some(session) = all.get_mut(id) else {
            return Err(Error::NotFound(format!("session {id} not found")));
        };
        if session.ai_session_id == ai_session_id {
            return Ok(false);
        }
        session.ai_session_id = ai_session_id;
        write_atomic(&self.sessions_path(), &all)?;
        Ok(true)
    }

    /// Persist the latest metrics snapshot on a session record. Called on every `session://metrics` emission so restore can seed the frontend.
    /// Returns `Ok(false)` when the stored value already matches (avoids redundant disk writes).
    ///
    /// # Errors
    /// Returns `Error::Internal` if `metrics.session_id` does not match `id` (invariant violation).
    /// Returns `Error::NotFound` if `id` does not match any stored session.
    pub fn update_session_metrics(&self, id: &SessionId, metrics: SessionMetricsEvent) -> Result<bool, Error> {
        if metrics.session_id != *id {
            return Err(Error::Internal(format!(
                "update_session_metrics: id ({id}) does not match metrics.session_id ({})",
                metrics.session_id
            )));
        }
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.load_sessions();
        let Some(session) = all.get_mut(id) else {
            return Err(Error::NotFound(format!("session {id} not found")));
        };
        if session.last_metrics.as_ref().is_some_and(|prev| prev.same_payload_as(&metrics)) {
            return Ok(false);
        }
        session.last_metrics = Some(metrics);
        write_atomic(&self.sessions_path(), &all)?;
        Ok(true)
    }

    // ----- Sub-sessions (last_open_sub_sessions list) --------------------

    /// Append a sub-session record to `AppConfig.lastOpenSubSessions`, replacing any existing entry with the same id. Serialized via the shared
    /// `write_lock`.
    pub fn append_last_open_sub_session(&self, record: crate::types::SubSessionRecord) -> Result<(), Error> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = self.load_config();
        cfg.last_open_sub_sessions.retain(|r| r.id != record.id);
        cfg.last_open_sub_sessions.push(record);
        write_atomic(&self.config_path(), &cfg)
    }

    /// Remove a sub-session record by id. Missing ids are a no-op success.
    pub fn remove_last_open_sub_session(&self, id: &crate::types::SubSessionId) -> Result<(), Error> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = self.load_config();
        let before = cfg.last_open_sub_sessions.len();
        cfg.last_open_sub_sessions.retain(|r| &r.id != id);
        if cfg.last_open_sub_sessions.len() == before {
            return Ok(());
        }
        write_atomic(&self.config_path(), &cfg)
    }
}

// --------------------------------------------------------------------------- Loaded-session migrations
// ---------------------------------------------------------------------------

/// Rewrite Copilot session `composed_command` values that were persisted before we dropped the legacy `--interactive <string>` invocation. The modern
/// `copilot` CLI rejects that flag with "too many arguments". Any trailing `copilot ...` segment is replaced with bare `copilot`, so
/// restart-on-launch and `session_restart` work for sessions created by older builds.
fn migrate_copilot_composed_commands(sessions: &mut BTreeMap<SessionId, Session>) {
    for (id, session) in sessions.iter_mut() {
        if session.tool != Tool::Copilot {
            continue;
        }
        let segments: Vec<&str> = session.composed_command.split(" && ").collect();
        let Some((last, head)) = segments.split_last() else {
            continue;
        };
        if !last.trim_start().starts_with("copilot") || last.trim() == "copilot" {
            continue;
        }
        let mut rebuilt: Vec<String> = head.iter().map(|s| (*s).to_string()).collect();
        rebuilt.push("copilot".to_string());
        let new_cmd = rebuilt.join(" && ");
        warn!(
            session_id = %id,
            old = %session.composed_command,
            new = %new_cmd,
            "migrating stale Copilot composed_command (dropping legacy --interactive flag)",
        );
        session.composed_command = new_cmd;
    }
}

// --------------------------------------------------------------------------- Loaded-config validation
// ---------------------------------------------------------------------------

fn validate_loaded_config(cfg: &mut AppConfig) {
    // Clamp a hand-edited `sidebar_width_px` into [180, 480]. The patch path in `merge_partial` already clamps frontend-driven writes; this load-time
    // pass is the self-heal for a user who hand-edited `config.json` directly.
    if let Some(width) = cfg.sidebar_width_px {
        let clamped = width.clamp(crate::types::SIDEBAR_WIDTH_MIN_PX, crate::types::SIDEBAR_WIDTH_MAX_PX);
        if clamped != width {
            warn!(
                code = "InvalidValue",
                field = "sidebarWidthPx",
                found = width,
                clamped = clamped,
                "sidebarWidthPx was outside [180, 480]; clamped on load",
            );
            cfg.sidebar_width_px = Some(clamped);
        }
    }

    cfg.worktree_roots = std::mem::take(&mut cfg.worktree_roots)
        .into_iter()
        .filter_map(|p| match dunce::canonicalize(&p) {
            Ok(c) if c.is_dir() => Some(c),
            Ok(c) => {
                warn!(
                    code = "InvalidPath",
                    path = %c.display(),
                    "worktreeRoots entry is not a directory; dropping",
                );
                None
            }
            Err(e) => {
                warn!(
                    code = "InvalidPath",
                    path = %p.display(),
                    error = %e,
                    "worktreeRoots entry could not be canonicalized; dropping",
                );
                None
            }
        })
        .collect();

    // workspace_root: canonicalize, drop on failure (treated like a stale path — the picker will be re-shown on next launch).
    if let Some(ws) = cfg.workspace_root.take() {
        match dunce::canonicalize(&ws) {
            Ok(c) if c.is_dir() => cfg.workspace_root = Some(c),
            Ok(c) => {
                warn!(
                    code = "InvalidPath",
                    path = %c.display(),
                    "workspaceRoot is not a directory; clearing",
                );
            }
            Err(e) => {
                warn!(
                    code = "InvalidPath",
                    path = %ws.display(),
                    error = %e,
                    "workspaceRoot could not be canonicalized; clearing",
                );
            }
        }
    }

    // Drop the per-worktree prelaunch overrides map silently — issue #63 retired this concept along with the per-session prelaunch_commands. Old v4
    // configs lacked the new `worktree_prep_commands` field and serde just defaulted it; the unknown-field tolerance for `worktreePrelaunchCommands`
    // and `prelaunchCommands` is implicit (serde ignores unrecognised JSON keys). Nothing to do here.
}

// --------------------------------------------------------------------------- Partial merge / save validation
// ---------------------------------------------------------------------------

fn merge_partial(cfg: &mut AppConfig, patch: PartialAppConfig) -> Result<(), Error> {
    if let Some(v) = patch.config_version {
        cfg.config_version = v;
    }
    // workspace_root is tri-state like active_session_id: absent → leave alone; Some(None) → clear; Some(Some(path)) → set after validating it is an
    // absolute, existing directory.
    if let Some(ws) = patch.workspace_root {
        match ws {
            None => cfg.workspace_root = None,
            Some(p) => {
                if p.is_relative() {
                    return Err(Error::InvalidPath(format!("workspaceRoot must be absolute, got {}", p.display())));
                }
                let canon = dunce::canonicalize(&p).map_err(|e| Error::InvalidPath(format!("{}: {e}", p.display())))?;
                if !canon.is_dir() {
                    return Err(Error::InvalidPath(format!("workspaceRoot is not a directory: {}", canon.display())));
                }
                cfg.workspace_root = Some(canon);
            }
        }
    }
    if let Some(roots) = patch.worktree_roots {
        let mut out = Vec::with_capacity(roots.len());
        for p in roots {
            if p.is_relative() {
                return Err(Error::InvalidPath(format!("worktreeRoots entries must be absolute, got {}", p.display())));
            }
            let canon = dunce::canonicalize(&p).map_err(|e| Error::InvalidPath(format!("{}: {e}", p.display())))?;
            if !canon.is_dir() {
                return Err(Error::InvalidPath(format!("worktreeRoots entry is not a directory: {}", canon.display())));
            }
            out.push(canon);
        }
        cfg.worktree_roots = out;
    }
    if let Some(cmds) = patch.worktree_prep_commands {
        cfg.worktree_prep_commands = cmds;
    }
    if let Some(launch) = patch.ai_launch_commands {
        for (plugin_id, command) in launch.commands {
            cfg.set_ai_launch_command(plugin_id, command);
        }
    }
    if let Some(plugin_settings) = patch.plugin_settings {
        merge_plugin_settings(cfg, plugin_settings)?;
    }
    if let Some(s) = patch.last_open_sessions {
        cfg.last_open_sessions = s;
    }
    if let Some(t) = patch.tab_order {
        cfg.tab_order = t;
    }
    // Tri-state: `None` → don't touch; `Some(None)` → clear; `Some(Some(id))` → set.
    if let Some(active) = patch.active_session_id {
        cfg.active_session_id = active;
    }
    if let Some(mut defs) = patch.custom_processes {
        validate_custom_processes(&defs)?;
        // Preserve cached `icon_data_uri` across patches that don't carry it (the frontend never sends it — it's a backend derived field). Drop the
        // cache when `command` changes so the next backfill pass re-resolves.
        let prev: BTreeMap<crate::types::CustomProcessDefId, &crate::types::CustomProcessDef> =
            cfg.custom_processes.iter().map(|d| (d.id.clone(), d)).collect();
        for def in defs.iter_mut() {
            if def.icon_data_uri.is_some() {
                continue;
            }
            if let Some(old) = prev.get(&def.id) {
                if old.command == def.command {
                    def.icon_data_uri = old.icon_data_uri.clone();
                }
            }
        }
        cfg.custom_processes = defs;
    }
    if let Some(records) = patch.last_open_sub_sessions {
        cfg.last_open_sub_sessions = records;
    }
    if let Some(tabs) = patch.worktree_tabs {
        cfg.worktree_tabs = tabs;
    }
    if let Some(order) = patch.worktree_tab_order {
        cfg.worktree_tab_order = order;
    }
    if let Some(active) = patch.active_worktree_tab_id {
        cfg.active_worktree_tab_id = active;
    }
    if let Some(width) = patch.sidebar_width_px {
        // Clamp here so a hand-edited config / racing frontend cannot land us on an off-screen sidebar. We never reject — `min/max` is a soft policy
        // that should self-heal, not a fatal validation error like a non-existent path.
        cfg.sidebar_width_px = Some(width.clamp(crate::types::SIDEBAR_WIDTH_MIN_PX, crate::types::SIDEBAR_WIDTH_MAX_PX));
    }
    if let Some(theme) = patch.theme {
        cfg.theme = theme;
    }
    Ok(())
}

fn merge_plugin_settings(cfg: &mut AppConfig, patch: PartialPluginSettings) -> Result<(), Error> {
    merge_ai_plugin_settings(cfg, patch.ai)?;
    merge_plugin_kind_settings(&mut cfg.plugin_settings.custom_process, patch.custom_process);
    merge_plugin_kind_settings(&mut cfg.plugin_settings.dashboard_widget, patch.dashboard_widget);
    Ok(())
}

fn merge_ai_plugin_settings(cfg: &mut AppConfig, patch: BTreeMap<String, PartialPluginSettingState>) -> Result<(), Error> {
    for (plugin_id, state) in patch {
        if let Some(enabled) = state.enabled {
            cfg.plugin_settings.ai.entry(plugin_id.clone()).or_default().enabled = Some(enabled);
        }
        for (setting_id, value) in state.settings {
            if setting_id == AI_LAUNCH_COMMAND_SETTING {
                let Some(command) = value.as_str() else {
                    return Err(Error::InvalidPluginSettings(format!(
                        "pluginSettings.ai[{plugin_id}].settings.{AI_LAUNCH_COMMAND_SETTING} must be a string"
                    )));
                };
                cfg.set_ai_launch_command(plugin_id.clone(), command.to_owned());
            } else {
                cfg.plugin_settings
                    .ai
                    .entry(plugin_id.clone())
                    .or_default()
                    .settings
                    .insert(setting_id, value);
            }
        }
    }
    Ok(())
}

fn merge_plugin_kind_settings(target: &mut BTreeMap<String, PluginSettingState>, patch: BTreeMap<String, PartialPluginSettingState>) {
    for (plugin_id, state) in patch {
        let entry = target.entry(plugin_id).or_default();
        if let Some(enabled) = state.enabled {
            entry.enabled = Some(enabled);
        }
        entry.settings.extend(state.settings);
    }
}

/// Reject obviously-invalid [`CustomProcessDef`] lists at the `config_set` boundary so corrupt state can't reach the runtime.
///
/// Rules (also enforced by the Settings UI in Phase 6, but the backend is the source of truth):
///
/// * Non-empty `id` and `name`.
/// * `id` matches `[a-zA-Z0-9_-]+` (so it can be safely used as a wire key and
///   React `key` without escaping).
/// * Non-empty `command`.
/// * IDs are unique within the list.
fn validate_custom_processes(defs: &[CustomProcessDef]) -> Result<(), Error> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for def in defs {
        if def.id.as_str().is_empty() {
            return Err(Error::InvalidCustomProcessDef("customProcesses[]: id must be non-empty".into()));
        }
        if !def.id.as_str().chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(Error::InvalidCustomProcessDef(format!(
                "customProcesses[]: id {:?} must match [a-zA-Z0-9_-]+",
                def.id.as_str()
            )));
        }
        if def.name.trim().is_empty() {
            return Err(Error::InvalidCustomProcessDef(format!(
                "customProcesses[{}]: name must be non-empty",
                def.id
            )));
        }
        if def.command.trim().is_empty() {
            return Err(Error::InvalidCustomProcessDef(format!(
                "customProcesses[{}]: command must be non-empty",
                def.id
            )));
        }
        if !seen.insert(def.id.as_str()) {
            return Err(Error::InvalidCustomProcessDef(format!(
                "customProcesses[]: duplicate id {:?}",
                def.id.as_str()
            )));
        }
    }
    Ok(())
}

// --------------------------------------------------------------------------- Atomic write
// ---------------------------------------------------------------------------

fn write_atomic<T: serde::Serialize>(target: &Path, value: &T) -> Result<(), Error> {
    let parent = target
        .parent()
        .ok_or_else(|| Error::InvalidPath(format!("target has no parent: {}", target.display())))?;
    fs::create_dir_all(parent).map_err(Error::Io)?;

    let serialized = serde_json::to_vec_pretty(value).map_err(Error::Serde)?;

    let mut tmp = NamedTempFile::new_in(parent).map_err(Error::Io)?;
    tmp.write_all(&serialized).map_err(Error::Io)?;
    tmp.flush().map_err(Error::Io)?;
    tmp.as_file().sync_all().map_err(Error::Io)?;
    tmp.persist(target).map_err(|e| Error::Io(e.error))?;

    #[cfg(unix)]
    {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    Ok(())
}

// --------------------------------------------------------------------------- Quarantine
// ---------------------------------------------------------------------------

fn quarantine(path: &Path) -> Option<PathBuf> {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let target = path.with_extension(format!("{}.bad-{ts}", path.extension().and_then(|e| e.to_str()).unwrap_or("json"),));
    match fs::rename(path, &target) {
        Ok(_) => Some(target),
        Err(e) => {
            warn!(
                code = "Io",
                path = %path.display(),
                error = %e,
                "failed to quarantine corrupt file",
            );
            None
        }
    }
}

// --------------------------------------------------------------------------- Built-in custom-process defs (configVersion 3→4 seeding)
// ---------------------------------------------------------------------------

/// Reserved ID for the built-in "Shell" terminal launcher.
pub const BUILTIN_DEF_ID_SHELL: &str = "shell";
/// Reserved ID for the built-in "Open Folder" application launcher.
pub const BUILTIN_DEF_ID_OPEN_FOLDER: &str = "open-folder";
/// Reserved ID for the built-in "VS Code" application launcher.
pub const BUILTIN_DEF_ID_VSCODE: &str = "vscode";

/// Construct the on-first-launch [`AppConfig`] with the built-in custom-process defs already seeded. Used both for the missing-file path and the
/// quarantine-and-default-on-load path so a fresh install always sees the documented Launch menu entries.
fn default_seeded_config() -> AppConfig {
    let mut cfg = AppConfig::default();
    seed_default_custom_processes(&mut cfg.custom_processes);
    cfg
}

/// Drop persisted [`CustomProcessDef`]s that fail validation. Unlike the strict `config_set` boundary, the load path is *graceful*: an
/// individually-corrupt def (empty command, bad id) is logged and removed rather than nuking the whole config. Duplicate IDs keep the first
/// occurrence and drop later ones.
fn sanitize_loaded_custom_processes(defs: &mut Vec<CustomProcessDef>) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let original_len = defs.len();
    defs.retain(|def| {
        if let Err(e) = validate_custom_processes(std::slice::from_ref(def)) {
            warn!(
                code = "CustomProcessDefDropped",
                id = %def.id,
                error = %e,
                "dropping invalid custom process def from loaded config",
            );
            return false;
        }
        if !seen.insert(def.id.as_str().to_owned()) {
            warn!(
                code = "CustomProcessDefDropped",
                id = %def.id,
                "dropping duplicate custom process def id from loaded config",
            );
            return false;
        }
        true
    });
    if defs.len() != original_len {
        debug!(
            removed = original_len - defs.len(),
            kept = defs.len(),
            "sanitized custom process defs from loaded config",
        );
    }
}

/// Sanitize persisted [`SubSessionRecord`]s on load: drop any whose `def_id` no longer exists in the user's `custom_processes` (the def was deleted
/// between sessions), and backfill `composed_command` from the def for legacy v3→v4 records that didn't persist it. Both are silent —
/// restore-on-launch is best-effort.
fn sanitize_loaded_sub_session_records(records: &mut Vec<SubSessionRecord>, defs: &[CustomProcessDef], worktree_tabs: &[WorktreeTab]) {
    let by_id: std::collections::BTreeMap<&CustomProcessDefId, &CustomProcessDef> = defs.iter().map(|d| (&d.id, d)).collect();
    let tab_ids: std::collections::BTreeSet<WorktreeTabId> = worktree_tabs.iter().map(|t| t.id).collect();
    let original_len = records.len();
    records.retain_mut(|rec| {
        // Drop records that still lack the canonical parent_worktree_tab_id (pre-migration orphans).
        let Some(tab_id) = rec.parent_worktree_tab_id else {
            warn!(
                code = "SubSessionRecordDropped",
                id = %rec.id,
                "dropping sub-session record without parent_worktree_tab_id (migration orphan)",
            );
            return false;
        };
        // Drop records whose worktree tab no longer exists.
        if !tab_ids.contains(&tab_id) {
            warn!(
                code = "SubSessionRecordDropped",
                id = %rec.id,
                tab_id = %tab_id,
                "dropping sub-session record whose worktree tab is no longer present",
            );
            return false;
        }
        let Some(def) = by_id.get(&rec.def_id) else {
            warn!(
                code = "SubSessionRecordDropped",
                id = %rec.id,
                def_id = %rec.def_id,
                "dropping sub-session record whose def is no longer present",
            );
            return false;
        };
        if rec.composed_command.trim().is_empty() {
            rec.composed_command = def.command.clone();
        }
        // Clear the legacy field — it's not needed at runtime.
        rec.parent_session_id = None;
        true
    });
    if records.len() != original_len {
        debug!(
            removed = original_len - records.len(),
            kept = records.len(),
            "sanitized sub-session records from loaded config",
        );
    }
}

/// table) into `defs`. Only IDs not already present are appended; existing entries (including ones the user has edited or disabled) are left
/// untouched. Insertion order mirrors the plan table so a fresh install renders the menu in the documented order.
///
/// `vscode` is enabled by default iff the `code` binary is discoverable on `PATH` at seed time. The probe is best-effort: a transient PATH hiccup
/// just leaves it disabled (the user can flip the toggle in the Settings dialog).
pub fn seed_default_custom_processes(defs: &mut Vec<CustomProcessDef>) {
    let existing: BTreeSet<CustomProcessDefId> = defs.iter().map(|d| d.id.clone()).collect();
    for built_in in default_custom_processes() {
        if !existing.contains(&built_in.id) {
            defs.push(built_in);
        }
    }
}

/// v4→v5 migration (Issue #44): synthesise [`WorktreeTab`] records from existing sessions. Each unique canonical `worktree_path` gets one tab.
/// Tab order mirrors the old session `tab_order` (first occurrence of each path wins its position). The `active_worktree_tab_id` is derived from the
/// old `active_session_id`'s worktree path, and the matching tab's `active_child_id` is set to that session so the user lands on the same terminal
/// they had focused before migration.
///
/// Sessions whose `worktree_path` no longer exists on disk (or is no longer a directory) are **skipped** by this pass — no `WorktreeTab` is
/// synthesised for them. This avoids persisting "zombie" tabs whose `path` resolves to nothing, which would otherwise survive the migration intact
/// (the tab record itself never gets a fresh `validate_worktree` check until first use). The skipped sessions remain in `cfg.tab_order` /
/// `cfg.last_open_sessions` until `restore_all_sessions` prunes them via the standard worktree-missing branch on next launch — so the user sees
/// neither a phantom tab nor a permanent leak. (PR #65 review-9.)
fn migrate_v4_to_v5(cfg: &mut AppConfig, sessions: &BTreeMap<SessionId, Session>) {
    use crate::compose;

    // Pre-compute canonicalised worktree path for each session whose worktree currently exists. `compose::validate_worktree` does
    // `exists() + canonicalize() + is_dir()`, returning the canonical PathBuf on success. Sessions whose worktrees have been deleted (or replaced
    // with a regular file) are filtered out here so they never produce a tab below.
    let session_canonical: BTreeMap<SessionId, PathBuf> = sessions
        .iter()
        .filter_map(|(sid, s)| match compose::validate_worktree(&s.worktree_path) {
            Ok(canonical) => Some((*sid, canonical)),
            Err(e) => {
                tracing::warn!(
                    session_id = %sid,
                    worktree_path = %s.worktree_path.display(),
                    error = ?e,
                    "v4→v5 migration: skipping session whose worktree path is missing or invalid",
                );
                None
            }
        })
        .collect();

    let mut seen_paths: BTreeMap<PathBuf, WorktreeTabId> = BTreeMap::new();
    let mut ordered_tabs: Vec<WorktreeTab> = Vec::new();

    let mut ensure_tab = |sid: &SessionId| {
        let canonical = match session_canonical.get(sid) {
            Some(p) => p.clone(),
            None => return,
        };
        if seen_paths.contains_key(&canonical) {
            return;
        }
        let tab_id = WorktreeTabId::new();
        let name = canonical
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "worktree".into());
        let existing_labels: Vec<&str> = ordered_tabs.iter().map(|t| t.label.as_str()).collect();
        let label = compose::dedupe_label(&existing_labels, &name);
        let tab_index = ordered_tabs.len();
        // Assign a tree icon as we go, so the v4→v5 migration produces exactly the same distribution as if these tabs had been opened one at a time
        // through `worktree_tab_open_impl`. Without this, every migrated tab would land on the serde default (0) and rely on the v6→v7 backfill — but
        // we know the icon assignment we want here, so do it inline.
        let existing_icon_ids: Vec<u32> = ordered_tabs.iter().map(|t| t.icon_id).collect();
        let icon_id = crate::worktree_icon::pick_least_used_icon(&existing_icon_ids);
        ordered_tabs.push(WorktreeTab {
            id: tab_id,
            path: canonical.clone(),
            name,
            branch: None, // best-effort — skip during migration (no git commands)
            label,
            tab_index,
            active_child_id: None,
            icon_id,
        });
        seen_paths.insert(canonical, tab_id);
    };

    // Walk tab_order first (preserves user's ordering), then pick up any stragglers.
    for sid in &cfg.tab_order {
        ensure_tab(sid);
    }
    for sid in sessions.keys() {
        ensure_tab(sid);
    }

    // Derive active_worktree_tab_id from old active_session_id, and set that tab's active_child_id. Uses the *same* canonicalised path the tab was
    // created with, so this lookup cannot miss when the tab was created in the first pass.
    if let Some(active_sid) = cfg.active_session_id {
        if let Some(canonical) = session_canonical.get(&active_sid) {
            if let Some(&tab_id) = seen_paths.get(canonical) {
                cfg.active_worktree_tab_id = Some(tab_id);
                if let Some(tab) = ordered_tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.active_child_id = Some(ChildId::Session(active_sid));
                }
            }
        }
    }

    cfg.worktree_tab_order = ordered_tabs.iter().map(|t| t.id).collect();
    cfg.worktree_tabs = ordered_tabs;

    tracing::info!(
        tab_count = cfg.worktree_tabs.len(),
        session_count = sessions.len(),
        "v4→v5 migration: synthesised worktree tabs from existing sessions",
    );
}

/// v5→v6 migration: reparent `SubSessionRecord` entries from `parent_session_id` (an agent session) to `parent_worktree_tab_id` (a worktree tab).
///
/// For each record that still carries the legacy `parent_session_id`:
///   1. Look up the parent session in `sessions` to discover its `worktree_path`.
///   2. Canonicalise the worktree path (via `compose::validate_worktree`).
///   3. Find the `WorktreeTab` whose canonical path matches.
///   4. Set `parent_worktree_tab_id` and clear `parent_session_id`.
///
/// Records whose parent session is missing, whose worktree path is invalid, or that have no matching `WorktreeTab` are dropped with a
/// distinguishing `tracing::warn!`.
fn migrate_v5_to_v6(cfg: &mut AppConfig, sessions: &BTreeMap<SessionId, Session>) {
    use crate::compose;

    // Build a lookup: canonical worktree path → WorktreeTabId.
    let mut path_to_tab: BTreeMap<PathBuf, WorktreeTabId> = BTreeMap::new();
    for tab in &cfg.worktree_tabs {
        match compose::validate_worktree(&tab.path) {
            Ok(canonical) => {
                path_to_tab.entry(canonical).or_insert(tab.id);
            }
            Err(_) => {
                // Tab's path may not exist on this machine any more — that's OK, it just won't match any sub-sessions.
            }
        }
    }

    let original_len = cfg.last_open_sub_sessions.len();
    cfg.last_open_sub_sessions.retain_mut(|rec| {
        // Already migrated (has parent_worktree_tab_id) — keep as-is.
        if rec.parent_worktree_tab_id.is_some() {
            rec.parent_session_id = None;
            return true;
        }

        let Some(parent_sid) = rec.parent_session_id else {
            tracing::warn!(
                code = "SubSessionRecordDropped",
                id = %rec.id,
                "v5→v6 migration: dropping sub-session record with neither parent_session_id nor parent_worktree_tab_id",
            );
            return false;
        };

        let Some(parent) = sessions.get(&parent_sid) else {
            tracing::warn!(
                code = "SubSessionRecordDropped",
                id = %rec.id,
                parent_session_id = %parent_sid,
                "v5→v6 migration: dropping sub-session record whose parent session is missing",
            );
            return false;
        };

        let canonical = match compose::validate_worktree(&parent.worktree_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    code = "SubSessionRecordDropped",
                    id = %rec.id,
                    parent_session_id = %parent_sid,
                    worktree_path = %parent.worktree_path.display(),
                    error = ?e,
                    "v5→v6 migration: dropping sub-session record whose parent session worktree path is invalid",
                );
                return false;
            }
        };

        let Some(&tab_id) = path_to_tab.get(&canonical) else {
            tracing::warn!(
                code = "SubSessionRecordDropped",
                id = %rec.id,
                parent_session_id = %parent_sid,
                worktree_path = %parent.worktree_path.display(),
                "v5→v6 migration: dropping sub-session record whose worktree path has no matching WorktreeTab",
            );
            return false;
        };

        rec.parent_worktree_tab_id = Some(tab_id);
        rec.parent_session_id = None;
        true
    });

    let dropped = original_len - cfg.last_open_sub_sessions.len();
    tracing::info!(
        migrated = cfg.last_open_sub_sessions.len(),
        dropped,
        "v5→v6 migration: reparented sub-session records from parent_session_id to parent_worktree_tab_id",
    );
}

/// v6→v7 migration (Issue #45): backfill [`WorktreeTab::icon_id`] for every tab that still carries the serde default `0` (i.e. was loaded from v6
/// JSON written before the field existed). The assignment walks `worktree_tab_order` so it is deterministic and matches the order tabs appear in the
/// sidebar; tabs already carrying a non-zero `icon_id` are left untouched so the v7→current load path is idempotent and a partially-migrated
/// (e.g. crash mid-write) config converges to the same end state on retry.
///
/// Stragglers — tabs present in `worktree_tabs` but missing from `worktree_tab_order` (this shouldn't happen on a well-formed config, but the loader
/// is intentionally lenient about reordering drift) — are picked up in a second pass after the order-list walk so the migration can't leave any tab
/// at `icon_id == 0`.
fn migrate_v6_to_v7(cfg: &mut AppConfig) {
    use crate::worktree_icon::pick_least_used_icon;

    let mut backfilled = 0_usize;
    let mut backfill_one = |tab: &mut WorktreeTab, existing: &[u32]| {
        if tab.icon_id == 0 {
            tab.icon_id = pick_least_used_icon(existing);
            backfilled += 1;
        }
    };

    // First pass: tabs in their authoritative sidebar order. Take a snapshot of the already-assigned icon ids before we start mutating, then update
    // it as we assign — that way the deterministic "lowest icon at min count" tiebreak observes each fresh assignment when picking the next one.
    let mut existing_icon_ids: Vec<u32> = cfg.worktree_tabs.iter().filter(|t| t.icon_id != 0).map(|t| t.icon_id).collect();
    for id in cfg.worktree_tab_order.clone() {
        if let Some(tab) = cfg.worktree_tabs.iter_mut().find(|t| t.id == id) {
            let before = tab.icon_id;
            backfill_one(tab, &existing_icon_ids);
            if before == 0 && tab.icon_id != 0 {
                existing_icon_ids.push(tab.icon_id);
            }
        }
    }
    // Second pass: any tab not in `worktree_tab_order` (drift defense). Same incremental update pattern.
    let order_ids: std::collections::BTreeSet<WorktreeTabId> = cfg.worktree_tab_order.iter().copied().collect();
    for tab in cfg.worktree_tabs.iter_mut() {
        if !order_ids.contains(&tab.id) {
            let before = tab.icon_id;
            backfill_one(tab, &existing_icon_ids);
            if before == 0 && tab.icon_id != 0 {
                existing_icon_ids.push(tab.icon_id);
            }
        }
    }

    if backfilled > 0 {
        tracing::info!(backfilled, "v6→v7 migration: assigned tree icons to worktree tabs that lacked icon_id");
    }
}

/// The full ordered list of built-in defs, regardless of whether they are already present in any particular config. Test-only callers may use this
/// for assertions; production code should call
/// [`seed_default_custom_processes`] which is additive.
#[must_use]
pub fn default_custom_processes() -> Vec<CustomProcessDef> {
    vec![
        CustomProcessDef {
            id: CustomProcessDefId::new(BUILTIN_DEF_ID_SHELL),
            name: "Shell".to_owned(),
            kind: CustomProcessKind::Terminal,
            command: default_shell_command(),
            enabled: true,
            icon: None,
            icon_data_uri: None,
        },
        CustomProcessDef {
            id: CustomProcessDefId::new(BUILTIN_DEF_ID_OPEN_FOLDER),
            name: "Open Folder".to_owned(),
            kind: CustomProcessKind::Application,
            command: default_open_folder_command().to_owned(),
            enabled: true,
            icon: None,
            icon_data_uri: None,
        },
        CustomProcessDef {
            id: CustomProcessDefId::new(BUILTIN_DEF_ID_VSCODE),
            name: "VS Code".to_owned(),
            kind: CustomProcessKind::Application,
            command: "code .".to_owned(),
            enabled: command_on_path("code"),
            icon: None,
            icon_data_uri: None,
        },
    ]
}

fn default_shell_command() -> String {
    // Phase 1 keeps this minimal: launch the platform shell interactively. The PTY pool will spawn it via `$SHELL -c <cmd>` (Unix) or `%COMSPEC% /c
    // <cmd>` (Windows), so the inner command is a fresh login-ish invocation of the same shell. We deliberately don't pass `--login` so we don't
    // fight the user's profile order.
    if cfg!(target_os = "windows") {
        "cmd".to_owned()
    } else {
        // Use $SHELL when set, but only if it looks like a sane absolute path with no shell-metacharacters. A weird $SHELL (containing spaces,
        // quotes, `;`, `&`, `|`, `$`, backticks, newlines, …) would be re-interpreted by the launcher's `sh -c`, so we fall back to `sh -i` rather
        // than persist a footgun into the user's seed.
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| {
                let s = s.trim();
                !s.is_empty() && std::path::Path::new(s).is_absolute() && !s.chars().any(|c| c.is_whitespace() || "\"'`$&|;<>()\\*?[]{}".contains(c))
            })
            .unwrap_or_else(|| "sh".to_owned());
        format!("{shell} -i")
    }
}

const fn default_open_folder_command() -> &'static str {
    if cfg!(target_os = "windows") {
        "explorer ."
    } else if cfg!(target_os = "macos") {
        "open ."
    } else {
        "xdg-open ."
    }
}

/// Return `true` if `cmd` resolves to an executable on the current process's `PATH`. Pure-std implementation so we don't have to pull in the `which`
/// crate just for this best-effort probe. Errors and missing `PATH` both yield `false`.
///
/// On Unix, requires at least one executable bit (`0o111`) so a stray non-executable file named like the command on `PATH` doesn't enable a launcher
/// that will fail to spawn.
#[must_use]
pub fn command_on_path(cmd: &str) -> bool {
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    let exe_suffixes: &[&str] = if cfg!(target_os = "windows") {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path) {
        for suffix in exe_suffixes {
            let candidate = if suffix.is_empty() {
                dir.join(cmd)
            } else {
                dir.join(format!("{cmd}{suffix}"))
            };
            if !candidate.is_file() {
                continue;
            }
            if is_executable(&candidate) {
                return true;
            }
        }
    }
    false
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    // On Windows, `is_file()` + a recognized suffix from PATHEXT-ish list is the practical equivalent. We don't crack `PATHEXT` here yet.
    true
}

// --------------------------------------------------------------------------- Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CustomProcessDef, CustomProcessDefId, CustomProcessKind, SessionStatus, SubSessionId, SubSessionRecord, TempFileSpec};
    use pretty_assertions::assert_eq;
    use std::fs;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn canon(p: &Path) -> PathBuf {
        dunce::canonicalize(p).expect("canon")
    }

    fn make_session(id: Uuid, label: &str, dir: &Path) -> Session {
        Session {
            id: SessionId(id),
            tool: Tool::Claude,
            worktree_path: dir.to_path_buf(),
            worktree_name: label.to_owned(),
            label: label.to_owned(),
            composed_command: format!("claude {label}"),
            structured_command: None,
            command_provenance: Vec::new(),
            status: SessionStatus::Running,
            pid: Some(42),
            created_at: 1_700_000_000,
            tab_index: 0,
            temp_files: vec![TempFileSpec {
                path: dir.join("sp.md"),
                contents: "ctx".to_owned(),
            }],
            ai_session_id: None,
            last_metrics: None,
        }
    }

    // ----- ConfigStore: load/save ---------------------------------------

    #[test]
    fn load_config_returns_defaults_when_missing() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        // Fresh-install path seeds the built-in custom-process defs; every other field must equal AppConfig::default().
        let mut expected = AppConfig::default();
        seed_default_custom_processes(&mut expected.custom_processes);
        assert_eq!(store.load_config(), expected);
    }

    #[test]
    fn malformed_config_is_quarantined_and_defaults_returned() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        fs::write(td.path().join(CONFIG_FILENAME), b"{not json").expect("write");

        let cfg = store.load_config();
        let mut expected = AppConfig::default();
        seed_default_custom_processes(&mut expected.custom_processes);
        assert_eq!(cfg, expected);

        let badfiles: Vec<_> = fs::read_dir(td.path())
            .expect("rd")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("config.json.bad-"))
            .collect();
        assert_eq!(badfiles.len(), 1, "expected exactly one quarantine file");
    }

    #[test]
    fn malformed_sessions_is_quarantined_and_empty_map_returned() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        fs::write(td.path().join(SESSIONS_FILENAME), b"###bad###").expect("write");

        let map = store.load_sessions();
        assert!(map.is_empty());

        let badfiles: Vec<_> = fs::read_dir(td.path())
            .expect("rd")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("sessions.json.bad-"))
            .collect();
        assert_eq!(badfiles.len(), 1);
    }

    #[test]
    fn try_load_sessions_returns_ok_empty_when_file_missing() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let map = store.try_load_sessions().expect("ok on missing file");
        assert!(map.is_empty());
    }

    #[test]
    fn try_load_sessions_surfaces_parse_failure_without_quarantine() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let path = td.path().join(SESSIONS_FILENAME);
        fs::write(&path, b"###bad###").expect("write");

        let err = store.try_load_sessions().expect_err("expected parse failure");
        assert!(matches!(err, Error::Internal(_)), "got {err:?}");

        // Strict variant must NOT quarantine — the caller (a destructive operation) needs the file intact so it can be inspected/repaired.
        assert!(path.exists(), "try_load_sessions must not quarantine the bad file");
        let badfiles: Vec<_> = fs::read_dir(td.path())
            .expect("rd")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("sessions.json.bad-"))
            .collect();
        assert!(badfiles.is_empty(), "try_load_sessions must not produce quarantine files");
    }

    #[test]
    fn merge_preserves_unspecified_fields() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");

        // First write: set worktree_prep_commands.
        let first = store
            .save_config(PartialAppConfig {
                worktree_prep_commands: Some(vec!["echo hi".to_owned()]),
                ..Default::default()
            })
            .expect("ok");
        assert_eq!(first.worktree_prep_commands, vec!["echo hi".to_owned()]);

        // Second write: set tab_order only — worktree_prep_commands must survive.
        let id = SessionId::new();
        let second = store
            .save_config(PartialAppConfig {
                tab_order: Some(vec![id]),
                ..Default::default()
            })
            .expect("ok");
        assert_eq!(second.worktree_prep_commands, vec!["echo hi".to_owned()]);
        assert_eq!(second.tab_order, vec![id]);
    }

    #[test]
    fn save_config_ai_launch_empty_string_patch_keeps_cached_icon_for_default_command() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let plugin_id = "claude".to_owned();
        let cached = "data:image/png;base64,KEEP".to_owned();

        store
            .save_config_with(PartialAppConfig::default(), |cfg| {
                cfg.ai_launch_commands.icon_data_uris.insert(plugin_id.clone(), Some(cached.clone()));
                true
            })
            .expect("seed icon cache");

        let after = store
            .save_config(PartialAppConfig {
                ai_launch_commands: Some(crate::types::PartialAiLaunchCommands {
                    commands: BTreeMap::from([(plugin_id.clone(), String::new())]),
                }),
                ..Default::default()
            })
            .expect("patch");

        assert_eq!(
            after.ai_launch_commands.icon_data_uris.get(&plugin_id).and_then(Option::as_deref),
            Some("data:image/png;base64,KEEP"),
            "missing-key and empty-string command are both default; cache should stay warm",
        );
    }

    #[test]
    fn save_config_ai_launch_command_change_clears_cached_icon() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let plugin_id = "claude".to_owned();

        store
            .save_config_with(PartialAppConfig::default(), |cfg| {
                cfg.ai_launch_commands.commands.insert(plugin_id.clone(), "old-cmd".to_owned());
                cfg.ai_launch_commands
                    .icon_data_uris
                    .insert(plugin_id.clone(), Some("data:image/png;base64,OLD".to_owned()));
                true
            })
            .expect("seed prior command+icon");

        let after = store
            .save_config(PartialAppConfig {
                ai_launch_commands: Some(crate::types::PartialAiLaunchCommands {
                    commands: BTreeMap::from([(plugin_id.clone(), "new-cmd".to_owned())]),
                }),
                ..Default::default()
            })
            .expect("patch");

        assert_eq!(after.ai_launch_command_for_id(&plugin_id), "new-cmd");
        assert!(
            !after.ai_launch_commands.commands.contains_key(&plugin_id),
            "legacy commands map is read-only compatibility; new writes must land in pluginSettings"
        );
        assert!(
            !after.ai_launch_commands.icon_data_uris.contains_key(&plugin_id),
            "changed command must invalidate cached icon for re-resolution",
        );
    }

    #[test]
    fn save_config_plugin_settings_deep_merges_and_invalidates_ai_icon() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let plugin_id = "claude".to_owned();

        store
            .save_config_with(PartialAppConfig::default(), |cfg| {
                cfg.set_ai_launch_command(plugin_id.clone(), "old-cmd".to_owned());
                cfg.ai_launch_commands
                    .icon_data_uris
                    .insert(plugin_id.clone(), Some("data:image/png;base64,OLD".to_owned()));
                true
            })
            .expect("seed prior command+icon");

        let after = store
            .save_config(PartialAppConfig {
                plugin_settings: Some(PartialPluginSettings {
                    ai: BTreeMap::from([(
                        plugin_id.clone(),
                        PartialPluginSettingState {
                            enabled: Some(false),
                            settings: BTreeMap::from([(
                                AI_LAUNCH_COMMAND_SETTING.to_owned(),
                                crate::types::PluginSettingValue::String("new-cmd".to_owned()),
                            )]),
                        },
                    )]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .expect("patch");

        assert!(!after.ai_plugin_enabled_for_tool(Tool::Claude));
        assert_eq!(after.ai_launch_command_for_id(&plugin_id), "new-cmd");
        assert!(!after.ai_launch_commands.icon_data_uris.contains_key(&plugin_id));
    }

    #[test]
    fn load_v4_config_drops_legacy_prelaunch_fields() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let raw = serde_json::json!({
            "configVersion": 4,
            "worktreeRoots": [],
            "prelaunchCommands": ["nvm use", "source .env"],
            "worktreePrelaunchCommands": {
                "/some/path": ["asdf reshim"]
            },
            "lastOpenSessions": [],
            "tabOrder": []
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");

        let cfg = store.load_config();
        // v4→v5 migration drops both legacy keys silently — see issue #63 docs in `migrate_v_to_current`.
        assert_eq!(cfg.config_version, CONFIG_VERSION_CURRENT);
        assert!(
            cfg.worktree_prep_commands.is_empty(),
            "worktree_prep_commands must NOT be seeded from legacy prelaunch values — semantics differ",
        );
    }

    // ----- Session round-trip & status update ---------------------------

    #[test]
    fn session_round_trip_preserves_record() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let s = make_session(
            Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid"),
            "feat-x",
            td.path(),
        );
        store.save_session(&s).expect("save");
        let all = store.load_sessions();
        assert_eq!(all.get(&s.id), Some(&s));
    }

    #[test]
    fn remove_session_idempotent() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let s = make_session(Uuid::new_v4(), "x", td.path());
        store.save_session(&s).expect("save");
        store.remove_session(&s.id).expect("remove");
        store.remove_session(&s.id).expect("remove again");
        assert!(store.load_sessions().is_empty());
    }

    #[test]
    fn update_session_status_mutates_record() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let s = make_session(Uuid::new_v4(), "x", td.path());
        store.save_session(&s).expect("save");

        store.update_session_status(&s.id, SessionStatus::Exited, None).expect("update");
        let after = store.load_sessions();
        let updated = after.get(&s.id).expect("present");
        assert_eq!(updated.status, SessionStatus::Exited);
        assert_eq!(updated.pid, None);
    }

    #[test]
    fn update_session_status_returns_not_found_for_unknown_id() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let err = store
            .update_session_status(&SessionId::new(), SessionStatus::Exited, None)
            .expect_err("must fail");
        assert!(matches!(err, Error::NotFound(_)));
    }

    // ----- update_session_ai_session_id --------------------------------

    #[test]
    fn update_ai_session_id_persists_value() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let s = make_session(Uuid::new_v4(), "x", td.path());
        store.save_session(&s).expect("save");

        let changed = store.update_session_ai_session_id(&s.id, Some("ai-123".to_owned())).expect("update");
        assert!(changed, "first set must report a change");

        let after = store.load_sessions();
        assert_eq!(after.get(&s.id).expect("present").ai_session_id.as_deref(), Some("ai-123"),);
    }

    #[test]
    fn update_ai_session_id_is_idempotent() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let s = make_session(Uuid::new_v4(), "x", td.path());
        store.save_session(&s).expect("save");

        store.update_session_ai_session_id(&s.id, Some("same".to_owned())).expect("first update");
        let changed = store.update_session_ai_session_id(&s.id, Some("same".to_owned())).expect("second update");
        assert!(!changed, "no-op write must report no change");
    }

    #[test]
    fn update_ai_session_id_returns_not_found_for_unknown_id() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let err = store
            .update_session_ai_session_id(&SessionId::new(), Some("x".to_owned()))
            .expect_err("must fail");
        assert!(matches!(err, Error::NotFound(_)));
    }

    #[test]
    fn update_ai_session_id_can_clear_value() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let s = make_session(Uuid::new_v4(), "x", td.path());
        store.save_session(&s).expect("save");

        store.update_session_ai_session_id(&s.id, Some("ai-1".to_owned())).expect("set");
        let changed = store.update_session_ai_session_id(&s.id, None).expect("clear");
        assert!(changed);
        assert_eq!(store.load_sessions().get(&s.id).expect("present").ai_session_id, None,);
    }

    // ----- update_session_metrics ----------------------------------------

    #[test]
    fn update_session_metrics_persists_snapshot() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let s = make_session(Uuid::new_v4(), "x", td.path());
        store.save_session(&s).expect("save");

        let metrics = SessionMetricsEvent {
            session_id: s.id,
            model: Some("claude-sonnet-4-6".to_owned()),
            context_used_pct: Some(42),
            context_tokens_used: Some(84_000),
            context_tokens_limit: Some(200_000),
            input_tokens: Some(50_000),
            output_tokens: Some(10_000),
            observed_at: 1_700_000_100,
        };

        let changed = store.update_session_metrics(&s.id, metrics.clone()).expect("update");
        assert!(changed, "first set must report a change");

        let after = store.load_sessions();
        let persisted = after.get(&s.id).expect("present").last_metrics.as_ref().expect("has metrics");
        assert_eq!(persisted.input_tokens, Some(50_000));
        assert_eq!(persisted.output_tokens, Some(10_000));
        assert_eq!(persisted.model.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn update_session_metrics_is_idempotent() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let s = make_session(Uuid::new_v4(), "x", td.path());
        store.save_session(&s).expect("save");

        let metrics = SessionMetricsEvent {
            session_id: s.id,
            model: None,
            context_used_pct: None,
            context_tokens_used: None,
            context_tokens_limit: None,
            input_tokens: Some(100),
            output_tokens: Some(50),
            observed_at: 1_700_000_100,
        };

        store.update_session_metrics(&s.id, metrics.clone()).expect("first");
        // Same payload, different observed_at — should be idempotent (same_payload_as ignores observed_at).
        let mut same_payload = metrics;
        same_payload.observed_at = 1_700_000_200;
        let changed = store.update_session_metrics(&s.id, same_payload).expect("second");
        assert!(!changed, "no-op write must report no change");
    }

    #[test]
    fn update_session_metrics_returns_not_found_for_unknown_id() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let id = SessionId::new();
        let metrics = SessionMetricsEvent {
            session_id: id,
            model: None,
            context_used_pct: None,
            context_tokens_used: None,
            context_tokens_limit: None,
            input_tokens: None,
            output_tokens: None,
            observed_at: 0,
        };
        let err = store.update_session_metrics(&id, metrics).expect_err("must fail");
        assert!(matches!(err, Error::NotFound(_)));
    }

    #[test]
    fn update_session_metrics_errors_on_id_mismatch() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let id_a = SessionId::new();
        let id_b = SessionId::new();
        let metrics = SessionMetricsEvent {
            session_id: id_b,
            model: None,
            context_used_pct: None,
            context_tokens_used: None,
            context_tokens_limit: None,
            input_tokens: None,
            output_tokens: None,
            observed_at: 0,
        };
        let err = store.update_session_metrics(&id_a, metrics).expect_err("must fail on mismatch");
        assert!(matches!(err, Error::Internal(_)));
        assert!(err.to_string().contains("does not match"));
    }

    // ----- Copilot composed_command migration --------------------------

    #[test]
    fn load_sessions_strips_legacy_copilot_interactive_flag() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let id = SessionId(Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("uuid"));

        // Hand-write sessions.json with the legacy invocation a pre-fix build would have persisted.
        let raw = serde_json::json!({
            id.to_string(): {
                "id": id,
                "tool": "copilot",
                "worktreePath": td.path(),
                "worktreeName": "feat-x",
                "label": "feat-x",
                "composedCommand": "echo hi && copilot --interactive \"context block\"",
                "status": "exited",
                "createdAt": 1_700_000_000_u64,
                "tabIndex": 0,
                "tempFiles": []
            }
        });
        fs::write(store.sessions_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");

        let loaded = store.load_sessions();
        let session = loaded.get(&id).expect("present");
        assert_eq!(session.composed_command, "echo hi && copilot");
    }

    #[test]
    fn load_sessions_leaves_bare_copilot_alone() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let id = SessionId(Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("uuid"));
        let raw = serde_json::json!({
            id.to_string(): {
                "id": id,
                "tool": "copilot",
                "worktreePath": td.path(),
                "worktreeName": "feat-y",
                "label": "feat-y",
                "composedCommand": "copilot",
                "status": "exited",
                "createdAt": 1_700_000_000_u64,
                "tabIndex": 0,
                "tempFiles": []
            }
        });
        fs::write(store.sessions_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let loaded = store.load_sessions();
        assert_eq!(loaded.get(&id).expect("present").composed_command, "copilot");
    }

    #[test]
    fn load_sessions_does_not_touch_claude_sessions() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let id = SessionId(Uuid::parse_str("33333333-3333-3333-3333-333333333333").expect("uuid"));
        let raw = serde_json::json!({
            id.to_string(): {
                "id": id,
                "tool": "claude",
                "worktreePath": td.path(),
                "worktreeName": "feat-z",
                "label": "feat-z",
                "composedCommand": "claude --system-prompt /tmp/x.md",
                "status": "exited",
                "createdAt": 1_700_000_000_u64,
                "tabIndex": 0,
                "tempFiles": []
            }
        });
        fs::write(store.sessions_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let loaded = store.load_sessions();
        assert_eq!(loaded.get(&id).expect("present").composed_command, "claude --system-prompt /tmp/x.md");
    }

    // ----- Atomic write durability --------------------------------------

    #[test]
    fn atomic_write_leaves_old_file_intact_on_persist_failure() {
        // Simulate a "persist failure" by trying to persist into a path whose parent directory is a *file*, not a dir. This is the closest portable
        // approximation of a cross-filesystem rename failure we can produce without root.
        let td = TempDir::new().expect("td");
        let target = td.path().join("config.json");
        fs::write(&target, b"OLD").expect("seed");

        // Build a doomed write into a path nested under a file.
        let doomed = td.path().join("config.json").join("inner.json");
        let result = write_atomic(&doomed, &serde_json::json!({"x": 1}));
        assert!(result.is_err(), "write to invalid parent must fail");

        // Original file is untouched.
        let still = fs::read(&target).expect("read");
        assert_eq!(still, b"OLD");
    }

    #[test]
    fn atomic_write_replaces_target() {
        let td = TempDir::new().expect("td");
        let target = td.path().join("foo.json");
        fs::write(&target, b"OLD").expect("seed");
        write_atomic(&target, &serde_json::json!({"hello": "world"})).expect("ok");
        let raw = fs::read_to_string(&target).expect("read");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("json");
        assert_eq!(parsed, serde_json::json!({"hello": "world"}));
    }

    // ----- active_session_id (Phase 7) ---------------------------------

    #[test]
    fn save_config_sets_and_clears_active_session_id() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let id = SessionId::new();

        // Set.
        let after_set = store
            .save_config(PartialAppConfig {
                active_session_id: Some(Some(id)),
                ..Default::default()
            })
            .expect("set");
        assert_eq!(after_set.active_session_id, Some(id));

        // Absent in patch → preserved.
        let after_noop = store
            .save_config(PartialAppConfig {
                worktree_prep_commands: Some(vec!["echo hi".to_owned()]),
                ..Default::default()
            })
            .expect("noop");
        assert_eq!(after_noop.active_session_id, Some(id));

        // Clear via Some(None).
        let after_clear = store
            .save_config(PartialAppConfig {
                active_session_id: Some(None),
                ..Default::default()
            })
            .expect("clear");
        assert_eq!(after_clear.active_session_id, None);
    }

    #[test]
    fn load_config_migrates_v1_to_current() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let raw = serde_json::json!({
            "configVersion": 1,
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": []
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let cfg = store.load_config();
        assert_eq!(cfg.config_version, CONFIG_VERSION_CURRENT);
        assert_eq!(cfg.active_session_id, None);
        assert_eq!(cfg.workspace_root, None);
    }

    // ----- workspace_root (v3, Roadmap §1) ----------------------------

    #[test]
    fn save_config_sets_and_clears_workspace_root() {
        let td = TempDir::new().expect("td");
        let store_dir = td.path().join("store");
        let ws_dir = td.path().join("workspace");
        fs::create_dir_all(&ws_dir).expect("mk ws");
        let store = ConfigStore::open(&store_dir).expect("open");

        // Set.
        let after_set = store
            .save_config(PartialAppConfig {
                workspace_root: Some(Some(ws_dir.clone())),
                ..Default::default()
            })
            .expect("set");
        assert_eq!(after_set.workspace_root, Some(canon(&ws_dir)));

        // Absent → preserved.
        let after_noop = store
            .save_config(PartialAppConfig {
                worktree_prep_commands: Some(vec!["echo hi".to_owned()]),
                ..Default::default()
            })
            .expect("noop");
        assert_eq!(after_noop.workspace_root, Some(canon(&ws_dir)));

        // Clear via Some(None).
        let after_clear = store
            .save_config(PartialAppConfig {
                workspace_root: Some(None),
                ..Default::default()
            })
            .expect("clear");
        assert_eq!(after_clear.workspace_root, None);
    }

    #[test]
    fn save_config_rejects_relative_workspace_root() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let err = store
            .save_config(PartialAppConfig {
                workspace_root: Some(Some(PathBuf::from("relative/path"))),
                ..Default::default()
            })
            .expect_err("rejected");
        assert!(matches!(err, Error::InvalidPath(_)), "got {err:?}");
    }

    #[test]
    fn save_config_rejects_workspace_root_that_is_not_a_dir() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let file = td.path().join("not-a-dir");
        fs::write(&file, b"x").expect("seed");
        let err = store
            .save_config(PartialAppConfig {
                workspace_root: Some(Some(file)),
                ..Default::default()
            })
            .expect_err("rejected");
        assert!(matches!(err, Error::InvalidPath(_)), "got {err:?}");
    }

    #[test]
    fn load_config_v2_with_single_worktree_root_promotes_to_workspace() {
        let td = TempDir::new().expect("td");
        let store_dir = td.path().join("store");
        let repo = td.path().join("repo");
        fs::create_dir_all(&repo).expect("mk repo");
        let store = ConfigStore::open(&store_dir).expect("open");
        let raw = serde_json::json!({
            "configVersion": 2,
            "worktreeRoots": [repo.to_string_lossy()],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "activeSessionId": null
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let cfg = store.load_config();
        assert_eq!(cfg.config_version, CONFIG_VERSION_CURRENT);
        assert_eq!(cfg.workspace_root, Some(canon(&repo)));
    }

    #[test]
    fn load_config_v2_with_multiple_roots_does_not_promote_workspace() {
        let td = TempDir::new().expect("td");
        let store_dir = td.path().join("store");
        let r1 = td.path().join("r1");
        let r2 = td.path().join("r2");
        fs::create_dir_all(&r1).expect("mk r1");
        fs::create_dir_all(&r2).expect("mk r2");
        let store = ConfigStore::open(&store_dir).expect("open");
        let raw = serde_json::json!({
            "configVersion": 2,
            "worktreeRoots": [r1.to_string_lossy(), r2.to_string_lossy()],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "activeSessionId": null
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let cfg = store.load_config();
        assert_eq!(cfg.workspace_root, None);
    }

    // ----- v3 → v4 migration: built-in custom-process seeding ----------

    #[test]
    fn load_config_migrates_v3_with_user_edited_shell_preserved() {
        // The v3 user has already customised the built-in `shell` (renamed it, swapped to fish, disabled it). The v3→v4 seed pass must NOT clobber
        // the user's edits — only append the missing built-ins (open-folder, vscode). This covers the `load_config` integration path, complementing
        // the `seeding_is_additive_and_does_not_overwrite_user_edits` unit test on the seeding helper itself.
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let raw = serde_json::json!({
            "configVersion": 3,
            "workspaceRoot": null,
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "activeSessionId": null,
            "customProcesses": [
                {
                    "id": BUILTIN_DEF_ID_SHELL,
                    "name": "My Fish",
                    "kind": "terminal",
                    "command": "fish -i",
                    "enabled": false
                }
            ]
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let cfg = store.load_config();
        assert_eq!(cfg.config_version, CONFIG_VERSION_CURRENT);
        // Shell preserved verbatim.
        let shell = cfg
            .custom_processes
            .iter()
            .find(|d| d.id.as_str() == BUILTIN_DEF_ID_SHELL)
            .expect("shell def must remain");
        assert_eq!(shell.name, "My Fish");
        assert_eq!(shell.command, "fish -i");
        assert!(!shell.enabled, "user's enabled=false must survive seeding");
        // Other built-ins appended.
        let ids: Vec<_> = cfg.custom_processes.iter().map(|d| d.id.as_str().to_owned()).collect();
        assert!(
            ids.contains(&BUILTIN_DEF_ID_OPEN_FOLDER.to_owned()),
            "open-folder must be appended by v3→v4 migration"
        );
        assert!(
            ids.contains(&BUILTIN_DEF_ID_VSCODE.to_owned()),
            "vscode must be appended by v3→v4 migration"
        );
    }

    #[test]
    fn load_config_migrates_v3_and_seeds_default_custom_processes() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let raw = serde_json::json!({
            "configVersion": 3,
            "workspaceRoot": null,
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "activeSessionId": null
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let cfg = store.load_config();
        assert_eq!(cfg.config_version, CONFIG_VERSION_CURRENT);
        let ids: Vec<_> = cfg.custom_processes.iter().map(|d| d.id.as_str().to_owned()).collect();
        assert_eq!(ids, vec!["shell", "open-folder", "vscode"]);
        assert!(cfg.last_open_sub_sessions.is_empty());
    }

    #[test]
    fn seeding_is_additive_and_does_not_overwrite_user_edits() {
        let user_edited = CustomProcessDef {
            id: CustomProcessDefId::new(BUILTIN_DEF_ID_SHELL),
            name: "My Custom Shell".to_owned(),
            kind: CustomProcessKind::Terminal,
            command: "fish -i".to_owned(),
            enabled: false,
            icon: None,
            icon_data_uri: None,
        };
        let mut defs = vec![user_edited.clone()];
        seed_default_custom_processes(&mut defs);
        // Shell entry preserved verbatim; the other two built-ins appended.
        assert_eq!(defs.len(), 3);
        assert_eq!(defs[0], user_edited);
        assert_eq!(defs[1].id.as_str(), BUILTIN_DEF_ID_OPEN_FOLDER);
        assert_eq!(defs[2].id.as_str(), BUILTIN_DEF_ID_VSCODE);
    }

    #[test]
    fn seeding_is_idempotent_when_called_repeatedly() {
        let mut defs = Vec::new();
        seed_default_custom_processes(&mut defs);
        let after_first = defs.clone();
        seed_default_custom_processes(&mut defs);
        assert_eq!(defs, after_first);
    }

    #[test]
    fn load_config_v4_does_not_reseed_after_user_deleted_a_builtin() {
        // User on v4 already has only `shell` (deleted vscode + open-folder intentionally); the migration must not run again, so the deletes stick.
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let raw = serde_json::json!({
            "configVersion": 4,
            "workspaceRoot": null,
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "activeSessionId": null,
            "customProcesses": [
                {
                    "id": "shell",
                    "name": "Shell",
                    "kind": "terminal",
                    "command": "sh -i",
                    "enabled": true
                }
            ],
            "lastOpenSubSessions": []
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&raw).expect("ser")).expect("write");
        let cfg = store.load_config();
        let ids: Vec<_> = cfg.custom_processes.iter().map(|d| d.id.as_str().to_owned()).collect();
        assert_eq!(ids, vec!["shell"], "v4+ must not re-run the seed pass");
    }

    // ----- v4 → v5 migration: missing-worktree filter (PR #65 review-9) ----

    /// Helper: serialise a session as the v4 on-disk shape (no `parentWorktreeTabId` / `childIndex`, which are v5 additions). Mirrors what
    /// `load_sessions_strips_legacy_copilot_interactive_flag` and the other hand-written-JSON tests do — we deliberately bypass the typed `Session`
    /// struct here so the test exercises the actual on-disk migration, not a round-trip through the current schema.
    fn v4_session_json(id: SessionId, worktree: &Path) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "tool": "claude",
            "worktreePath": worktree,
            "worktreeName": "wt",
            "label": id.to_string(),
            "composedCommand": "claude",
            "status": "exited",
            "createdAt": 1_700_000_000_u64,
            "tabIndex": 0,
            "tempFiles": []
        })
    }

    #[test]
    fn migrate_v4_to_v5_skips_sessions_with_missing_worktree() {
        // PR #65 review-9: a v4 session whose worktree directory has been deleted while the app was closed must NOT produce a `WorktreeTab` in v5 —
        // the previous implementation fell back to `s.worktree_path.clone()` on canonicalize failure, persisting a zombie tab whose `path` resolved
        // to nothing.
        let td = TempDir::new().expect("td");
        let valid_wt = TempDir::new().expect("valid wt");
        let missing_wt = td.path().join("nonexistent-worktree");
        // Sanity: the missing path really doesn't exist.
        assert!(!missing_wt.exists());

        let store = ConfigStore::open(td.path()).expect("open");
        let valid_sid = SessionId(Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").expect("uuid"));
        let missing_sid = SessionId(Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").expect("uuid"));

        let sessions_raw = serde_json::json!({
            valid_sid.to_string(): v4_session_json(valid_sid, valid_wt.path()),
            missing_sid.to_string(): v4_session_json(missing_sid, &missing_wt),
        });
        fs::write(store.sessions_path(), serde_json::to_vec_pretty(&sessions_raw).expect("ser")).expect("write");

        let cfg_raw = serde_json::json!({
            "configVersion": 4,
            "workspaceRoot": null,
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [valid_sid.to_string(), missing_sid.to_string()],
            "tabOrder": [valid_sid.to_string(), missing_sid.to_string()],
            "activeSessionId": null,
            "customProcesses": [],
            "lastOpenSubSessions": []
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&cfg_raw).expect("ser")).expect("write");

        let cfg = store.load_config();
        assert_eq!(cfg.config_version, CONFIG_VERSION_CURRENT);
        assert_eq!(cfg.worktree_tabs.len(), 1, "only the valid-worktree session should produce a tab");
        assert_eq!(
            cfg.worktree_tabs[0].path,
            canon(valid_wt.path()),
            "the surviving tab must point at the canonical valid worktree path",
        );
        assert_eq!(
            cfg.worktree_tab_order,
            vec![cfg.worktree_tabs[0].id],
            "tab_order must contain only the surviving tab id",
        );
        assert_eq!(cfg.active_worktree_tab_id, None, "no active session was set, so active tab stays None");
    }

    #[test]
    fn migrate_v4_to_v5_active_session_pointing_at_missing_worktree_leaves_active_tab_none() {
        // Edge case: `active_session_id` references the session whose worktree was deleted. The migration must NOT synthesise a zombie active tab —
        // `active_worktree_tab_id` must remain `None` so restore-on-launch lands on no tab rather than on a phantom one.
        let td = TempDir::new().expect("td");
        let valid_wt = TempDir::new().expect("valid wt");
        let missing_wt = td.path().join("ghost-worktree");

        let store = ConfigStore::open(td.path()).expect("open");
        let valid_sid = SessionId(Uuid::parse_str("cccccccc-cccc-cccc-cccc-cccccccccccc").expect("uuid"));
        let missing_sid = SessionId(Uuid::parse_str("dddddddd-dddd-dddd-dddd-dddddddddddd").expect("uuid"));

        let sessions_raw = serde_json::json!({
            valid_sid.to_string(): v4_session_json(valid_sid, valid_wt.path()),
            missing_sid.to_string(): v4_session_json(missing_sid, &missing_wt),
        });
        fs::write(store.sessions_path(), serde_json::to_vec_pretty(&sessions_raw).expect("ser")).expect("write");

        let cfg_raw = serde_json::json!({
            "configVersion": 4,
            "workspaceRoot": null,
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [valid_sid.to_string(), missing_sid.to_string()],
            "tabOrder": [valid_sid.to_string(), missing_sid.to_string()],
            "activeSessionId": missing_sid,
            "customProcesses": [],
            "lastOpenSubSessions": []
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&cfg_raw).expect("ser")).expect("write");

        let cfg = store.load_config();
        assert_eq!(cfg.worktree_tabs.len(), 1, "only the valid-worktree session should produce a tab");
        assert_eq!(
            cfg.active_worktree_tab_id, None,
            "active_session_id pointed at a missing-worktree session, so active_worktree_tab_id stays None",
        );
        for tab in &cfg.worktree_tabs {
            assert_eq!(
                tab.active_child_id, None,
                "no surviving tab should have active_child_id pointing at the missing session",
            );
        }
    }

    #[test]
    fn migrate_v4_to_v5_active_session_with_valid_worktree_lands_on_correct_tab() {
        // Sanity: when `active_session_id` references a session whose worktree DOES exist, the migration must derive `active_worktree_tab_id` and
        // set the matching tab's `active_child_id` to that session — preserving the user's last-focused terminal.
        let td = TempDir::new().expect("td");
        let valid_wt = TempDir::new().expect("valid wt");
        let store = ConfigStore::open(td.path()).expect("open");
        let valid_sid = SessionId(Uuid::parse_str("eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee").expect("uuid"));

        let sessions_raw = serde_json::json!({
            valid_sid.to_string(): v4_session_json(valid_sid, valid_wt.path()),
        });
        fs::write(store.sessions_path(), serde_json::to_vec_pretty(&sessions_raw).expect("ser")).expect("write");

        let cfg_raw = serde_json::json!({
            "configVersion": 4,
            "workspaceRoot": null,
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [valid_sid.to_string()],
            "tabOrder": [valid_sid.to_string()],
            "activeSessionId": valid_sid,
            "customProcesses": [],
            "lastOpenSubSessions": []
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&cfg_raw).expect("ser")).expect("write");

        let cfg = store.load_config();
        assert_eq!(cfg.worktree_tabs.len(), 1);
        let tab = &cfg.worktree_tabs[0];
        assert_eq!(cfg.active_worktree_tab_id, Some(tab.id));
        assert_eq!(tab.active_child_id, Some(crate::types::ChildId::Session(valid_sid)));
    }

    // ----- v4 → v5 migration: icon assignment (Issue #45) --------------

    /// The v4→v5 synthesise loop must populate every freshly-created tab's `icon_id` with the deterministic least-used pick — it can't lean on the
    /// v6→v7 backfill because that pass only runs on configs originally written under v6, and a v4-quarantined-then-loaded config completes its
    /// migration in one shot. Mirrors the contract for `worktree_tab_open_impl`.
    #[test]
    fn migrate_v4_to_v5_assigns_icons_to_synthesised_tabs() {
        use crate::worktree_icon::WORKTREE_ICON_COUNT;

        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        // Three valid worktrees → three synthesised tabs, expecting icons 1, 2, 3.
        let wt_a = TempDir::new().expect("wt a");
        let wt_b = TempDir::new().expect("wt b");
        let wt_c = TempDir::new().expect("wt c");
        let sid_a = SessionId(Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").expect("uuid"));
        let sid_b = SessionId(Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").expect("uuid"));
        let sid_c = SessionId(Uuid::parse_str("cccccccc-cccc-cccc-cccc-cccccccccccc").expect("uuid"));

        let sessions_raw = serde_json::json!({
            sid_a.to_string(): v4_session_json(sid_a, wt_a.path()),
            sid_b.to_string(): v4_session_json(sid_b, wt_b.path()),
            sid_c.to_string(): v4_session_json(sid_c, wt_c.path()),
        });
        fs::write(store.sessions_path(), serde_json::to_vec_pretty(&sessions_raw).expect("ser")).expect("write");

        let cfg_raw = serde_json::json!({
            "configVersion": 4,
            "workspaceRoot": null,
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [sid_a.to_string(), sid_b.to_string(), sid_c.to_string()],
            "tabOrder": [sid_a.to_string(), sid_b.to_string(), sid_c.to_string()],
            "activeSessionId": null,
            "customProcesses": [],
            "lastOpenSubSessions": []
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&cfg_raw).expect("ser")).expect("write");

        let cfg = store.load_config();
        assert_eq!(cfg.worktree_tabs.len(), 3);
        for tab in &cfg.worktree_tabs {
            assert!(
                (1..=WORKTREE_ICON_COUNT).contains(&tab.icon_id),
                "every synthesised tab must get a valid iconId, got {} on tab {}",
                tab.icon_id,
                tab.id
            );
        }
        // Order in `worktree_tab_order` matches the synthesise order — those are the first three icons.
        let icons_in_order: Vec<u32> = cfg
            .worktree_tab_order
            .iter()
            .filter_map(|tid| cfg.worktree_tabs.iter().find(|t| t.id == *tid).map(|t| t.icon_id))
            .collect();
        assert_eq!(icons_in_order, vec![1, 2, 3], "synthesised tabs should walk icons 1..=3 in tab_order");
    }

    // ----- v6 → v7 migration: icon backfill (Issue #45) ----------------

    /// Pre-v7 JSON has no `iconId` on worktree tabs; the loader's `serde(default)` fills it with 0 and the v6→v7 migration must replace each 0 with
    /// a deterministic least-used pick walked in `worktree_tab_order`. Tabs that already carry a non-zero `iconId` (e.g. from a forward-compatible
    /// frontend) must be left untouched so the migration is idempotent.
    #[test]
    fn migrate_v6_to_v7_backfills_zero_icon_ids_in_tab_order() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let tab_a = WorktreeTabId::new();
        let tab_b = WorktreeTabId::new();
        let tab_c = WorktreeTabId::new();
        // v6 JSON: explicitly omit `iconId` on A and B (defaults to 0 via serde) and pin C to 5 so we can assert it's preserved.
        let cfg_raw = serde_json::json!({
            "configVersion": 6,
            "workspaceRoot": null,
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "activeSessionId": null,
            "customProcesses": [],
            "lastOpenSubSessions": [],
            "worktreeTabs": [
                { "id": tab_a, "path": "/repo/a", "name": "a", "label": "a", "tabIndex": 0 },
                { "id": tab_b, "path": "/repo/b", "name": "b", "label": "b", "tabIndex": 1 },
                { "id": tab_c, "path": "/repo/c", "name": "c", "label": "c", "tabIndex": 2, "iconId": 5 }
            ],
            "worktreeTabOrder": [tab_a, tab_b, tab_c],
            "activeWorktreeTabId": null
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&cfg_raw).expect("ser")).expect("write");

        let cfg = store.load_config();
        assert_eq!(cfg.config_version, CONFIG_VERSION_CURRENT);
        // Sort tabs to a deterministic order matching the order list — `load_config` doesn't reorder `worktree_tabs` itself.
        let by_id: std::collections::HashMap<_, _> = cfg.worktree_tabs.iter().map(|t| (t.id, t.icon_id)).collect();
        // C had iconId = 5 already → must be preserved.
        assert_eq!(by_id[&tab_c], 5, "pre-set iconId must be preserved across the backfill");
        // A is first in tab_order with no icon yet; existing assignments are [5] (from C), so the lowest-count icons are 1..=4 and 6..=16. Pick 1.
        assert_eq!(by_id[&tab_a], 1, "first unassigned tab in order must get icon 1");
        // After A → 1, existing assignments are [5, 1]; B picks 2 (lowest at min count = 0).
        assert_eq!(by_id[&tab_b], 2, "second unassigned tab in order must get icon 2");
    }

    /// Drift defense: a tab present in `worktree_tabs` but missing from `worktree_tab_order` (well-formed configs shouldn't produce this, but the
    /// loader is intentionally lenient) must still get an iconId backfilled in the second pass — otherwise we'd persist 0 on disk forever.
    #[test]
    fn migrate_v6_to_v7_backfills_tabs_missing_from_order_list() {
        use crate::worktree_icon::WORKTREE_ICON_COUNT;

        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let tab_orphan = WorktreeTabId::new();
        let cfg_raw = serde_json::json!({
            "configVersion": 6,
            "workspaceRoot": null,
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "activeSessionId": null,
            "customProcesses": [],
            "lastOpenSubSessions": [],
            "worktreeTabs": [
                { "id": tab_orphan, "path": "/repo/orphan", "name": "orphan", "label": "orphan", "tabIndex": 0 }
            ],
            "worktreeTabOrder": [],
            "activeWorktreeTabId": null
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&cfg_raw).expect("ser")).expect("write");

        let cfg = store.load_config();
        let orphan = cfg.worktree_tabs.iter().find(|t| t.id == tab_orphan).expect("orphan tab present");
        assert!(
            (1..=WORKTREE_ICON_COUNT).contains(&orphan.icon_id),
            "tab missing from tab_order must still get an iconId backfilled (got {})",
            orphan.icon_id
        );
    }

    /// Invariant defense: even when the persisted config is *already* stamped as v7, a tab carrying `icon_id == 0` (manual edit, partial write, a
    /// frontend that round-tripped without the field) must be backfilled on load — otherwise the documented post-load invariant
    /// ("every WorktreeTab has icon_id in 1..=N") silently breaks. The migration guard runs whenever *any* tab has a zero, regardless of
    /// `config_version`. A correctly-stamped tab on the same record must be left untouched.
    #[test]
    fn loading_v7_config_with_zero_icon_id_self_heals_via_migration() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let healthy = WorktreeTabId::new();
        let corrupted = WorktreeTabId::new();
        let cfg_raw = serde_json::json!({
            "configVersion": 7,
            "workspaceRoot": null,
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "activeSessionId": null,
            "customProcesses": [],
            "lastOpenSubSessions": [],
            "worktreeTabs": [
                { "id": healthy,   "path": "/repo/healthy",   "name": "h", "label": "h", "tabIndex": 0, "iconId": 9 },
                { "id": corrupted, "path": "/repo/corrupted", "name": "c", "label": "c", "tabIndex": 1, "iconId": 0 }
            ],
            "worktreeTabOrder": [healthy, corrupted],
            "activeWorktreeTabId": null
        });
        fs::write(store.config_path(), serde_json::to_vec_pretty(&cfg_raw).expect("ser")).expect("write");

        let cfg = store.load_config();
        // Stamped version stays at current — backfill alone shouldn't bump it (it was already current).
        assert_eq!(cfg.config_version, CONFIG_VERSION_CURRENT);
        let by_id: std::collections::HashMap<_, _> = cfg.worktree_tabs.iter().map(|t| (t.id, t.icon_id)).collect();
        // Healthy tab keeps its existing assignment.
        assert_eq!(by_id[&healthy], 9, "tab with a valid iconId must not be rewritten by the self-heal pass");
        // Corrupted tab gets a deterministic least-used pick. Existing assignments at the time it's picked are [9] (healthy) — so the lowest
        // count icon is 1 (and 2..=8, 10..=16); the lowest-numbered wins → 1.
        assert_eq!(
            by_id[&corrupted], 1,
            "corrupted iconId == 0 must be self-healed to the deterministic least-used pick (1)"
        );
    }

    // ----- save_config: customProcesses validation --------------------

    #[test]
    fn save_config_rejects_custom_process_with_empty_id() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let err = store
            .save_config(PartialAppConfig {
                custom_processes: Some(vec![CustomProcessDef {
                    id: CustomProcessDefId::new(""),
                    name: "x".to_owned(),
                    kind: CustomProcessKind::Terminal,
                    command: "echo".to_owned(),
                    enabled: true,
                    icon: None,
                    icon_data_uri: None,
                }]),
                ..Default::default()
            })
            .expect_err("rejected");
        assert!(matches!(err, Error::InvalidCustomProcessDef(_)), "got {err:?}");
    }

    #[test]
    fn save_config_rejects_custom_process_with_invalid_id_chars() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let err = store
            .save_config(PartialAppConfig {
                custom_processes: Some(vec![CustomProcessDef {
                    id: CustomProcessDefId::new("has space"),
                    name: "x".to_owned(),
                    kind: CustomProcessKind::Terminal,
                    command: "echo".to_owned(),
                    enabled: true,
                    icon: None,
                    icon_data_uri: None,
                }]),
                ..Default::default()
            })
            .expect_err("rejected");
        assert!(matches!(err, Error::InvalidCustomProcessDef(_)), "got {err:?}");
    }

    #[test]
    fn save_config_rejects_custom_process_with_empty_command() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let err = store
            .save_config(PartialAppConfig {
                custom_processes: Some(vec![CustomProcessDef {
                    id: CustomProcessDefId::new("ok"),
                    name: "ok".to_owned(),
                    kind: CustomProcessKind::Terminal,
                    command: "   ".to_owned(),
                    enabled: true,
                    icon: None,
                    icon_data_uri: None,
                }]),
                ..Default::default()
            })
            .expect_err("rejected");
        assert!(matches!(err, Error::InvalidCustomProcessDef(_)), "got {err:?}");
    }

    #[test]
    fn save_config_rejects_duplicate_custom_process_ids() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let dup = CustomProcessDef {
            id: CustomProcessDefId::new("dup"),
            name: "x".to_owned(),
            kind: CustomProcessKind::Terminal,
            command: "echo".to_owned(),
            enabled: true,
            icon: None,
            icon_data_uri: None,
        };
        let err = store
            .save_config(PartialAppConfig {
                custom_processes: Some(vec![dup.clone(), dup]),
                ..Default::default()
            })
            .expect_err("rejected");
        assert!(matches!(err, Error::InvalidCustomProcessDef(_)), "got {err:?}");
    }

    #[test]
    fn save_config_round_trips_valid_custom_processes_and_records() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let tab_id = WorktreeTabId(Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("uuid"));
        let sub = SubSessionRecord {
            id: SubSessionId(Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("uuid")),
            parent_session_id: None,
            parent_worktree_tab_id: Some(tab_id),
            def_id: CustomProcessDefId::new("shell"),
            kind: CustomProcessKind::Terminal,
            label: "Shell".to_owned(),
            composed_command: "sh -i".to_owned(),
        };
        let def = CustomProcessDef {
            id: CustomProcessDefId::new("shell"),
            name: "Shell".to_owned(),
            kind: CustomProcessKind::Terminal,
            command: "sh -i".to_owned(),
            enabled: true,
            icon: None,
            icon_data_uri: None,
        };
        // A matching worktree tab must exist for sanitize to keep the sub-session record.
        let tab = WorktreeTab {
            id: tab_id,
            path: td.path().to_owned(),
            name: "test".to_owned(),
            branch: None,
            label: "test".to_owned(),
            tab_index: 0,
            active_child_id: None,
            icon_id: 1,
        };
        let after = store
            .save_config(PartialAppConfig {
                custom_processes: Some(vec![def.clone()]),
                last_open_sub_sessions: Some(vec![sub.clone()]),
                worktree_tabs: Some(vec![tab]),
                worktree_tab_order: Some(vec![tab_id]),
                ..Default::default()
            })
            .expect("ok");
        assert_eq!(after.custom_processes, vec![def]);
        assert_eq!(after.last_open_sub_sessions, vec![sub]);
    }

    // ----- command_on_path probe ---------------------------------------

    #[test]
    #[serial_test::serial(env_path)]
    fn command_on_path_finds_seeded_executable() {
        let td = TempDir::new().expect("td");
        let dir = td.path();
        let stem = "arborist-probe-bin";
        let exe_name = if cfg!(target_os = "windows") {
            format!("{stem}.exe")
        } else {
            stem.to_owned()
        };
        let path = dir.join(&exe_name);
        fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).expect("meta").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).expect("chmod");
        }

        let original_path = std::env::var_os("PATH");
        let mut new_path = std::ffi::OsString::from(dir);
        if let Some(p) = original_path.clone() {
            #[cfg(unix)]
            new_path.push(":");
            #[cfg(windows)]
            new_path.push(";");
            new_path.push(p);
        }
        // Mutating env in tests is racy; this test is `#[serial]` to compensate.
        std::env::set_var("PATH", &new_path);
        let result = command_on_path(stem);
        match original_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        assert!(result, "{stem} should be discovered on the temporary PATH");
    }

    #[test]
    fn command_on_path_returns_false_for_unknown_binary() {
        assert!(!command_on_path("arborist-definitely-not-on-path-zzz-12345"));
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(env_path)]
    fn command_on_path_rejects_non_executable_file_on_unix() {
        let td = TempDir::new().expect("td");
        let dir = td.path();
        let stem = "arborist-non-exec";
        let path = dir.join(stem);
        fs::write(&path, b"not really an executable\n").expect("write");
        // Deliberately leave default 0o644-ish perms (no exec bit).

        let original_path = std::env::var_os("PATH");
        let mut new_path = std::ffi::OsString::from(dir);
        if let Some(p) = original_path.clone() {
            new_path.push(":");
            new_path.push(p);
        }
        std::env::set_var("PATH", &new_path);
        let result = command_on_path(stem);
        match original_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        assert!(!result, "non-executable file with name {stem} must not be reported as on PATH");
    }

    // ----- Fresh-install + load-time hardening -------------------------

    #[test]
    fn load_config_seeds_defaults_when_file_missing() {
        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");
        let cfg = store.load_config();
        let ids: Vec<&str> = cfg.custom_processes.iter().map(|d| d.id.as_str()).collect();
        assert!(
            ids.contains(&BUILTIN_DEF_ID_SHELL),
            "fresh install must include {BUILTIN_DEF_ID_SHELL}, got {ids:?}"
        );
        assert!(
            ids.contains(&BUILTIN_DEF_ID_OPEN_FOLDER),
            "fresh install must include {BUILTIN_DEF_ID_OPEN_FOLDER}, got {ids:?}"
        );
        assert!(
            ids.contains(&BUILTIN_DEF_ID_VSCODE),
            "fresh install must include {BUILTIN_DEF_ID_VSCODE}, got {ids:?}"
        );
    }

    #[test]
    fn load_config_clamps_out_of_range_sidebar_width_px() {
        // A user hand-edits config.json to set sidebarWidthPx outside [180, 480]. Load should clamp into range rather than preserve the bad value
        // (and rather than waiting for the next frontend resize to trigger the patch-path clamp in merge_partial).
        let td = TempDir::new().expect("td");
        let path = td.path().join("config.json");
        let raw = serde_json::json!({
            "configVersion": CONFIG_VERSION_CURRENT,
            "worktreeRoots": [],
            "lastOpenSessions": [],
            "tabOrder": [],
            "sidebarWidthPx": 9000,
        });
        fs::write(&path, serde_json::to_string(&raw).expect("ser")).expect("write");
        let store = ConfigStore::open(td.path()).expect("open");
        let cfg = store.load_config();
        assert_eq!(cfg.sidebar_width_px, Some(crate::types::SIDEBAR_WIDTH_MAX_PX));

        // And the under-bound direction.
        let raw_low = serde_json::json!({
            "configVersion": CONFIG_VERSION_CURRENT,
            "worktreeRoots": [],
            "lastOpenSessions": [],
            "tabOrder": [],
            "sidebarWidthPx": 1,
        });
        fs::write(&path, serde_json::to_string(&raw_low).expect("ser")).expect("write");
        let cfg_low = store.load_config();
        assert_eq!(cfg_low.sidebar_width_px, Some(crate::types::SIDEBAR_WIDTH_MIN_PX));
    }

    #[test]
    fn merge_theme_preference() {
        use arborist_types::ThemeMode;

        let td = TempDir::new().expect("td");
        let store = ConfigStore::open(td.path()).expect("open");

        // Default is System.
        let cfg = store.load_config();
        assert_eq!(cfg.theme, ThemeMode::System);

        // Patch to Dark.
        let after = store
            .save_config(PartialAppConfig {
                theme: Some(ThemeMode::Dark),
                ..Default::default()
            })
            .expect("ok");
        assert_eq!(after.theme, ThemeMode::Dark);

        // Patch without theme field — must preserve Dark.
        let after2 = store
            .save_config(PartialAppConfig {
                sidebar_width_px: Some(200),
                ..Default::default()
            })
            .expect("ok");
        assert_eq!(after2.theme, ThemeMode::Dark);
    }

    #[test]
    fn load_config_seeds_defaults_when_file_unparseable() {
        let td = TempDir::new().expect("td");
        // Write garbage as config.json so parse fails and triggers quarantine-and-default. Defaults must still include built-ins.
        fs::write(td.path().join("config.json"), b"not json {{ ").expect("write");
        let store = ConfigStore::open(td.path()).expect("open");
        let cfg = store.load_config();
        assert!(!cfg.custom_processes.is_empty(), "defaults must seed");
    }

    #[test]
    fn load_config_quarantines_future_version() {
        let td = TempDir::new().expect("td");
        let path = td.path().join("config.json");
        let future = serde_json::json!({
            "configVersion": CONFIG_VERSION_CURRENT + 1,
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "futureUnknownField": "danger"
        });
        fs::write(&path, serde_json::to_string(&future).expect("ser")).expect("write");
        let store = ConfigStore::open(td.path()).expect("open");
        let cfg = store.load_config();
        assert_eq!(
            cfg.config_version, CONFIG_VERSION_CURRENT,
            "future-version config must be quarantined and replaced with defaults"
        );
        assert!(
            std::fs::read_dir(td.path())
                .expect("readdir")
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().starts_with("config.json.bad-")),
            "quarantine file must exist alongside",
        );
    }

    #[test]
    fn load_config_drops_invalid_persisted_custom_processes() {
        let td = TempDir::new().expect("td");
        let path = td.path().join("config.json");
        // Hand-craft a v4 config with one valid, one empty-command, one duplicate-id def. Sanitize on load should keep only the first valid one.
        let crafted = serde_json::json!({
            "configVersion": CONFIG_VERSION_CURRENT,
            "worktreeRoots": [],
            "prelaunchCommands": [],
            "worktreePrelaunchCommands": {},
            "lastOpenSessions": [],
            "tabOrder": [],
            "customProcesses": [
                { "id": "good", "name": "Good", "kind": "terminal", "command": "sh", "enabled": true },
                { "id": "bad", "name": "Bad", "kind": "terminal", "command": "   ", "enabled": true },
                { "id": "good", "name": "Dup", "kind": "terminal", "command": "sh", "enabled": true },
                { "id": "has space", "name": "Space", "kind": "terminal", "command": "sh", "enabled": true }
            ],
            "lastOpenSubSessions": []
        });
        fs::write(&path, serde_json::to_string(&crafted).expect("ser")).expect("write");
        let store = ConfigStore::open(td.path()).expect("open");
        let cfg = store.load_config();
        let ids: Vec<&str> = cfg.custom_processes.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["good"], "only the first valid def must survive");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    #[serial_test::serial(env_shell)]
    fn default_shell_command_falls_back_when_shell_env_is_suspicious() {
        let original = std::env::var_os("SHELL");
        // Suspicious: contains a metacharacter.
        std::env::set_var("SHELL", "/bin/sh; rm -rf /");
        let cmd = default_shell_command();
        match original {
            Some(v) => std::env::set_var("SHELL", v),
            None => std::env::remove_var("SHELL"),
        }
        assert_eq!(cmd, "sh -i", "metacharacter in $SHELL must trigger fallback");
    }

    // ----- from_layout --------------------------------------------------

    /// `from_layout` must round-trip a save/load on the per-(branch, workspace) settings path resolved from the layout, and the resulting store must
    /// expose its layout via [`ConfigStore::layout`] so callers (seed-on-first-launch, in-app workspace switch) can reach auxiliary paths like
    /// `lock_path()` and `legacy_config_path()`.
    #[test]
    fn from_layout_writes_under_layout_workspace_dir() {
        let app_data = TempDir::new().expect("app_data");
        let workspace = TempDir::new().expect("workspace");
        let workspace_canon = canon(workspace.path());

        // Branch build → settings live under `<app_data>/branches/feature-x/workspaces/<key>/config.json`.
        let root = crate::store_layout::StoreRoot::new(app_data.path().to_path_buf(), "feature-x".to_owned());
        let workspace_canon_typed = crate::store_layout::CanonicalPath::assume_canonical(workspace_canon.clone());
        let layout = root.for_workspace(&workspace_canon_typed);
        let expected_settings_path = layout.settings_path();
        let expected_workspace_dir = layout.workspace_dir();

        let store = ConfigStore::from_layout(layout.clone()).expect("from_layout");

        // The store's directory is exactly the layout's workspace dir.
        assert_eq!(store.dir(), expected_workspace_dir.as_path());
        // The retained layout points back at the same workspace.
        let retained = store.layout().expect("layout retained");
        assert_eq!(retained.workspace().as_path(), workspace_canon.as_path());
        assert_eq!(retained.settings_path(), expected_settings_path);

        // Round-trip: a save persists to the layout's settings path.
        let partial = PartialAppConfig {
            config_version: Some(CONFIG_VERSION_CURRENT),
            ..PartialAppConfig::default()
        };
        store.save_config(partial).expect("save");
        assert!(
            expected_settings_path.exists(),
            "save_config should have written {}",
            expected_settings_path.display(),
        );
        // And no file leaked into the legacy top-level path.
        assert!(
            !root.legacy_config_path().exists(),
            "from_layout must not write to the legacy top-level path",
        );
    }

    /// Two `from_layout` stores with identical (branch, workspace) inputs must resolve to the same directory — proves the workspace key is
    /// deterministic and that branch builds in the same workspace see one shared on-disk state.
    #[test]
    fn from_layout_is_deterministic_for_same_inputs() {
        let app_data = TempDir::new().expect("app_data");
        let workspace = TempDir::new().expect("workspace");
        let canonical = crate::store_layout::CanonicalPath::assume_canonical(canon(workspace.path()));

        let root_a = crate::store_layout::StoreRoot::new(app_data.path().to_path_buf(), "feature-x".to_owned());
        let root_b = root_a.clone();
        let store_a = ConfigStore::from_layout(root_a.for_workspace(&canonical)).expect("a");
        let store_b = ConfigStore::from_layout(root_b.for_workspace(&canonical)).expect("b");
        assert_eq!(store_a.dir(), store_b.dir());
    }

    /// Different workspaces under the same branch must produce distinct directories — proves isolation between sibling workspaces is structural, not
    /// best-effort.
    #[test]
    fn from_layout_isolates_distinct_workspaces() {
        let app_data = TempDir::new().expect("app_data");
        let ws_a = TempDir::new().expect("ws_a");
        let ws_b = TempDir::new().expect("ws_b");

        let root = crate::store_layout::StoreRoot::new(app_data.path().to_path_buf(), "feature-x".to_owned());
        let store_a =
            ConfigStore::from_layout(root.for_workspace(&crate::store_layout::CanonicalPath::assume_canonical(canon(ws_a.path())))).expect("a");
        let store_b =
            ConfigStore::from_layout(root.for_workspace(&crate::store_layout::CanonicalPath::assume_canonical(canon(ws_b.path())))).expect("b");
        assert_ne!(store_a.dir(), store_b.dir(), "distinct workspaces must isolate their stores",);
    }
}
