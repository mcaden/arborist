//! Claude hook settings file builder.
//!
//! Composes the per-session `claude-settings.json` we hand to Claude via `--settings <path>` so every hook we register fires the
//! `arborist-claude-hook` helper binary. Merges the user's existing settings (user-home → project → project local, lowest precedence first — see
//! [`user_settings_paths`]) with our hook entries so the user's own PreToolUse formatters / Stop validators / etc. keep running.
//!
//! ## Why per-session not project-shared
//!
//! The hook command line embeds the Arborist session id and the per-session JSONL events path as literal `args` (Claude's "exec form" — no shell
//! substitution). Two Claude sessions in the same worktree therefore need *different* settings files. A per-worktree `.claude/settings.local.json`
//! would clobber the second session's args. `--settings` accepts a per-process JSON path so we read user settings once at create time, merge in our
//! hooks, and write the result to `<session_temp_dir>/claude-settings.json`.
//!
//! ## --settings precedence (from the Claude CLI docs)
//!
//! `--settings <path>` overrides same-keyed top-level fields from the user's `~/.claude/settings.json` and project `.claude/settings*.json`. Keys
//! we omit keep their file-based values. The `hooks` key in particular is an *array of arrays* per event — Claude does not deep-merge, so if we set
//! `hooks.PreToolUse` and the user also has `hooks.PreToolUse`, theirs is shadowed. To preserve user behaviour, [`merge_user_settings`] explicitly
//! concatenates the user's `hooks.<EventName>` arrays into the file we write.

use std::path::Path;

use serde_json::{json, Map, Value};

use crate::types::SessionId;

/// Claude hook event names we register: `(<EventName>, <helper-arg>, <matcher>)`. The first two fields map 1:1 to the helper binary's first
/// positional argument; the third is the Claude `matcher` field, which scopes *which* sub-variant of the event we want to be fired for. Most
/// events use `""` (match every fire); `Notification` uses `"idle_prompt"` to scope to the "Claude is idle waiting on the user" sub-event and
/// skip the noisier siblings (`permission_prompt`, `elicitation_*`, `auth_success`).
pub const HOOK_EVENTS: &[(&str, &str, &str)] = &[
    ("PreToolUse", "pre-tool-use", ""),
    ("PostToolUse", "post-tool-use", ""),
    ("PostToolUseFailure", "post-tool-failure", ""),
    ("PermissionRequest", "permission-request", ""),
    ("UserPromptSubmit", "user-prompt", ""),
    ("Stop", "stop", ""),
    ("SessionEnd", "session-end", ""),
    // `idle_prompt` is Claude's "agent is idle, waiting on the user" notification — the case where Claude finishes its message with a question
    // and parks at its prompt. Mapped to `ActivityEvent::Attention` by the tailer so the sidebar promotes the tab to the attention state until
    // the user focuses it. Other `Notification` matchers (`permission_prompt`, `elicitation_*`, `auth_success`) are deliberately not subscribed:
    // permission flow is already covered by `PermissionRequest` above, and the rest would be noise.
    ("Notification", "notification-idle", "idle_prompt"),
];

/// Build the per-session `claude-settings.json` payload as a string ready to drop into a [`crate::types::TempFileSpec`].
///
/// `user_settings_files` lists the *paths* the host wants merged in (user-home `settings.json`, project `settings.json`, project
/// `settings.local.json`) — oldest precedence first. We read each lazily; missing or unparseable files are skipped with a tracing warn (the user's
/// other settings still apply).
///
/// `helper_exe` is the absolute path to `arborist-claude-hook` (resolved at AppContext startup via [`std::env::current_exe`]'s sibling). When
/// absent, [`crate::compose`] elides the `--settings` flag entirely and falls back to today's hook-less behaviour.
pub fn build_settings_string(
    helper_exe: &Path,
    arborist_session_id: SessionId,
    events_path: &Path,
    user_settings_files: &[std::path::PathBuf],
) -> String {
    let mut merged = Value::Object(Map::new());
    for path in user_settings_files {
        match read_settings_file(path) {
            Ok(Some(v)) => merge_user_settings(&mut merged, v),
            Ok(None) => {} // missing — silent
            Err(e) => tracing::warn!(path = %path.display(), error = %e, "claude user settings file unreadable; skipping"),
        }
    }
    append_arborist_hooks(&mut merged, helper_exe, arborist_session_id, events_path);
    // Pretty-print: makes the file readable in `<session_temp_dir>/` for debugging, costs ~negligible bytes.
    serde_json::to_string_pretty(&merged).unwrap_or_else(|_| String::from("{}"))
}

fn read_settings_file(path: &Path) -> std::io::Result<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    let parsed: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
    };
    Ok(Some(parsed))
}

