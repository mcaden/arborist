//! Arborist backend library crate.
//!
//! Phase 2 introduces the shared data model in [`types`]; Phase 3 adds the
//! [`commands`] module with the typed RPC scaffold (currently just `ping`).
//! Later phases will add the PTY pool, config store, and real command handlers.

pub mod activity;
pub mod app_launcher;
pub mod cmd_resolver;
pub mod commands;
pub mod compose;
pub mod config_store;
pub mod copilot_events;
pub mod git;
pub mod icon_backfill;
pub mod plugins;
pub mod process_icon;
pub mod pty_pool;
pub mod repo_settings;
pub mod session_metrics;
pub mod session_temp;
pub mod shell_trust;
pub mod sub_sessions;
/// Wire-contract types for the Rust backend ↔ React frontend boundary.
///
/// Re-exported from the standalone `arborist-types` crate so editing those
/// types only recompiles that small crate, not the whole `arborist_lib` /
/// `arborist` binary. Internal call sites continue to use `crate::types::*`
/// and external tests continue to use `arborist_lib::types::*` unchanged.
pub use arborist_types as types;
pub mod window_focus;
pub mod worktree_icon;
pub mod worktree_prep;

pub use types::{
    AppConfig, AppError, Error, PartialAppConfig, Session, SessionCreateArgs, SessionId, SessionIdArg, SessionInputArgs, SessionMetricsEvent,
    SessionOutputEvent, SessionResizeArgs, SessionStatus, SessionStatusEvent, SessionView, TempFileSpec, Tool, WorkspaceValidateArgs,
    WorkspaceValidateResult, WorktreeCreateArgs, WorktreeCreateResult, WorktreeInfo, CONFIG_VERSION_CURRENT,
};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

/// Keeps the non-blocking file-appender thread alive for the duration of the app.  Stored in Tauri managed state so it is dropped when the app exits.
#[allow(dead_code)]
struct LogGuard(WorkerGuard);

