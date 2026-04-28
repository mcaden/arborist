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
pub mod types;

pub use types::{
    AppConfig, AppError, DefaultInstructionSets, Error, InstructionSet, InstructionSetId,
    PartialAppConfig, PartialDefaultInstructionSets, Session, SessionCreateArgs, SessionId,
    SessionIdArg, SessionInputArgs, SessionOutputEvent, SessionResizeArgs, SessionStatus,
    SessionStatusEvent, SessionView, TempFileSpec, Tool, WorkspaceValidateArgs,
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

/// Build and run the Tauri application.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            use tauri::Manager;

            // Initialise logging first so every subsequent log call is captured.
            let log_dir = app.path().app_log_dir()?;
            std::fs::create_dir_all(&log_dir)?;
            if let Some(guard) = init_tracing(Some(&log_dir)) {
                app.manage(LogGuard(guard));
            }
            tracing::info!("Arborist starting up");

            // Build the production AppContext: portable-pty spawner, the
            // on-disk ConfigStore, and a PtySink that bridges back into
            // both Tauri events and the persisted session record.
            let store = commands::store_for(app.handle())?;
            let pool = std::sync::Arc::new(pty_pool::PtyPool::new(std::sync::Arc::new(
                pty_pool::PortablePtySpawner,
            )));
            let sink = commands::build_production_sink(app.handle().clone(), store.clone());
            let git_runner: std::sync::Arc<dyn git::GitRunner> =
                std::sync::Arc::new(git::RealGitRunner);
            let ctx = std::sync::Arc::new(commands::AppContext::new(pool, store, sink, git_runner));
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
}
