//! Grove backend library crate.
//!
//! Phase 2 introduces the shared data model in [`types`]; Phase 3 adds the
//! [`commands`] module with the typed RPC scaffold (currently just `ping`).
//! Later phases will add the PTY pool, config store, and real command
//! handlers.

pub mod commands;
pub mod compose;
pub mod config_store;
pub mod pty_pool;
pub mod types;

pub use types::{
    AppConfig, AppError, DefaultInstructionSets, Error, InstructionSet, InstructionSetId,
    PartialAppConfig, PartialDefaultInstructionSets, Session, SessionId, SessionOutputEvent,
    SessionStatus, SessionStatusEvent, SessionView, TempFileSpec, Tool, CONFIG_VERSION_CURRENT,
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
        .invoke_handler(tauri::generate_handler![
            commands::ping,
            commands::config_get,
            commands::config_set,
            commands::instructions_list,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Grove");
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
