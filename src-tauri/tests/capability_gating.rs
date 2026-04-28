//! Capability gating regression test.
//!
//! ## Context
//!
//! In Tauri v2, every command callable from the WebView — including
//! application-defined commands — is gated by a capability declaration that
//! references a permission file (see
//! <https://v2.tauri.app/security/permissions/>). Adding a new
//! `#[tauri::command]` without registering the matching permission in
//! `capabilities/main.json` causes the `invoke()` call to be rejected at
//! runtime with no compile-time warning. We have already paid that bill
//! once during Phase 3 development; this test exists to keep paying down
//! the debt by failing loudly the next time.
//!
//! ## What we test (and what we don't)
//!
//! Ideally we would build a `tauri::test::mock_app()` with a stripped-down
//! capability that *omits* `allow-ping`, invoke `ping`, and assert the
//! invocation is rejected. In Tauri 2.x the public `tauri::test` surface
//! does not yet expose a way to override the embedded capability set per
//! test build (the capability JSON is baked into the binary via
//! `tauri::generate_context!` at compile time). Rather than ship a fake
//! test that looks meaningful but isn't, we settle for a structural
//! assertion on the checked-in capability file:
//!
//! * `core:default` is present (the catch-all for built-in core APIs).
//! * `allow-ping` is present (the permission that gates the `ping`
//!   application command).
//! * The corresponding `permissions/allow-ping.toml` file exists and
//!   declares `commands.allow = ["ping"]`.
//!
//! Together these prove (a) the production build will accept `ping`
//! invocations, and (b) deleting either the capability entry or the
//! permission file fails CI rather than silently breaking the WebView.
//!
//! When `tauri::test` grows ergonomic capability overrides, replace the
//! structural assertions below with a true negative round-trip. Tracked
//! informally as a Phase-3 follow-up.

use std::path::PathBuf;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn main_capability_allows_core_default_and_ping() {
    let path = manifest_dir().join("capabilities").join("main.json");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    let value: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {}", path.display(), e));

    let permissions = value
        .get("permissions")
        .and_then(|p| p.as_array())
        .expect("`permissions` must be an array");

    let identifiers: Vec<&str> = permissions.iter().filter_map(|p| p.as_str()).collect();

    assert!(
        identifiers.contains(&"core:default"),
        "main capability must include core:default; got {identifiers:?}",
    );
    assert!(
        identifiers.contains(&"allow-ping"),
        "main capability must include allow-ping so the `ping` command is callable; got {identifiers:?}",
    );
    assert!(
        identifiers.contains(&"allow-config"),
        "main capability must include allow-config so config_get/config_set are callable; got {identifiers:?}",
    );
    assert!(
        identifiers.contains(&"allow-instructions"),
        "main capability must include allow-instructions so instructions_list is callable; got {identifiers:?}",
    );
}

#[test]
fn allow_ping_permission_file_declares_ping_command() {
    let path = manifest_dir().join("permissions").join("allow-ping.toml");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));

    // Cheap structural check — avoids pulling toml as a dev-dependency just
    // for this test. We assert the load-bearing tokens are present.
    assert!(
        raw.contains("identifier = \"allow-ping\""),
        "permission identifier must remain `allow-ping`",
    );
    assert!(
        raw.contains("\"ping\""),
        "permission must allow the `ping` command",
    );
}

#[test]
fn allow_config_permission_file_declares_config_commands() {
    let path = manifest_dir().join("permissions").join("allow-config.toml");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        raw.contains("identifier = \"allow-config\""),
        "permission identifier must remain `allow-config`",
    );
    assert!(
        raw.contains("\"config_get\""),
        "permission must allow the `config_get` command",
    );
    assert!(
        raw.contains("\"config_set\""),
        "permission must allow the `config_set` command",
    );
}

#[test]
fn allow_instructions_permission_file_declares_instructions_command() {
    let path = manifest_dir()
        .join("permissions")
        .join("allow-instructions.toml");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        raw.contains("identifier = \"allow-instructions\""),
        "permission identifier must remain `allow-instructions`",
    );
    assert!(
        raw.contains("\"instructions_list\""),
        "permission must allow the `instructions_list` command",
    );
}
