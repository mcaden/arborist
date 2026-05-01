//! Arborist backend library crate.
//!
//! Phase 2 introduces the shared data model in [`types`]; Phase 3 adds the
//! [`commands`] module with the typed RPC scaffold (currently just `ping`).
//! Later phases will add the PTY pool, config store, and real command
//! handlers.

pub mod activity;
pub mod commands;
pub mod compose;
pub mod config_store;
pub mod git;
pub mod pty_pool;
pub mod session_metrics;
pub mod types;

pub use types::{
    AppConfig, AppError, DefaultInstructionSets, Error, InstructionSet, InstructionSetId,
    PartialAppConfig, PartialDefaultInstructionSets, Session, SessionCreateArgs, SessionId,
    SessionIdArg, SessionInputArgs, SessionMetricsEvent, SessionOutputEvent, SessionResizeArgs,
    SessionStatus, SessionStatusEvent, SessionView, TempFileSpec, Tool, WorkspaceValidateArgs,
    WorkspaceValidateResult, WorktreeCreateArgs, WorktreeCreateResult, WorktreeInfo,
    CONFIG_VERSION_CURRENT,
};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

/// Keeps the non-blocking file-appender thread alive for the duration of the
/// app.  Stored in Tauri managed state so it is dropped when the app exits.
#[allow(dead_code)]
struct LogGuard(WorkerGuard);

