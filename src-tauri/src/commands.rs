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

use crate::config_store::{list_instructions_for, ConfigStore};
use crate::types::{AppConfig, AppError, InstructionSet, PartialAppConfig};
use std::path::PathBuf;
use tauri::Manager;

/// Smoke-test command used to verify the Tauri command/event scaffold is
/// wired correctly. Always returns `Ok("pong")`.
#[tauri::command]
pub async fn ping() -> Result<String, AppError> {
    Ok("pong".to_owned())
}

/// Resolve the [`ConfigStore`] for the current Tauri app instance.
///
/// We rely on Tauri's `Manager::path()` helper to give us the per-OS app-data
/// directory (`%APPDATA%/com.grove.app` on Windows,
/// `~/Library/Application Support/com.grove.app` on macOS, and
/// `$XDG_DATA_HOME/com.grove.app` (or `~/.local/share/com.grove.app`) on
/// Linux). The directory is created on first access.
fn store_for(app: &tauri::AppHandle) -> Result<ConfigStore, AppError> {
    let dir: PathBuf = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::new("Io", format!("app_data_dir: {e}")))?;
    ConfigStore::open(dir).map_err(AppError::from)
}

/// Returns the persisted [`AppConfig`], applying canonicalization and
/// fallback rules documented on [`ConfigStore::load_config`].
#[tauri::command]
pub async fn config_get(app: tauri::AppHandle) -> Result<AppConfig, AppError> {
    let store = store_for(&app)?;
    Ok(store.load_config())
}

/// Deep-merges `partial` into the persisted [`AppConfig`] and returns
/// nothing on success. Path fields in `partial` are validated and
/// canonicalized; relative paths are rejected with `InvalidPath`.
#[tauri::command]
pub async fn config_set(app: tauri::AppHandle, partial: PartialAppConfig) -> Result<(), AppError> {
    let store = store_for(&app)?;
    store.save_config(partial).map_err(AppError::from)?;
    Ok(())
}

/// Discovers and returns the list of [`InstructionSet`]s available under the
/// configured `instructionSetsDir`.
#[tauri::command]
pub async fn instructions_list(app: tauri::AppHandle) -> Result<Vec<InstructionSet>, AppError> {
    let store = store_for(&app)?;
    let cfg = store.load_config();
    list_instructions_for(&cfg)
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
