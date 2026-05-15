//! `arborist-claude-hook` — sidecar binary registered as a Claude Code hook handler.
//!
//! Claude Code spawns this process at every hook fire point we register (PreToolUse, PostToolUse, PostToolUseFailure, PermissionRequest, Stop,
//! UserPromptSubmit, SessionEnd). The hook payload (JSON) arrives on stdin; the JSONL events file path and the Arborist session id arrive as
//! `args` baked into the per-session `claude-settings.json` Arborist materialised at session-create time. The helper translates the payload to a
//! single structured line and atomically appends it to the events file. The backend tailer
//! ([`arborist_lib::claude_hook_events`]) consumes that file.
//!
//! ## CLI contract
//!
//! ```text
//! arborist-claude-hook <event> <arborist-session-id> <events-jsonl-path>
//! ```
//!
//! Recognised events (`<event>` literal token):
//!
//! | event                | Wire `kind`           | Notes                                                                              |
//! |----------------------|-----------------------|------------------------------------------------------------------------------------|
//! | `pre-tool-use`       | `toolStart`           | Extracts `tool_use_id`, `tool_name` from the payload.                              |
//! | `post-tool-use`      | `toolEnd` (success)   | Sets `success: true`.                                                              |
//! | `post-tool-failure`  | `toolEnd` (failure)   | Sets `success: false`.                                                             |
//! | `permission-request` | `awaitingPermission`  | Extracts `tool_use_id`, `tool_name`, `required_permission`, brief tool-input summary. |
//! | `user-prompt`        | `turnStart`           | Drops the payload's `prompt` content — we only care about the lifecycle marker.    |
//! | `stop`               | `turnEnd`             | No duration available from Claude's Stop event.                                    |
//! | `session-end`        | `sessionEnd`          | Hard reset for the tailer state machine.                                           |
//!
//! Unknown `<event>` values are silently treated as no-ops (still exit 0). The helper **never** blocks Claude — every exit path returns 0,
//! including on malformed JSON, missing fields, or unwritable events file. A non-zero exit would interrupt Claude's tool execution, which is far
//! worse than missing a sidebar status update.
//!
//! ## Concurrency
//!
//! Claude can fire multiple hooks concurrently in a session (e.g. parallel tool calls). Each invocation acquires an OS advisory write lock
//! (`fs2::FileExt::lock_exclusive`) on the events file for the brief append, so concurrent writers serialise without losing lines.
//!
//! ## Why a separate binary (not a subcommand of `arborist`)
//!
//! Per the implementation plan: the binary is bundled alongside `arborist.exe` so Tauri-installed deployments find it next to the main app. A
//! subcommand of `arborist` would force every hook fire to load the Tauri runtime — pointless overhead for what should be a sub-millisecond write.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;

fn main() -> ExitCode {
    // Args: program, event, arborist-session-id, events-jsonl-path. Any error here is silent — see the module docs for why we always exit 0.
    let mut argv = std::env::args();
    let _ = argv.next();
    let event = argv.next().unwrap_or_default();
    let _session_id = argv.next().unwrap_or_default(); // kept for future per-session validation; not used in the JSONL payload
    let events_path = argv.next().unwrap_or_default();

    if event.is_empty() || events_path.is_empty() {
        return ExitCode::SUCCESS;
    }

    let payload: serde_json::Value = match read_stdin_json() {
        Ok(v) => v,
        // No payload / unparseable / EOF before any bytes → still write a minimal "kind only" record where applicable. For lifecycle-only events
        // like `stop`, `user-prompt`, `session-end`, no payload data is needed. For tool/permission events without a parseable payload we drop
        // silently since we can't construct a meaningful line.
        Err(_) => serde_json::Value::Null,
    };

    let Some(line) = build_line(&event, &payload) else {
        return ExitCode::SUCCESS;
    };

    let _ = append_line(&PathBuf::from(events_path), &line);
    ExitCode::SUCCESS
}

fn read_stdin_json() -> Result<serde_json::Value, ()> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).map_err(|_| ())?;
    if buf.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&buf).map_err(|_| ())
}

