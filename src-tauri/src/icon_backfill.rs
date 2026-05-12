//! Resolve and cache icon data URIs for `CustomProcessDef`s and AI-launch commands.
//!
//! Strategy: at config-save time and at app startup, walk every command string that maps to a sidebar tab, run it through
//! [`crate::cmd_resolver::resolve_command_icon_path`], extract a PNG
//! data URI via [`crate::process_icon::IconCache::data_uri_for_path`], and store the result on the def itself (`icon_data_uri`). The frontend then
//! renders synchronously from the persisted config — no per-render IPC call into the OS shell APIs.
//!
//! Pure side-effect: mutates `cfg`. The caller decides whether to re-persist (the function returns whether anything actually changed).

use std::path::Path;

use crate::cmd_resolver::resolve_command_icon_path;
use crate::process_icon::IconCache;
use crate::types::AppConfig;

/// Walk every icon-bearing command in `cfg` and fill in any missing `icon_data_uri` fields. Existing values are left alone — the invalidation
/// contract lives in [`crate::config_store::merge_partial`], which clears the cache when a command changes. Returns `true` if any field was populated
/// (i.e. caller should re-persist).
///
/// `fallback_cwd` is used for relative-path resolution. Defs are templates (not bound to a worktree at save time), so the workspace root is the most
/// useful default; OS temp dir is an acceptable last resort.
pub fn backfill_icons(cfg: &mut AppConfig, cache: &IconCache, fallback_cwd: &Path) -> bool {
    let mut changed = false;

    for def in cfg.custom_processes.iter_mut() {
        if def.icon_data_uri.is_some() {
            continue;
        }
        if let Some(uri) = resolve_one(&def.command, fallback_cwd, cache) {
            def.icon_data_uri = Some(uri);
            changed = true;
        }
    }

    for builtin in crate::plugins::ai::BUILTIN_AI {
        let plugin_id = builtin.plugin.id();
        if cfg.ai_launch_commands.icon_data_uri_for_id(plugin_id).is_some() {
            continue;
        }
        let launch_command = cfg.ai_launch_commands.command_for_id(plugin_id);
        if let Some(uri) = resolve_ai_icon(builtin.plugin.default_program(), launch_command, fallback_cwd, cache) {
            cfg.ai_launch_commands.icon_data_uris.insert(plugin_id.to_owned(), Some(uri));
            changed = true;
        }
    }

    changed
}

fn resolve_one(command: &str, cwd: &Path, cache: &IconCache) -> Option<String> {
    let exe = resolve_command_icon_path(command, cwd)?;
    cache.data_uri_for_path(&exe)
}

/// Two-phase icon resolution for the built-in AI tabs.
///
/// Users can replace the launch command with anything — npm shims, in-house "agency" wrappers, custom dispatchers — and we don't want those wrapper
/// icons showing up where Claude's or Copilot's brand icon belongs. So we try the *canonical* CLI name first and only fall back to the user's actual
/// launch command if the default can't be resolved on this machine.
///
/// 1. Resolve the default name (`claude` / `copilot`) on PATH+PATHEXT. If that
///    succeeds and isn't an interpreter wrapper, use its icon.
/// 2. Otherwise, fall back to resolving the user-configured `launch_command`
///    (if non-empty).
/// 3. Otherwise, return `None` — the frontend falls back to the bundled
///    `ToolIcon` SVG glyph.
fn resolve_ai_icon(default_name: &str, launch_command: &str, cwd: &Path, cache: &IconCache) -> Option<String> {
    resolve_ai_icon_with(default_name, launch_command, cwd, cache, resolve_one)
}