fn make_env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Initialise the global `tracing` subscriber.
///
/// When `log_dir` is `Some`, a rolling daily log file is written alongside
/// the console output.  The file writer runs on a background thread; the
/// returned `WorkerGuard` must be kept alive for the lifetime of the process.
///
/// When `log_dir` is `None` (e.g. in tests / smoke examples) only the
/// console layer is installed.
///
/// The log level for both outputs is driven by the `RUST_LOG` environment
/// variable; if unset it defaults to `info`.
pub fn init_tracing(log_dir: Option<&std::path::Path>) -> Option<WorkerGuard> {
    let console_layer = tracing_subscriber::fmt::layer().with_filter(make_env_filter());

    if let Some(dir) = log_dir {
        let file_appender = tracing_appender::rolling::daily(dir, "arborist.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_filter(make_env_filter());

        let _ = tracing_subscriber::registry()
            .with(console_layer)
            .with(file_layer)
            .try_init();

        tracing::info!(path = %dir.display(), "file logging initialised");
        Some(guard)
    } else {
        let _ = tracing_subscriber::registry()
            .with(console_layer)
            .try_init();

        None
    }
}

/// Compute the main window title for a given build branch.
///
/// On `main` (or when no branch could be detected) the title is just
/// `"Arborist"`.  On any other branch the title becomes
/// `"Arborist - <branch>"` so it is obvious which build is running.
pub(crate) fn window_title_for_branch(branch: &str) -> String {
    let trimmed = branch.trim();
    if trimmed.is_empty() || trimmed == "main" {
        "Arborist".to_string()
    } else {
        format!("Arborist - {trimmed}")
    }
}

/// Branch this binary was built from, captured at compile time by `build.rs`.
pub(crate) const BUILD_BRANCH: &str = env!("ARBORIST_BUILD_BRANCH");

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

            // Initialise logging first so every subsequent log call is
            // captured. We hold the WorkerGuard locally throughout the
            // boot block: `std::process::exit` skips destructors, so any
            // boot-failure exit path below MUST `drop(log_guard)` first
            // to flush buffered log lines to disk. On success we hand
            // the guard off to Tauri's managed state.
            let log_dir = app.path().app_log_dir()?;
            std::fs::create_dir_all(&log_dir)?;
            let log_guard = init_tracing(Some(&log_dir));
            tracing::info!("Arborist starting up");

            // If this build came from a branch other than `main`, surface the
            // branch name in the window title bar so it's obvious which build
            // is running.
            if let Some(window) = app.get_webview_window("main") {
                let title = window_title_for_branch(BUILD_BRANCH);
                if let Err(err) = window.set_title(&title) {
                    tracing::warn!(%err, "failed to set main window title");
                }
            }

            // Phase 6: resolve, lock, and bind the per-(branch, workspace)
            // store before any AppContext is built. This guarantees that
            // restore-on-launch and every later command operates on the
            // isolated workspace store, never on the legacy shared one.
            //
            // Resolution chain: --workspace CLI arg → branch hint file →
            // legacy `<app_data_dir>/config.json::workspace_root` →
            // native folder picker (rfd).
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

            let binding = match boot::boot_select_workspace(&cli_args, &app_data_dir, BUILD_BRANCH)
            {
                Ok(Some(b)) => b,
                Ok(None) => {
                    tracing::info!("user cancelled workspace picker; exiting");
                    drop(log_guard);
                    std::process::exit(0);
                }
                Err(boot::BootError::Contention { branch, workspace }) => {
                    boot::show_lock_contention_dialog(&branch, &workspace);
                    drop(log_guard);
                    std::process::exit(1);
                }
                Err(e) => {
                    tracing::error!(error = %e, "workspace boot bind failed");
                    eprintln!("Arborist failed to open workspace: {e}");
                    drop(log_guard);
                    std::process::exit(1);
                }
            };

            // Boot succeeded — hand the WorkerGuard off to Tauri's
            // managed state so logs continue to flush for the lifetime
            // of the running app.
            if let Some(guard) = log_guard {
                app.manage(LogGuard(guard));
            }

            // Build the production AppContext: portable-pty spawner, the
            // workspace-bound ConfigStore, and a PtySink that bridges back
            // into both Tauri events and the persisted session record.
            let store = binding.store.clone();
            let scope = boot::into_scope(binding);
            let workspace_handle = std::sync::Arc::new(std::sync::RwLock::new(scope));
            let pool = std::sync::Arc::new(pty_pool::PtyPool::new(std::sync::Arc::new(
                pty_pool::PortablePtySpawner,
            )));
            let sink = commands::build_production_sink(app.handle().clone(), store.clone());
            let metrics_emit = commands::build_production_metrics_emit(app.handle().clone());
            let ai_session_discover = commands::build_production_ai_session_discover(store.clone());
            let turn_emit = commands::build_production_turn_emit(app.handle().clone());
            let git_runner: std::sync::Arc<dyn git::GitRunner> =
                std::sync::Arc::new(git::RealGitRunner);
            let ctx = std::sync::Arc::new(commands::AppContext::with_workspace(
                pool,
                workspace_handle,
                sink,
                git_runner,
                metrics_emit,
                ai_session_discover,
                turn_emit,
            ));
            app.manage(ctx);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::ping,
            commands::config_get,
            commands::config_set,
            commands::instructions_list,
            commands::session_create,
            commands::session_list,
            commands::session_close,
            commands::session_focus,
            commands::session_resize,
            commands::session_input,
            commands::session_restart,
            commands::frontend_ready,
            commands::worktrees_list,
            commands::workspace_validate,
            commands::worktree_create,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Arborist");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_tracing_is_idempotent() {
        // Calling twice must not panic — the global subscriber may already be set.
        init_tracing(None);
        init_tracing(None);
    }

    #[test]
    fn title_on_main_is_plain() {
        assert_eq!(window_title_for_branch("main"), "Arborist");
    }

    #[test]
    fn title_on_empty_branch_is_plain() {
        assert_eq!(window_title_for_branch(""), "Arborist");
        assert_eq!(window_title_for_branch("   "), "Arborist");
    }

    #[test]
    fn title_on_feature_branch_includes_name() {
        assert_eq!(window_title_for_branch("feature/x"), "Arborist - feature/x");
        assert_eq!(
            window_title_for_branch("  branch-name  "),
            "Arborist - branch-name"
        );
    }
}
