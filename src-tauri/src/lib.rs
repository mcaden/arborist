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

use tracing_subscriber::EnvFilter;

/// Initialise the global `tracing` subscriber.
///
/// The log level is driven by the `RUST_LOG` environment variable; if it is
/// unset we fall back to `info`.
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Build and run the Tauri application.
pub fn run() {
    init_tracing();

    tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // Build the production AppContext: portable-pty spawner, the
            // on-disk ConfigStore, and a PtySink that bridges back into
            // both Tauri events and the persisted session record.
            use tauri::Manager;
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
        init_tracing();
        init_tracing();
    }
}