/// Merge `overlay` into `base`. For non-`hooks` top-level keys the overlay wins (later-precedence settings clobber earlier ones — matches Claude's
/// own settings-precedence model). For `hooks.<EventName>` arrays we **concatenate** so an earlier file's hook entries run alongside the overlay's.
/// Other shapes inside `hooks` are passed through with overlay-wins semantics.
pub fn merge_user_settings(base: &mut Value, overlay: Value) {
    let Value::Object(overlay_obj) = overlay else {
        return; // top-level non-objects are ignored (Claude settings files are always objects)
    };
    let base_obj = match base {
        Value::Object(o) => o,
        _ => {
            *base = Value::Object(Map::new());
            match base {
                Value::Object(o) => o,
                _ => unreachable!(),
            }
        }
    };
    for (key, value) in overlay_obj {
        if key == "hooks" {
            merge_hook_blocks(base_obj.entry(key).or_insert_with(|| Value::Object(Map::new())), value);
        } else {
            base_obj.insert(key, value);
        }
    }
}

fn merge_hook_blocks(base_hooks: &mut Value, overlay_hooks: Value) {
    let Value::Object(overlay_hooks_obj) = overlay_hooks else { return };
    let base_hooks_obj = match base_hooks {
        Value::Object(o) => o,
        _ => {
            *base_hooks = Value::Object(Map::new());
            match base_hooks {
                Value::Object(o) => o,
                _ => unreachable!(),
            }
        }
    };
    for (event_name, overlay_entries) in overlay_hooks_obj {
        // Each value is conventionally an array of `{ matcher, hooks: [...] }` blocks. Concatenate.
        let entry = base_hooks_obj.entry(event_name).or_insert_with(|| Value::Array(Vec::new()));
        match (entry, overlay_entries) {
            (Value::Array(existing), Value::Array(more)) => existing.extend(more),
            // Overlay is non-array (unexpected shape) — fall back to overlay-wins.
            (slot, other) => *slot = other,
        }
    }
}

fn append_arborist_hooks(base: &mut Value, helper_exe: &Path, arborist_session_id: SessionId, events_path: &Path) {
    let base_obj = match base {
        Value::Object(o) => o,
        _ => {
            *base = Value::Object(Map::new());
            match base {
                Value::Object(o) => o,
                _ => unreachable!(),
            }
        }
    };
    let hooks_root = base_obj.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    let hooks_obj = match hooks_root {
        Value::Object(o) => o,
        _ => {
            *hooks_root = Value::Object(Map::new());
            match hooks_root {
                Value::Object(o) => o,
                _ => unreachable!(),
            }
        }
    };
    let exe_str = helper_exe.to_string_lossy().into_owned();
    let session_id_str = arborist_session_id.0.to_string();
    let events_str = events_path.to_string_lossy().into_owned();
    for (event_name, helper_arg, matcher) in HOOK_EVENTS {
        let entry = json!({
            "matcher": *matcher,
            "hooks": [
                {
                    "type": "command",
                    // Exec form: `args` array bypasses shell parsing. `${VAR}` placeholders are NOT substituted in this form, so we bake the
                    // session id and events path in as literal strings — exactly the routing data the helper needs.
                    "command": exe_str,
                    "args": [helper_arg, session_id_str.clone(), events_str.clone()],
                }
            ]
        });
        let arr = hooks_obj.entry((*event_name).to_owned()).or_insert_with(|| Value::Array(Vec::new()));
        match arr {
            Value::Array(v) => v.push(entry),
            _ => *arr = Value::Array(vec![entry]),
        }
    }
}