/// Test seam for [`resolve_ai_icon`]: the resolver is injected so unit tests can exercise the default-first ordering without touching the global
/// `PATH` / `PATHEXT` environment (which would race with parallel tests in the same process).
fn resolve_ai_icon_with<F>(default_name: &str, launch_command: &str, cwd: &Path, cache: &IconCache, resolve: F) -> Option<String>
where
    F: Fn(&str, &Path, &IconCache) -> Option<String>,
{
    if let Some(uri) = resolve(default_name, cwd, cache) {
        return Some(uri);
    }
    let launch = launch_command.trim();
    if launch.is_empty() {
        return None;
    }
    resolve(launch, cwd, cache)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_icon::{IconExtractor, RealIconExtractor};
    use crate::types::{AiLaunchCommands, AppConfig, CustomProcessDef, CustomProcessDefId, CustomProcessKind};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    fn def_with(id: &str, command: &str, icon_data_uri: Option<String>) -> CustomProcessDef {
        CustomProcessDef {
            id: CustomProcessDefId::new(id),
            kind: CustomProcessKind::Terminal,
            name: id.into(),
            command: command.into(),
            enabled: true,
            icon: None,
            icon_data_uri,
        }
    }

    /// A test extractor that hands back a fixed PNG for any path it's asked about, recording the calls so we can assert dedup.
    struct CountingExtractor {
        calls: Mutex<Vec<PathBuf>>,
    }
    impl CountingExtractor {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
    }
    impl IconExtractor for CountingExtractor {
        fn exe_path(&self, _pid: u32) -> Option<PathBuf> {
            None
        }
        fn extract_png(&self, exe: &Path) -> Option<Vec<u8>> {
            self.calls.lock().unwrap().push(exe.to_path_buf());
            // Tiny fake PNG header — base64 encoder doesn't care about content, the data URI plumbing is what we're exercising.
            Some(vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
        }
    }

    #[test]
    fn backfill_skips_when_cached_value_already_present() {
        let cache = IconCache::new(Arc::new(CountingExtractor::new()));
        let mut cfg = AppConfig {
            custom_processes: vec![def_with("x", "pwsh", Some("data:image/png;base64,AAAA".into()))],
            ai_launch_commands: AiLaunchCommands {
                // Pre-populate AI fields so they don't trigger best-effort default-name resolution and confuse the `changed` flag we're asserting on.
                icon_data_uris: std::collections::BTreeMap::from([
                    ("claude".to_owned(), Some("data:image/png;base64,BBBB".into())),
                    ("copilot".to_owned(), Some("data:image/png;base64,CCCC".into())),
                ]),
                ..Default::default()
            },
            ..Default::default()
        };
        let changed = backfill_icons(&mut cfg, &cache, std::env::temp_dir().as_path());
        assert!(!changed, "no fields should have been backfilled");
        assert_eq!(
            cfg.custom_processes[0].icon_data_uri.as_deref(),
            Some("data:image/png;base64,AAAA"),
            "existing cache must be preserved verbatim",
        );
    }

    #[test]
    fn backfill_populates_missing_uri_for_resolvable_command() {
        // Real extractor — on Windows the system shell APIs will hand back *something* for cmd.exe. Skip if the test env can't resolve it (e.g. Linux
        // CI without wine).
        let cache = IconCache::new(Arc::new(RealIconExtractor));
        // We need a command that resolves to a real exe but isn't an interpreter. On Windows `notepad` is universally present and resolvable via
        // PATH.
        let probe_cmd = if cfg!(windows) { "notepad" } else { "ls" };
        if resolve_command_icon_path(probe_cmd, &std::env::temp_dir()).is_none() {
            eprintln!("skipping: {probe_cmd} not resolvable in this env");
            return;
        }
        let mut cfg = AppConfig {
            custom_processes: vec![def_with("x", probe_cmd, None)],
            ..Default::default()
        };
        let changed = backfill_icons(&mut cfg, &cache, std::env::temp_dir().as_path());
        if cfg!(windows) {
            assert!(changed, "expected icon to be backfilled for {probe_cmd}");
            assert!(cfg.custom_processes[0]
                .icon_data_uri
                .as_deref()
                .is_some_and(|s| s.starts_with("data:image/png;base64,")));
        }
    }

    /// `resolve_ai_icon` should prefer the default CLI name's icon over the user's customized launch command when the default is resolvable. This is
    /// the "agency wrapper" use case — the user runs Claude through some in-house dispatcher but still wants the Claude brand icon, not the
    /// wrapper's.
    ///
    /// We test via the injectable `resolve_ai_icon_with` seam so the test never has to touch the global `PATH` env var (which races with parallel
    /// tests in the same process).
    #[test]
    fn ai_icon_prefers_default_name_over_launch_command_wrapper() {
        let cache = IconCache::new(Arc::new(CountingExtractor::new()));
        let cwd = std::env::temp_dir();
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls_for_resolver = Arc::clone(&calls);
        // Fake resolver: returns Some only when asked about the canonical default name. For anything else (the wrapper command), pretends resolution
        // fails.
        let resolve = move |program: &str, _: &Path, _: &IconCache| -> Option<String> {
            calls_for_resolver.lock().unwrap().push(program.to_owned());
            (program == "claude").then(|| "data:image/png;base64,DEFAULT".to_owned())
        };

        let result = resolve_ai_icon_with("claude", "C:/wrappers/agency.exe --tool=claude", &cwd, &cache, resolve);
        assert_eq!(result.as_deref(), Some("data:image/png;base64,DEFAULT"));
        // Critical assertion: only the *default* name was queried — the wrapper command was never asked about, because the default succeeded first.
        assert_eq!(*calls.lock().unwrap(), vec!["claude".to_owned()]);
    }

    /// Mirror of the above: when the default name is *not* resolvable, the wrapper command's icon is used as a fallback.
    #[test]
    fn ai_icon_falls_back_to_launch_command_when_default_unresolvable() {
        let cache = IconCache::new(Arc::new(CountingExtractor::new()));
        let cwd = std::env::temp_dir();
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls_for_resolver = Arc::clone(&calls);
        let resolve = move |program: &str, _: &Path, _: &IconCache| -> Option<String> {
            calls_for_resolver.lock().unwrap().push(program.to_owned());
            (program == "C:/wrappers/agency.exe --tool=claude").then(|| "data:image/png;base64,WRAPPER".to_owned())
        };

        let result = resolve_ai_icon_with("claude", "C:/wrappers/agency.exe --tool=claude", &cwd, &cache, resolve);
        assert_eq!(result.as_deref(), Some("data:image/png;base64,WRAPPER"));
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["claude".to_owned(), "C:/wrappers/agency.exe --tool=claude".to_owned()],
            "default tried first, wrapper tried as fallback"
        );
    }

    /// Empty / whitespace launch command + unresolvable default → `None` (frontend falls back to the bundled SVG).
    #[test]
    fn ai_icon_returns_none_when_default_fails_and_launch_is_empty() {
        let cache = IconCache::new(Arc::new(CountingExtractor::new()));
        let cwd = std::env::temp_dir();
        let resolve = |_: &str, _: &Path, _: &IconCache| -> Option<String> { None };
        assert!(resolve_ai_icon_with("claude", "   ", &cwd, &cache, resolve).is_none());
        assert!(resolve_ai_icon_with("claude", "", &cwd, &cache, resolve).is_none());
    }
}