fn make_env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Initialise the global `tracing` subscriber.
///
/// When `log_dir` is `Some`, a rolling daily log file is written alongside the console output. The file writer runs on a background thread; the
/// returned `WorkerGuard` must be kept alive for the lifetime of the process.
///
/// When `log_dir` is `None` (e.g. in tests / smoke examples) only the console layer is installed.
///
/// The log level for both outputs is driven by the `RUST_LOG` environment variable; if unset it defaults to `info`.
pub fn init_tracing(log_dir: Option<&std::path::Path>) -> Option<WorkerGuard> {
    let console_layer = tracing_subscriber::fmt::layer().with_filter(make_env_filter());

    if let Some(dir) = log_dir {
        let file_appender = tracing_appender::rolling::daily(dir, "arborist.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_filter(make_env_filter());

        let _ = tracing_subscriber::registry().with(console_layer).with(file_layer).try_init();

        tracing::info!(path = %dir.display(), "file logging initialised");
        Some(guard)
    } else {
        let _ = tracing_subscriber::registry().with(console_layer).try_init();

        None
    }
}

/// Compute the main window title for a given build branch and (optionally) bound workspace.
///
/// Format (issue #56):
/// * canonical build, no workspace → `Arborist`
/// * canonical build, workspace bound → `Arborist - <workspace>`
/// * feature build, no workspace → `Arborist {<branch>}`
/// * feature build, workspace bound → `Arborist - <workspace> {<branch>}`
///
/// "Canonical" matches [`store_layout::is_canonical_build`] (empty branch or the literal `"main"`) so the title-bar story and the storage-scoping
/// story stay aligned to one rule. The branch in the title is the **build-time** `BUILD_BRANCH` — same axis as storage scoping — not the workspace's
/// currently-checked-out branch. The workspace name is the path's trailing component (see [`workspace_basename`]).
pub(crate) fn window_title(branch: &str, workspace_root: Option<&std::path::Path>) -> String {
    let trimmed_branch = branch.trim();
    let workspace_name = workspace_root.and_then(workspace_basename);

    let mut title = String::from("Arborist");
    if let Some(ws) = workspace_name {
        title.push_str(" - ");
        title.push_str(&ws);
    }
    if !store_layout::is_canonical_build(branch) {
        title.push_str(" {");
        title.push_str(trimmed_branch);
        title.push('}');
    }
    title
}

/// Display name for a workspace path: the trailing path component, lossily stringified. Returns `None` when the path has no usable component (e.g.
/// a filesystem root like `/` or `C:\`), so callers can decide to omit the workspace segment entirely rather than print an empty name.
fn workspace_basename(path: &std::path::Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Branch this binary was built from, captured at compile time by `build.rs`
/// and embedded via a generated file under `OUT_DIR` (no environment variable
/// involved on either side).
pub(crate) const BUILD_BRANCH: &str = include_str!(concat!(env!("OUT_DIR"), "/build_branch.txt"));

pub mod boot;
pub mod seed;
pub mod store_layout;
pub mod workspace_lock;
pub mod workspace_scope;

/// Build and run the Tauri application.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            use tauri::Manager;

            // Initialise logging first so every subsequent log call is captured. We hold the WorkerGuard locally throughout the boot block:
            // `std::process::exit` skips destructors, so any boot-failure exit path below MUST `drop(log_guard)` first to flush buffered log lines to
            // disk. On success we hand the guard off to Tauri's managed state.
            let log_dir = app.path().app_log_dir()?;
            std::fs::create_dir_all(&log_dir)?;
            let log_guard = init_tracing(Some(&log_dir));
            tracing::info!("Arborist starting up");

            // If this build came from a branch other than `main`, surface the branch name in the window title bar so it's obvious which build is
            // running. We set the title twice: once here with no workspace bound (covers the brief startup window before `boot_select_workspace`
            // returns), and again after the workspace binding succeeds with the bound `workspace_root` so the workspace name is included
            // (issue #56).
            if let Some(window) = app.get_webview_window("main") {
                let title = window_title(BUILD_BRANCH, None);
                if let Err(err) = window.set_title(&title) {
                    tracing::warn!(%err, "failed to set main window title");
                }
            }

            // Phase 6: resolve, lock, and bind the per-(branch, workspace) store. If successful, the AppContext is bound to the resolved workspace.
            // If no workspace is available (fresh install, contention, invalid saved workspace), an *unbound* AppContext is built so the WebView can
            // load immediately and let the frontend's in-app workspace picker handle selection.
            //
            // Resolution chain (non-blocking): --workspace CLI arg → branch hint file → legacy `<app_data_dir>/config.json::workspace_root`.
            // Native folder picker dialogs are no longer used during boot — workspace selection is handled by the in-app picker when unbound.
            let app_data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&app_data_dir)?;
            let cli_args = match boot::parse_cli_args(std::env::args_os()) {
                Ok(a) => a,
                Err(e) => {
                    tracing::error!(error = %e, "boot CLI parse failed");
                    eprintln!("Arborist CLI parse error: {e}");
                    drop(log_guard);
                    std::process::exit(2);
                }
            };

            // Boot needs a GitRunner to verify resolved workspace paths are git repository roots before binding (matches the `workspace_validate`
            // command for the in-app picker). RealGitRunner is cheap to construct (zero-sized) — we build one early and reuse the same instance for
            // the AppContext below so the in-app commands share it.
            let boot_git_runner = git::RealGitRunner;

            // Attempt synchronous workspace resolution. CLI failures are still hard exits (the user explicitly chose a workspace). All other
            // failures (hint/legacy contention, fresh install, invalid saved workspace) produce an unbound boot so the WebView can load immediately
            // and let the in-app picker handle selection. This eliminates blocking the setup closure on native `rfd` dialogs, which caused
            // "AppContext not initialised" errors because the WebView loaded and called commands while setup was still blocked.
            let binding = match boot::boot_select_workspace_nonblocking(&cli_args, &app_data_dir, BUILD_BRANCH, &boot_git_runner) {
                Ok(Some(b)) => Some(b),
                Ok(None) => {
                    tracing::info!("no workspace bound at boot; starting in unbound mode (in-app picker will handle selection)");
                    None
                }
                Err(boot::BootError::Contention { branch, workspace }) if cli_args.workspace.is_some() => {
                    boot::show_lock_contention_dialog(&branch, &workspace);
                    drop(log_guard);
                    std::process::exit(1);
                }
                Err(boot::BootError::Contention { .. }) => {
                    tracing::info!("saved workspace is locked by another instance; starting in unbound mode");
                    None
                }
                Err(boot::BootError::NotARepository { workspace, reason, origin }) if matches!(origin, boot::BootSource::Cli) => {
                    tracing::error!(
                        ?origin, workspace = %workspace.display(), reason = %reason,
                        "workspace is not a git repository root",
                    );
                    eprintln!(
                        "Arborist failed to open workspace ({origin:?}): {reason}\n  workspace: {}",
                        workspace.display()
                    );
                    drop(log_guard);
                    std::process::exit(1);
                }
                Err(boot::BootError::NotARepository { workspace, reason, origin }) => {
                    tracing::warn!(
                        ?origin, workspace = %workspace.display(), reason = %reason,
                        "resolved workspace is not a valid git repository root; starting unbound"
                    );
                    None
                }
                Err(boot::BootError::WorkspaceRootPersist { dir, source }) => {
                    boot::show_workspace_root_persist_dialog(&dir, &source.to_string());
                    drop(log_guard);
                    std::process::exit(1);
                }
                Err(e) if cli_args.workspace.is_some() => {
                    tracing::error!(error = %e, "workspace boot bind failed");
                    eprintln!("Arborist failed to open workspace: {e}");
                    drop(log_guard);
                    std::process::exit(1);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "workspace boot bind failed; starting unbound");
                    None
                }
            };

            // Boot succeeded — hand the WorkerGuard off to Tauri's managed state so logs continue to flush for the lifetime of the running app.
            if let Some(guard) = log_guard {
                app.manage(LogGuard(guard));
            }

            // Re-set the window title now that we know which workspace we bound (issue #56). The earlier set above used `None` so users see the build
            // branch immediately; this update adds the workspace name. Best-effort: a failure here only affects the title, not the boot.
            if let Some(ref b) = binding {
                if let Some(window) = app.get_webview_window("main") {
                    let title = window_title(BUILD_BRANCH, Some(&b.workspace_root));
                    if let Err(err) = window.set_title(&title) {
                        tracing::warn!(%err, "failed to update main window title after workspace bind");
                    }
                }
            }

            // Build the production AppContext: portable-pty spawner, the workspace-bound (or unbound) ConfigStore (held behind RwLock so phase 7
            // workspace_switch can transactionally swap it), and a PtySink that bridges back into both Tauri events and the persisted session record.
            // The sink/discover closures take the workspace handle (not a snapshot) so they always operate on the currently-bound store, even after a
            // switch.
            let scope = match binding {
                Some(b) => boot::into_scope(b),
                None => {
                    // Unbound boot: no workspace, no persistence needed. Use a per-run tempdir so `config_get` returns a pristine
                    // default AppConfig (the file won't exist → defaults with `workspaceRoot: null`). The scope is swapped out when
                    // the frontend picker calls `workspace_switch`.
                    let scratch_dir = std::env::temp_dir().join(format!("arborist-unbound-{}", std::process::id()));
                    let scratch_store = config_store::ConfigStore::open(&scratch_dir)
                        .map_err(|e| format!("cannot create scratch store for unbound boot at {}: {e}", scratch_dir.display()))?;
                    workspace_scope::WorkspaceScope::unbound(scratch_store)
                }
            };
            let is_bound = !scope.is_unbound();
            let workspace_handle = std::sync::Arc::new(std::sync::RwLock::new(scope));
            let pool = std::sync::Arc::new(pty_pool::PtyPool::new(std::sync::Arc::new(pty_pool::PortablePtySpawner)));
            let sink = commands::build_production_sink(app.handle().clone(), workspace_handle.clone());
            let metrics_emit = commands::build_production_metrics_emit(app.handle().clone(), workspace_handle.clone());
            let ai_session_discover = commands::build_production_ai_session_discover(workspace_handle.clone());
            let turn_emit = commands::build_production_turn_emit(app.handle().clone());
            let git_runner: std::sync::Arc<dyn git::GitRunner> = std::sync::Arc::new(git::RealGitRunner);
            let ctx = std::sync::Arc::new(commands::AppContext::with_workspace(
                pool,
                workspace_handle,
                sink,
                git_runner,
                metrics_emit,
                ai_session_discover,
                turn_emit,
            ));
            // Hold a local Arc so the startup backfill below can share the *same* `ConfigStore` (and its write lock) that subsequent `config_set`
            // calls will use.
            let ctx_for_backfill = ctx.clone();
            app.manage(ctx);

            // Plugin registry wiring: built-in AI, custom-process, and dashboard-widget plugins all register through `plugins::build_registry()`.
            //
            // A `RegisterError` here means a developer added two plugins with the same id — log + structured exit instead of an `expect()` panic
            // so the user sees a single clear line and the process exits cleanly (matches the boot-failure pattern earlier in this block).
            let plugin_registry = match plugins::build_registry() {
                Ok(reg) => reg,
                Err(err) => {
                    tracing::error!(error = %err, "plugin registry build failed");
                    eprintln!("Arborist failed to build plugin registry: {err}");
                    std::process::exit(1);
                }
            };
            let plugin_registry = std::sync::Arc::new(plugin_registry);
            app.manage(plugin_registry.clone());

            // Phase 2: parallel sub-session pool + store + sink. Lives alongside the existing AppContext so existing tests don't need to know about
            // it.
            let sub_pool = std::sync::Arc::new(sub_sessions::SubPtyPool::new(std::sync::Arc::new(pty_pool::PortablePtySpawner)));
            let sub_store = std::sync::Arc::new(sub_sessions::SubSessionStore::new());
            let sub_sink = commands::build_production_sub_sink(app.handle().clone(), sub_store.clone());
            // Phase 3: application sub-tabs. Their pool reuses the same sink (output is no-op for apps, status / exited flow into the same Tauri
            // events as terminal sub-tabs).
            let app_pool = std::sync::Arc::new(app_launcher::AppPool::new(std::sync::Arc::new(app_launcher::RealAppSpawner)));
            let focuser: std::sync::Arc<dyn window_focus::WindowFocuser> = std::sync::Arc::new(window_focus::RealFocuser);
            let icon_cache = std::sync::Arc::new(process_icon::IconCache::new(std::sync::Arc::new(process_icon::RealIconExtractor)));
            let sub_ctx = std::sync::Arc::new(sub_sessions::SubAppContext::new(
                sub_pool,
                sub_store,
                sub_sink,
                plugin_registry,
                app_pool,
                focuser,
                icon_cache,
            ));
            app.manage(sub_ctx.clone());

            // Apply `--ai-launch-claude` / `--ai-launch-copilot` overrides if supplied. Only meaningful when bound to a workspace.
            if is_bound && (cli_args.ai_launch_claude.is_some() || cli_args.ai_launch_copilot.is_some()) {
                let store = ctx_for_backfill.store();
                let mut commands = std::collections::BTreeMap::new();
                if let Some(v) = cli_args.ai_launch_claude.clone() {
                    commands.insert(types::Tool::Claude.as_id().to_owned(), v);
                }
                if let Some(v) = cli_args.ai_launch_copilot.clone() {
                    commands.insert(types::Tool::Copilot.as_id().to_owned(), v);
                }
                let patch = types::PartialAppConfig {
                    ai_launch_commands: Some(types::PartialAiLaunchCommands { commands }),
                    ..Default::default()
                };
                if let Err(err) = store.save_config(patch) {
                    tracing::warn!(%err, "failed to apply --ai-launch-* CLI overrides to config");
                }
            }

            // Best-effort: warm the persisted icon cache. Only meaningful when bound to a workspace.
            if is_bound {
                let store = ctx_for_backfill.store();
                let cache = sub_ctx.icon_cache.clone();
                if let Err(err) = store.save_config_with(types::PartialAppConfig::default(), move |cfg| {
                    let cwd = cfg.workspace_root.clone().filter(|p| p.is_dir()).unwrap_or_else(std::env::temp_dir);
                    icon_backfill::backfill_icons(cfg, &cache, &cwd)
                }) {
                    tracing::warn!(
                        %err,
                        "startup icon backfill: failed to persist refreshed config"
                    );
                } else {
                    tracing::debug!("startup icon backfill: cache populated");
                }
            }

            // Show the main window now that setup is complete. The window starts hidden (`visible: false` in tauri.conf.json) to prevent a
            // white flash while the WebView initialises. Showing it here — after AppContext is managed and ready for commands — ensures the
            // frontend's boot effect sees a fully initialised backend.
            if let Some(window) = app.get_webview_window("main") {
                if let Err(err) = window.show() {
                    tracing::warn!(%err, "failed to show main window after setup");
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::ping,
            commands::config_get,
            commands::config_set,
            commands::shell_command_preview,
            commands::repo_command_trust,
            commands::repo_command_allow_once,
            commands::dialog_pick_directory,
            commands::session_create,
            commands::session_list,
            commands::session_close,
            commands::session_focus,
            commands::session_resize,
            commands::session_input,
            commands::session_restart,
            commands::frontend_ready,
            commands::worktrees_list,
            commands::worktree_git_status,
            commands::workspace_validate,
            commands::workspace_switch,
            commands::worktree_create,
            commands::worktree_prep_open_log,
            commands::subsession_create,
            commands::subsession_close,
            commands::subsession_focus,
            commands::subsession_list,
            commands::subsession_input,
            commands::subsession_resize,
            commands::subsession_relaunch,
            commands::subsession_icon,
            commands::worktree_tab_open,
            commands::worktree_tab_close,
            commands::worktree_tab_focus,
            commands::worktree_tab_list,
            commands::worktree_tab_reorder,
            commands::worktree_tab_set_active_child,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Arborist");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn init_tracing_is_idempotent() {
        // Calling twice must not panic — the global subscriber may already be set.
        init_tracing(None);
        init_tracing(None);
    }

    // ----- window_title — full matrix of (canonical-build × workspace-bound). Issue #56.

    #[test]
    fn title_canonical_build_no_workspace_is_plain() {
        assert_eq!(window_title("main", None), "Arborist");
        assert_eq!(window_title("", None), "Arborist");
        assert_eq!(window_title("   ", None), "Arborist");
    }

    #[test]
    fn title_canonical_build_with_workspace_appends_dash_workspace() {
        assert_eq!(window_title("main", Some(Path::new("/Users/dev/projects/grove"))), "Arborist - grove");
        assert_eq!(window_title("", Some(Path::new("/Users/dev/projects/grove"))), "Arborist - grove");
    }

    #[test]
    fn title_feature_build_no_workspace_wraps_branch_in_braces() {
        assert_eq!(window_title("feature/x", None), "Arborist {feature/x}");
        assert_eq!(window_title("branch-name", None), "Arborist {branch-name}");
    }

    #[test]
    fn title_feature_build_with_workspace_includes_both() {
        assert_eq!(
            window_title("my-feature-branch", Some(Path::new("/Users/dev/projects/my-workspace"))),
            "Arborist - my-workspace {my-feature-branch}",
        );
    }

    #[test]
    fn title_trims_branch_whitespace_in_braces() {
        assert_eq!(
            window_title("  feature/x  ", Some(Path::new("/Users/dev/projects/grove"))),
            "Arborist - grove {feature/x}",
        );
        assert_eq!(window_title("  branch-name  ", None), "Arborist {branch-name}");
    }

    // ----- workspace_basename edge cases.

    #[cfg(unix)]
    #[test]
    fn workspace_basename_unix_returns_trailing_component() {
        assert_eq!(workspace_basename(Path::new("/Users/dev/projects/grove")), Some("grove".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_basename_unix_handles_trailing_separator() {
        // Path::file_name skips the trailing separator and returns the real basename.
        assert_eq!(workspace_basename(Path::new("/Users/dev/projects/grove/")), Some("grove".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_basename_unix_returns_none_for_root() {
        assert_eq!(workspace_basename(Path::new("/")), None);
    }

    #[cfg(windows)]
    #[test]
    fn workspace_basename_windows_returns_trailing_component() {
        assert_eq!(workspace_basename(Path::new(r"C:\repos\grove")), Some("grove".to_string()));
    }

    #[cfg(windows)]
    #[test]
    fn workspace_basename_windows_handles_trailing_separator() {
        assert_eq!(workspace_basename(Path::new(r"C:\repos\grove\")), Some("grove".to_string()));
    }

    #[cfg(windows)]
    #[test]
    fn workspace_basename_windows_returns_none_for_drive_root() {
        assert_eq!(workspace_basename(Path::new(r"C:\")), None);
    }

    #[test]
    fn workspace_basename_preserves_unicode() {
        // file_name + to_string_lossy preserves valid UTF-8 unchanged.
        let path = std::env::temp_dir().join("プロジェクト");
        assert_eq!(workspace_basename(&path).as_deref(), Some("プロジェクト"));
    }
}
