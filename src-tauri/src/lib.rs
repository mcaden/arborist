//! Grove backend library crate.
//!
//! Phase 1 only contains scaffolding; later phases will add the PTY pool,
//! config store, command handlers, and shared types.

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
