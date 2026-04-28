//! Tauri command handlers.
//!
//! Phase 3 introduces only the `ping` smoke-test command so the typed
//! frontend ↔ backend RPC scaffold can be exercised end-to-end before any
//! real business logic lands. All future commands listed in
//! `dev/docs/DESIGN.md` §6 will live in this module and follow the same
//! pattern: deserialise via typed payload structs in [`crate::types`], call
//! into the relevant subsystem, and convert errors to [`AppError`] at the
//! boundary.
//!
//! ## Capability model (Tauri v2)
//!
//! In Tauri v2, application-defined commands are gated by capability
//! declarations the same way plugin commands are. Each command needs a
//! permission file under `src-tauri/permissions/` referenced from
//! `src-tauri/capabilities/main.json`. The permission for `ping` lives in
//! `permissions/allow-ping.toml` and is referenced as `"allow-ping"` from
//! the main capability. Adding a new command without the matching
//! permission entry will cause the `invoke()` call to be rejected at
//! runtime with no compile-time warning.

use crate::types::AppError;

/// Smoke-test command used to verify the Tauri command/event scaffold is
/// wired correctly. Always returns `Ok("pong")`.
#[tauri::command]
pub async fn ping() -> Result<String, AppError> {
    Ok("pong".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ping_returns_pong() {
        let result = ping().await.expect("ping is infallible");
        assert_eq!(result, "pong");
    }
}