/// Translate a (`<event>`, Claude JSON payload) pair into the JSONL line our tailer consumes. Returns `None` for events whose payload is required
/// but missing, so we drop them silently. Pure — easy to unit-test.
pub fn build_line(event: &str, payload: &serde_json::Value) -> Option<String> {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let mut obj = serde_json::Map::new();
    obj.insert("ts".to_owned(), json_number(ts));

    let tool_use_id = payload.get("tool_use_id").and_then(|v| v.as_str()).map(str::to_owned);
    let tool_name = payload.get("tool_name").and_then(|v| v.as_str()).map(str::to_owned);

    match event {
        "pre-tool-use" => {
            let id = tool_use_id?;
            obj.insert("kind".to_owned(), "toolStart".into());
            obj.insert("toolUseId".to_owned(), id.into());
            if let Some(name) = tool_name {
                obj.insert("toolName".to_owned(), name.into());
            }
        }
        "post-tool-use" | "post-tool-failure" => {
            let id = tool_use_id?;
            obj.insert("kind".to_owned(), "toolEnd".into());
            obj.insert("toolUseId".to_owned(), id.into());
            obj.insert("success".to_owned(), serde_json::Value::Bool(event == "post-tool-use"));
        }
        "permission-request" => {
            let id = tool_use_id?;
            obj.insert("kind".to_owned(), "awaitingPermission".into());
            obj.insert("toolUseId".to_owned(), id.into());
            if let Some(name) = tool_name.clone() {
                obj.insert("toolName".to_owned(), name.into());
            }
            if let Some(perm) = payload.get("required_permission").and_then(|v| v.as_str()) {
                obj.insert("permissionKind".to_owned(), perm.to_owned().into());
            }
            if let Some(summary) = summarize_tool_input(payload) {
                obj.insert("summary".to_owned(), summary.into());
            }
        }
        "user-prompt" => {
            obj.insert("kind".to_owned(), "turnStart".into());
        }
        "stop" => {
            obj.insert("kind".to_owned(), "turnEnd".into());
        }
        "session-end" => {
            obj.insert("kind".to_owned(), "sessionEnd".into());
        }
        _ => return None,
    }

    let line = serde_json::to_string(&serde_json::Value::Object(obj)).ok()?;
    let mut out = line;
    out.push('\n');
    Some(out)
}

/// Build a one-line summary for a permission prompt from the tool input. Best-effort — falls back to `None` if no useful field is present.
fn summarize_tool_input(payload: &serde_json::Value) -> Option<String> {
    let input = payload.get("tool_input")?;
    // Common tool-input shapes we surface (in priority order):
    //   Bash      → command
    //   WebFetch  → url
    //   Read/Edit/Write → file_path
    //   Glob/Grep → pattern
    for key in ["command", "url", "file_path", "pattern", "query"] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str()) {
            // Truncate long values so the sidebar tooltip stays readable. The tailer doesn't truncate, but a 4 KB single-line summary in the
            // events file is silly.
            const MAX: usize = 200;
            if s.chars().count() > MAX {
                let truncated: String = s.chars().take(MAX).collect();
                return Some(format!("{truncated}…"));
            }
            return Some(s.to_owned());
        }
    }
    None
}