/// Resolve the list of user settings files to merge, in oldest-precedence-first order: user-home `settings.json`, project `settings.json`,
/// project `settings.local.json`. Caller passes `None` for any source it does not have (e.g. tests, or a session without a resolved home dir).
#[must_use]
pub fn user_settings_paths(user_home: Option<&Path>, worktree: Option<&Path>) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = user_home {
        out.push(home.join(".claude").join("settings.json"));
    }
    if let Some(wt) = worktree {
        out.push(wt.join(".claude").join("settings.json"));
        out.push(wt.join(".claude").join("settings.local.json"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn fixed_id() -> SessionId {
        SessionId(Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid"))
    }

    fn parse(json_str: &str) -> Value {
        serde_json::from_str(json_str).expect("settings file must be valid JSON")
    }

    #[test]
    fn build_settings_registers_every_event() {
        let s = build_settings_string(
            Path::new("/usr/local/bin/arborist-claude-hook"),
            fixed_id(),
            Path::new("/tmp/events.jsonl"),
            &[],
        );
        let v = parse(&s);
        let hooks = &v["hooks"];
        for (event_name, _, _) in HOOK_EVENTS {
            assert!(
                hooks.get(*event_name).and_then(|a| a.as_array()).map(|a| !a.is_empty()).unwrap_or(false),
                "event {event_name} missing"
            );
        }
    }

    #[test]
    fn build_settings_uses_exec_form_with_literal_args() {
        let s = build_settings_string(
            Path::new("/usr/local/bin/arborist-claude-hook"),
            fixed_id(),
            Path::new("/tmp/events.jsonl"),
            &[],
        );
        let v = parse(&s);
        let pre_tool = &v["hooks"]["PreToolUse"][0];
        assert_eq!(pre_tool["matcher"], "");
        let inner = &pre_tool["hooks"][0];
        assert_eq!(inner["type"], "command");
        assert_eq!(inner["command"], "/usr/local/bin/arborist-claude-hook");
        let args = inner["args"].as_array().unwrap();
        assert_eq!(args.len(), 3);
        assert_eq!(args[0], "pre-tool-use");
        assert_eq!(args[1], fixed_id().0.to_string());
        assert_eq!(args[2], "/tmp/events.jsonl");
    }

    #[test]
    fn merge_user_pretool_hooks_runs_alongside_ours() {
        let tmp = tempfile::tempdir().unwrap();
        let user_path = tmp.path().join("user.json");
        let user_settings = serde_json::json!({
            "model": "claude-opus-4-7",
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "prettier" }] }
                ]
            }
        });
        std::fs::write(&user_path, serde_json::to_string(&user_settings).unwrap()).unwrap();

        let s = build_settings_string(
            Path::new("/abs/arborist-claude-hook"),
            fixed_id(),
            Path::new("/tmp/events.jsonl"),
            std::slice::from_ref(&user_path),
        );
        let v = parse(&s);
        let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2, "expected user entry + ours");
        // User's entry first.
        assert_eq!(pre[0]["matcher"], "Bash");
        assert_eq!(pre[0]["hooks"][0]["command"], "prettier");
        // Ours appended.
        assert_eq!(pre[1]["hooks"][0]["command"], "/abs/arborist-claude-hook");
        // Non-hook top-level keys are preserved from the user file.
        assert_eq!(v["model"], "claude-opus-4-7");
    }

    #[test]
    fn merge_user_settings_overlay_wins_for_non_hook_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.json");
        let b = tmp.path().join("b.json");
        std::fs::write(&a, br#"{ "model": "old", "theme": "light" }"#).unwrap();
        std::fs::write(&b, br#"{ "model": "new" }"#).unwrap();

        let s = build_settings_string(
            Path::new("/abs/arborist-claude-hook"),
            fixed_id(),
            Path::new("/tmp/events.jsonl"),
            &[a.clone(), b.clone()],
        );
        let v = parse(&s);
        // Later file overrides same key.
        assert_eq!(v["model"], "new");
        // Earlier file's unique keys persist.
        assert_eq!(v["theme"], "light");
    }

    #[test]
    fn missing_user_settings_file_is_silently_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let s = build_settings_string(
            Path::new("/abs/arborist-claude-hook"),
            fixed_id(),
            Path::new("/tmp/events.jsonl"),
            &[tmp.path().join("does-not-exist.json")],
        );
        let v = parse(&s);
        assert!(v["hooks"]["PreToolUse"].is_array());
    }

    #[test]
    fn malformed_user_settings_file_does_not_panic_and_does_not_break_our_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("bad.json");
        std::fs::write(&bad, b"this is not json").unwrap();

        let s = build_settings_string(
            Path::new("/abs/arborist-claude-hook"),
            fixed_id(),
            Path::new("/tmp/events.jsonl"),
            std::slice::from_ref(&bad),
        );
        let v = parse(&s);
        // Even with a broken user file, our hooks must register.
        for (event_name, _, _) in HOOK_EVENTS {
            assert!(
                v["hooks"].get(*event_name).is_some(),
                "event {event_name} missing after malformed user file"
            );
        }
    }

    #[test]
    fn user_settings_paths_includes_all_three_layers() {
        let home = std::path::PathBuf::from("/home/u");
        let wt = std::path::PathBuf::from("/repos/x");
        let paths = user_settings_paths(Some(&home), Some(&wt));
        assert_eq!(paths.len(), 3);
        assert!(paths[0].ends_with(".claude/settings.json"));
        assert!(paths[1].ends_with(".claude/settings.json"));
        assert!(paths[2].ends_with(".claude/settings.local.json"));
    }

    #[test]
    fn user_settings_paths_handles_missing_home_or_worktree() {
        assert!(user_settings_paths(None, None).is_empty());
        assert_eq!(user_settings_paths(Some(Path::new("/h")), None).len(), 1);
        assert_eq!(user_settings_paths(None, Some(Path::new("/w"))).len(), 2);
    }
}
