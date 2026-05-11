//! Capability gating regression test.
//!
//! ## Context
//!
//! In Tauri v2, every command callable from the WebView — including application-defined commands — is gated by a capability declaration that
//! references a permission file (see <https://v2.tauri.app/security/permissions/>). Adding a new `#[tauri::command]` without registering the matching
//! permission in `capabilities/main.json` causes the `invoke()` call to be rejected at runtime with no compile-time warning. We have already paid
//! that bill once during Phase 3 development; this test exists to keep paying down the debt by failing loudly the next time.
//!
//! ## What we test (and what we don't)
//!
//! Ideally we would build a `tauri::test::mock_app()` with a stripped-down capability that *omits* `allow-ping`, invoke `ping`, and assert the
//! invocation is rejected. In Tauri 2.x the public `tauri::test` surface does not yet expose a way to override the embedded capability set per test
//! build (the capability JSON is baked into the binary via `tauri::generate_context!` at compile time). Rather than ship a fake test that looks
//! meaningful but isn't, we settle for a structural assertion on the checked-in capability file:
//!
//! * `core:default` is present (the catch-all for built-in core APIs).
//! * `allow-ping` is present (the permission that gates the `ping` application
//!   command).
//! * The corresponding `permissions/allow-ping.toml` file exists and declares
//!   `commands.allow = ["ping"]`.
//!
//! Together these prove (a) the production build will accept `ping` invocations, and (b) deleting either the capability entry or the permission file
//! fails CI rather than silently breaking the WebView.
//!
//! When `tauri::test` grows ergonomic capability overrides, replace the structural assertions below with a true negative round-trip. Tracked
//! informally as a Phase-3 follow-up.

use std::path::PathBuf;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn main_capability_allows_core_default_and_ping() {
    let path = manifest_dir().join("capabilities").join("main.json");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {}", path.display(), e));

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
    assert!(
        identifiers.contains(&"allow-session"),
        "main capability must include allow-session so session_* commands are callable; got {identifiers:?}",
    );
    assert!(
        identifiers.contains(&"allow-frontend-ready"),
        "main capability must include allow-frontend-ready so frontend_ready is callable; got {identifiers:?}",
    );
    assert!(
        identifiers.contains(&"allow-worktrees-list"),
        "main capability must include allow-worktrees-list so worktrees_list is callable; got {identifiers:?}",
    );
    assert!(
        identifiers.contains(&"allow-workspace-validate"),
        "main capability must include allow-workspace-validate so workspace_validate is callable; got {identifiers:?}",
    );
    assert!(
        identifiers.contains(&"allow-workspace-switch"),
        "main capability must include allow-workspace-switch so workspace_switch is callable; got {identifiers:?}",
    );
    assert!(
        identifiers.contains(&"allow-worktree-create"),
        "main capability must include allow-worktree-create so worktree_create is callable; got {identifiers:?}",
    );
    assert!(
        identifiers.contains(&"allow-worktree-prep-open-log"),
        "main capability must include allow-worktree-prep-open-log so worktree_prep_open_log is callable; got {identifiers:?}",
    );
    assert!(
        identifiers.contains(&"dialog:allow-open"),
        "main capability must include dialog:allow-open so the file picker is callable; got {identifiers:?}",
    );
    assert!(
        identifiers.contains(&"allow-subsession-icon"),
        "main capability must include allow-subsession-icon so subsession_icon is callable; got {identifiers:?}",
    );
    assert!(
        identifiers.contains(&"allow-subsession"),
        "main capability must include allow-subsession so subsession_* commands are callable; got {identifiers:?}",
    );
    assert!(
        identifiers.contains(&"allow-worktree-tab"),
        "main capability must include allow-worktree-tab so worktree_tab_* commands are callable; got {identifiers:?}",
    );
    assert!(
        identifiers.contains(&"allow-worktree-git-status"),
        "main capability must include allow-worktree-git-status so worktree_git_status is callable; got {identifiers:?}",
    );
}

#[test]
fn allow_worktree_git_status_permission_file_declares_command() {
    let path = manifest_dir().join("permissions").join("allow-worktree-git-status.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        raw.contains("identifier = \"allow-worktree-git-status\""),
        "permission identifier must remain `allow-worktree-git-status`",
    );
    assert!(
        raw.contains("\"worktree_git_status\""),
        "allow-worktree-git-status must declare worktree_git_status; raw permission file:\n{raw}",
    );
}

#[test]
fn allow_subsession_permission_file_declares_subsession_commands() {
    let path = manifest_dir().join("permissions").join("allow-subsession.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        raw.contains("identifier = \"allow-subsession\""),
        "permission identifier must remain `allow-subsession`",
    );
    for cmd in [
        "subsession_create",
        "subsession_close",
        "subsession_focus",
        "subsession_list",
        "subsession_input",
        "subsession_resize",
        "subsession_relaunch",
    ] {
        let needle = format!("\"{cmd}\"");
        assert!(raw.contains(&needle), "allow-subsession must declare {cmd}; raw permission file:\n{raw}",);
    }
}

#[test]
fn allow_subsession_icon_permission_file_declares_command() {
    let path = manifest_dir().join("permissions").join("allow-subsession-icon.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        raw.contains("identifier = \"allow-subsession-icon\""),
        "permission identifier must remain `allow-subsession-icon`",
    );
    assert!(raw.contains("\"subsession_icon\""), "permission must allow the `subsession_icon` command",);
}