fn append_line(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    // Parent dir is guaranteed by the spawn-prep step (`SpawnPrep::ensure_temp_dir`) but be defensive in case the hook fires before that.
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Open + lock + write + unlock + drop. On Windows, concurrent `OPEN_ALWAYS` opens of the same file can transiently fail with `Access is denied`
    // (sharing-violation classified as ERROR_ACCESS_DENIED, code 5) when the OS hasn't yet finalised a prior handle's close. Claude can fire
    // multiple hooks at the same instant for parallel tool calls, so retry with capped backoff. Real-world contention is a handful of hooks per
    // second across separate processes; the long retry budget here is a safety margin, not a hot path.
    const ATTEMPTS: usize = 32;
    let mut last_err: Option<std::io::Error> = None;
    for attempt in 0..ATTEMPTS {
        match try_append_locked(path, line) {
            Ok(()) => return Ok(()),
            Err(e) => {
                // `PermissionDenied` is treated as retryable *only on Windows*, where it surfaces as the sharing-violation ERROR_ACCESS_DENIED
                // described in the comment above and clears as soon as the prior handle finalises. On Unix-like platforms `PermissionDenied`
                // virtually always means an actual mode/owner problem (unwritable temp dir, SELinux/AppArmor block) that retrying won't fix; the
                // 450 ms budget there is wasted, and worse, it delays Claude's tool/turn progression by that much per failing hook fire.
                let retryable_perm_denied = cfg!(windows) && matches!(e.kind(), std::io::ErrorKind::PermissionDenied);
                let retryable = retryable_perm_denied || matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted);
                if !retryable || attempt == ATTEMPTS - 1 {
                    return Err(e);
                }
                last_err = Some(e);
                // Capped exponential backoff: 1, 2, 4, 8, 16 ms (cap), then 16 ms steady. Worst-case total ≈ 450 ms.
                let shift = attempt.min(4) as u32;
                std::thread::sleep(std::time::Duration::from_millis(1u64 << shift));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("append_line: retries exhausted")))
}

fn try_append_locked(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    use std::io::Seek;
    // We want to *append* to the file but `.append(true)` alone doesn't grant the access rights `LockFileEx` needs on Windows. Open with explicit
    // read+write, ask Rust to leave existing content alone (`.truncate(false)`), then seek to end after taking the lock.
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    f.lock_exclusive()?;
    // Seek to end *after* taking the lock so a concurrent process that beat us to write doesn't make us overwrite its bytes.
    let res = f.seek(std::io::SeekFrom::End(0)).and_then(|_| f.write_all(line.as_bytes()));
    let _ = f.unlock();
    res
}

