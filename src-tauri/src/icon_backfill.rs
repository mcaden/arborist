//! Resolve and cache icon data URIs for `CustomProcessDef`s and the
//! Claude / Copilot AI-launch commands.
//!
//! Strategy: at config-save time and at app startup, walk every
//! command string that maps to a sidebar tab, run it through
//! [`crate::cmd_resolver::resolve_command_icon_path`], extract a PNG
//! data URI via [`crate::process_icon::IconCache::data_uri_for_path`],
//! and store the result on the def itself (`icon_data_uri`). The
//! frontend then renders synchronously from the persisted config —
//! no per-render IPC call into the OS shell APIs.
//!
//! Pure side-effect: mutates `cfg`. The caller decides whether to
//! re-persist (the function returns whether anything actually
//! changed).

use std::path::Path;

use crate::cmd_resolver::resolve_command_icon_path;
use crate::process_icon::IconCache;
use crate::types::AppConfig;

/// Walk every icon-bearing command in `cfg` and fill in any missing
/// `icon_data_uri` fields. Existing values are left alone — the
/// invalidation contract lives in [`crate::config_store::merge_partial`],
/// which clears the cache when a command changes. Returns `true` if
/// any field was populated (i.e. caller should re-persist).
///
/// `fallback_cwd` is used for relative-path resolution. Defs are
/// templates (not bound to a worktree at save time), so the workspace
/// root is the most useful default; OS temp dir is an acceptable last
/// resort.
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

    if cfg.ai_launch_commands.claude_icon_data_uri.is_none() {
        if let Some(uri) = resolve_ai_icon(
            "claude",
            &cfg.ai_launch_commands.claude,
            fallback_cwd,
            cache,
        ) {
            cfg.ai_launch_commands.claude_icon_data_uri = Some(uri);
            changed = true;
        }
    }
    if cfg.ai_launch_commands.copilot_icon_data_uri.is_none() {
        if let Some(uri) = resolve_ai_icon(
            "copilot",
            &cfg.ai_launch_commands.copilot,
            fallback_cwd,
            cache,
        ) {
            cfg.ai_launch_commands.copilot_icon_data_uri = Some(uri);
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
/// Users can replace the launch command with anything — npm shims,
/// in-house "agency" wrappers, custom dispatchers — and we don't
/// want those wrapper icons showing up where Claude's or Copilot's
/// brand icon belongs. So we try the *canonical* CLI name first and
/// only fall back to the user's actual launch command if the default
/// can't be resolved on this machine.
///
/// 1. Resolve the default name (`claude` / `copilot`) on PATH+PATHEXT.
///    If that succeeds and isn't an interpreter wrapper, use its icon.
/// 2. Otherwise, fall back to resolving the user-configured
///    `launch_command` (if non-empty).
/// 3. Otherwise, return `None` — the frontend falls back to the
///    bundled `ToolIcon` SVG glyph.
fn resolve_ai_icon(
    default_name: &str,
    launch_command: &str,
    cwd: &Path,
    cache: &IconCache,
) -> Option<String> {
    if let Some(uri) = resolve_one(default_name, cwd, cache) {
        return Some(uri);
    }
    let launch = launch_command.trim();
    if launch.is_empty() {
        return None;
    }
    resolve_one(launch, cwd, cache)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_icon::{IconExtractor, RealIconExtractor};
    use crate::types::{
        AiLaunchCommands, AppConfig, CustomProcessDef, CustomProcessDefId, CustomProcessKind,
    };
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

    /// A test extractor that hands back a fixed PNG for any path it's
    /// asked about, recording the calls so we can assert dedup.
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
            // Tiny fake PNG header — base64 encoder doesn't care about
            // content, the data URI plumbing is what we're exercising.
            Some(vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
        }
    }

    #[test]
    fn backfill_skips_when_cached_value_already_present() {
        let cache = IconCache::new(Arc::new(CountingExtractor::new()));
        let mut cfg = AppConfig {
            custom_processes: vec![def_with(
                "x",
                "pwsh",
                Some("data:image/png;base64,AAAA".into()),
            )],
            ai_launch_commands: AiLaunchCommands {
                claude: String::new(),
                copilot: String::new(),
                // Pre-populate AI fields so they don't trigger
                // best-effort default-name resolution and confuse
                // the `changed` flag we're asserting on.
                claude_icon_data_uri: Some("data:image/png;base64,BBBB".into()),
                copilot_icon_data_uri: Some("data:image/png;base64,CCCC".into()),
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
        // Real extractor — on Windows the system shell APIs will hand
        // back *something* for cmd.exe. Skip if the test env can't
        // resolve it (e.g. Linux CI without wine).
        let cache = IconCache::new(Arc::new(RealIconExtractor));
        // We need a command that resolves to a real exe but isn't an
        // interpreter. On Windows `notepad` is universally present
        // and resolvable via PATH.
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

    /// `resolve_ai_icon` should prefer the default CLI name's icon
    /// over the user's customized launch command when the default is
    /// resolvable. This is the "agency wrapper" use case — the user
    /// runs Claude through some in-house dispatcher but still wants
    /// the Claude brand icon, not the wrapper's.
    #[test]
    fn ai_icon_prefers_default_name_over_launch_command_wrapper() {
        let dir = tempfile::tempdir().unwrap();

        // Set up a *fake* default `claude.exe` in our isolated PATH.
        let default_dir = dir.path().join("default");
        std::fs::create_dir_all(&default_dir).unwrap();
        let default_exe = default_dir.join(if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        });
        std::fs::File::create(&default_exe).unwrap();

        // And a *wrapper* exe at a different path that isn't on PATH.
        let wrapper_dir = dir.path().join("wrapper");
        std::fs::create_dir_all(&wrapper_dir).unwrap();
        let wrapper_exe = wrapper_dir.join(if cfg!(windows) {
            "agency.exe"
        } else {
            "agency"
        });
        std::fs::File::create(&wrapper_exe).unwrap();

        // Pretend each call to the icon extractor returns a unique
        // PNG so we can tell which path resolved.
        struct PathTaggingExtractor;
        impl IconExtractor for PathTaggingExtractor {
            fn exe_path(&self, _pid: u32) -> Option<PathBuf> {
                None
            }
            fn extract_png(&self, exe: &Path) -> Option<Vec<u8>> {
                // Embed the basename in the bytes — base64 of these
                // is what the resulting data URI carries.
                Some(
                    exe.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .as_bytes()
                        .to_vec(),
                )
            }
        }
        let cache = IconCache::new(Arc::new(PathTaggingExtractor));

        // Inject our isolated PATH so `claude` resolves to default_exe.
        let prev_path = std::env::var_os("PATH");
        let mut new_path = std::ffi::OsString::from(default_dir.as_os_str());
        if let Some(p) = &prev_path {
            new_path.push(if cfg!(windows) { ";" } else { ":" });
            new_path.push(p);
        }
        // SAFETY: tests run sequentially within a single process by default
        // for cargo test (no parallelism inside this binary's #[test]s
        // unless run with --test-threads). Other tests that read PATH
        // could race; we restore below.
        unsafe {
            std::env::set_var("PATH", &new_path);
        }
        let result = resolve_ai_icon("claude", wrapper_exe.to_str().unwrap(), dir.path(), &cache);
        unsafe {
            match prev_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }

        let uri = result.expect("icon should resolve");
        // Decode the base64 payload to recover the basename we embedded.
        let b64 = uri.strip_prefix("data:image/png;base64,").unwrap();
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let recovered = String::from_utf8(bytes).unwrap();
        let expected = if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        };
        // PATHEXT is typically `.EXE;.CMD;…` on Windows, so the
        // resolver may return `claude.EXE` even though we created
        // `claude.exe` on disk. Compare case-insensitively — what
        // matters is that the *basename* came from the default name,
        // not from `agency`.
        assert_eq!(
            recovered.to_ascii_lowercase(),
            expected,
            "AI icon should come from the default CLI name, not the wrapper"
        );
    }
}