#[test]
fn allow_ping_permission_file_declares_ping_command() {
    let path = manifest_dir().join("permissions").join("allow-ping.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));

    // Cheap structural check — avoids pulling toml as a dev-dependency just for this test. We assert the load-bearing tokens are present.
    assert!(
        raw.contains("identifier = \"allow-ping\""),
        "permission identifier must remain `allow-ping`",
    );
    assert!(raw.contains("\"ping\""), "permission must allow the `ping` command",);
}

#[test]
fn allow_config_permission_file_declares_config_commands() {
    let path = manifest_dir().join("permissions").join("allow-config.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        raw.contains("identifier = \"allow-config\""),
        "permission identifier must remain `allow-config`",
    );
    assert!(raw.contains("\"config_get\""), "permission must allow the `config_get` command",);
    assert!(raw.contains("\"config_set\""), "permission must allow the `config_set` command",);
}

#[test]
fn allow_instructions_permission_file_declares_instructions_command() {
    let path = manifest_dir().join("permissions").join("allow-instructions.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        raw.contains("identifier = \"allow-instructions\""),
        "permission identifier must remain `allow-instructions`",
    );
    assert!(
        raw.contains("\"instructions_list\""),
        "permission must allow the `instructions_list` command",
    );
}

#[test]
fn allow_session_permission_file_declares_session_commands() {
    let path = manifest_dir().join("permissions").join("allow-session.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        raw.contains("identifier = \"allow-session\""),
        "permission identifier must remain `allow-session`",
    );
    for cmd in [
        "session_create",
        "session_list",
        "session_close",
        "session_focus",
        "session_resize",
        "session_input",
        "session_restart",
    ] {
        let needle = format!("\"{cmd}\"");
        assert!(raw.contains(&needle), "allow-session must declare {cmd}; raw permission file:\n{raw}",);
    }
}

#[test]
fn allow_frontend_ready_permission_file_declares_frontend_ready_command() {
    let path = manifest_dir().join("permissions").join("allow-frontend-ready.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        raw.contains("identifier = \"allow-frontend-ready\""),
        "permission identifier must remain `allow-frontend-ready`",
    );
    assert!(raw.contains("\"frontend_ready\""), "permission must allow the `frontend_ready` command",);
}

#[test]
fn allow_worktrees_list_permission_file_declares_worktrees_command() {
    let path = manifest_dir().join("permissions").join("allow-worktrees-list.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        raw.contains("identifier = \"allow-worktrees-list\""),
        "permission identifier must remain `allow-worktrees-list`",
    );
    assert!(raw.contains("\"worktrees_list\""), "permission must allow the `worktrees_list` command",);
}

#[test]
fn main_capability_grants_workspace_validate() {
    // Covered by the consolidated identifier check in `main_capability_allows_core_default_and_ping`; this test asserts the permission file
    // independently.
    let path = manifest_dir().join("permissions").join("allow-workspace-validate.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        raw.contains("identifier = \"allow-workspace-validate\""),
        "permission identifier must remain `allow-workspace-validate`",
    );
    assert!(
        raw.contains("\"workspace_validate\""),
        "permission must allow the `workspace_validate` command",
    );
}

#[test]
fn allow_workspace_validate_permission_file_declares_command() {
    // Same intent as above test; kept distinct so a regression in either file/identifier is reported with a precise failure name.
    let path = manifest_dir().join("permissions").join("allow-workspace-validate.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(raw.contains("workspace_validate"));
}

#[test]
fn main_capability_grants_worktree_create() {
    let path = manifest_dir().join("permissions").join("allow-worktree-create.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        raw.contains("identifier = \"allow-worktree-create\""),
        "permission identifier must remain `allow-worktree-create`",
    );
    assert!(raw.contains("\"worktree_create\""), "permission must allow the `worktree_create` command",);
}

#[test]
fn allow_worktree_create_permission_file_declares_command() {
    let path = manifest_dir().join("permissions").join("allow-worktree-create.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(raw.contains("worktree_create"));
}

#[test]
fn main_capability_grants_worktree_prep_open_log() {
    let path = manifest_dir().join("permissions").join("allow-worktree-prep-open-log.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        raw.contains("identifier = \"allow-worktree-prep-open-log\""),
        "permission identifier must remain `allow-worktree-prep-open-log`",
    );
    assert!(
        raw.contains("\"worktree_prep_open_log\""),
        "permission must allow the `worktree_prep_open_log` command",
    );
}

#[test]
fn allow_workspace_switch_permission_file_declares_command() {
    let path = manifest_dir().join("permissions").join("allow-workspace-switch.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        raw.contains("identifier = \"allow-workspace-switch\""),
        "permission identifier must remain `allow-workspace-switch`",
    );
    assert!(
        raw.contains("\"workspace_switch\""),
        "permission must allow the `workspace_switch` command",
    );
}

#[test]
fn allow_worktree_tab_permission_file_declares_commands() {
    let path = manifest_dir().join("permissions").join("allow-worktree-tab.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        raw.contains("identifier = \"allow-worktree-tab\""),
        "permission identifier must remain `allow-worktree-tab`"
    );
    for cmd in [
        "worktree_tab_open",
        "worktree_tab_close",
        "worktree_tab_focus",
        "worktree_tab_list",
        "worktree_tab_reorder",
        "worktree_tab_set_active_child",
    ] {
        assert!(raw.contains(&format!("\"{cmd}\"")), "permission must allow the `{cmd}` command");
    }
}