fn json_number(f: f64) -> serde_json::Value {
    serde_json::Number::from_f64(f)
        .map(serde_json::Value::Number)
        .unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> serde_json::Value {
        serde_json::from_str(line.trim_end_matches('\n')).expect("valid JSON line")
    }

    #[test]
    fn pre_tool_use_emits_tool_start() {
        let payload = serde_json::json!({
            "tool_use_id": "tu_abc",
            "tool_name": "Bash",
            "tool_input": { "command": "git push" }
        });
        let line = build_line("pre-tool-use", &payload).unwrap();
        let v = parse(&line);
        assert_eq!(v["kind"], "toolStart");
        assert_eq!(v["toolUseId"], "tu_abc");
        assert_eq!(v["toolName"], "Bash");
    }

    #[test]
    fn post_tool_use_emits_tool_end_success_true() {
        let payload = serde_json::json!({ "tool_use_id": "tu_abc" });
        let line = build_line("post-tool-use", &payload).unwrap();
        let v = parse(&line);
        assert_eq!(v["kind"], "toolEnd");
        assert_eq!(v["toolUseId"], "tu_abc");
        assert_eq!(v["success"], true);
    }

    #[test]
    fn post_tool_failure_emits_tool_end_success_false() {
        let payload = serde_json::json!({ "tool_use_id": "tu_abc" });
        let line = build_line("post-tool-failure", &payload).unwrap();
        let v = parse(&line);
        assert_eq!(v["success"], false);
    }

    #[test]
    fn permission_request_emits_awaiting_permission_with_summary() {
        let payload = serde_json::json!({
            "tool_use_id": "tu_abc",
            "tool_name": "Bash",
            "required_permission": "bash:execute",
            "tool_input": { "command": "git push" }
        });
        let line = build_line("permission-request", &payload).unwrap();
        let v = parse(&line);
        assert_eq!(v["kind"], "awaitingPermission");
        assert_eq!(v["toolUseId"], "tu_abc");
        assert_eq!(v["permissionKind"], "bash:execute");
        assert_eq!(v["summary"], "git push");
        assert_eq!(v["toolName"], "Bash");
    }

    #[test]
    fn user_prompt_emits_turn_start_without_payload_content() {
        let payload = serde_json::json!({ "prompt": "do the thing" });
        let line = build_line("user-prompt", &payload).unwrap();
        let v = parse(&line);
        assert_eq!(v["kind"], "turnStart");
        assert!(v.get("prompt").is_none(), "prompt content must not leak into the events file");
    }

    #[test]
    fn stop_emits_turn_end() {
        let line = build_line("stop", &serde_json::Value::Null).unwrap();
        assert_eq!(parse(&line)["kind"], "turnEnd");
    }

    #[test]
    fn session_end_emits_session_end() {
        let line = build_line("session-end", &serde_json::Value::Null).unwrap();
        assert_eq!(parse(&line)["kind"], "sessionEnd");
    }

    #[test]
    fn unknown_event_returns_none() {
        assert!(build_line("future-event", &serde_json::Value::Null).is_none());
    }

    #[test]
    fn missing_tool_use_id_drops_tool_event() {
        let payload = serde_json::json!({ "tool_name": "Bash" });
        assert!(build_line("pre-tool-use", &payload).is_none());
        assert!(build_line("post-tool-use", &payload).is_none());
        assert!(build_line("permission-request", &payload).is_none());
    }

    #[test]
    fn summarize_long_command_is_truncated() {
        let long_cmd: String = "x".repeat(500);
        let payload = serde_json::json!({
            "tool_use_id": "tu_abc",
            "required_permission": "bash:execute",
            "tool_input": { "command": long_cmd },
        });
        let line = build_line("permission-request", &payload).unwrap();
        let v = parse(&line);
        let summary = v["summary"].as_str().unwrap();
        assert!(summary.ends_with('…'));
        assert!(summary.chars().count() < 500);
    }

    #[test]
    fn summarize_prefers_command_then_url_then_file_path() {
        // command wins
        let p = serde_json::json!({
            "tool_use_id": "x",
            "tool_input": { "command": "ls", "url": "http://x", "file_path": "/a" }
        });
        assert_eq!(
            build_line("permission-request", &p).map(|l| parse(&l)["summary"].as_str().map(str::to_owned)),
            Some(Some("ls".to_owned()))
        );

        // url is next
        let p = serde_json::json!({
            "tool_use_id": "x",
            "tool_input": { "url": "http://x", "file_path": "/a" }
        });
        assert_eq!(
            build_line("permission-request", &p).map(|l| parse(&l)["summary"].as_str().map(str::to_owned)),
            Some(Some("http://x".to_owned()))
        );

        // file_path as fallback
        let p = serde_json::json!({
            "tool_use_id": "x",
            "tool_input": { "file_path": "/a" }
        });
        assert_eq!(
            build_line("permission-request", &p).map(|l| parse(&l)["summary"].as_str().map(str::to_owned)),
            Some(Some("/a".to_owned()))
        );
    }

    #[test]
    fn append_line_creates_parent_dir_and_round_trips_content() {
        let tmp = tempfile::tempdir().unwrap();
        // Nest under a missing subdir to exercise the defensive `create_dir_all`.
        let path = tmp.path().join("nested").join("hook-events.jsonl");

        append_line(&path, "{\"kind\":\"turnStart\"}\n").unwrap();
        append_line(&path, "{\"kind\":\"toolStart\",\"toolUseId\":\"x\"}\n").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(serde_json::from_str::<serde_json::Value>(lines[0]).unwrap()["kind"], "turnStart");
        assert_eq!(serde_json::from_str::<serde_json::Value>(lines[1]).unwrap()["kind"], "toolStart");
    }

    #[test]
    fn append_line_lock_does_not_deadlock_when_invoked_serially() {
        // Sequential acquire/release — the lock must release on handle drop. Loop tightly to surface any lingering-handle bug.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hook-events.jsonl");
        for i in 0..16 {
            append_line(&path, &format!("{{\"kind\":\"turnStart\",\"i\":{i}}}\n")).unwrap();
        }
        let lines = std::fs::read_to_string(&path).unwrap();
        assert_eq!(lines.lines().count(), 16);
    }
}
