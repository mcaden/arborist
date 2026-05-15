//! Claude Code hook-events tailer.
//!
//! Claude does not publish a structured event stream the way Copilot's `events.jsonl` does. Instead it exposes a hook contract
//! (<https://code.claude.com/docs/en/hooks>) that fires shell commands at every interesting lifecycle point. We register one hook per event we care
//! about (`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest`, `UserPromptSubmit`, `Stop`, `SessionEnd`) pointing at the
//! `arborist-claude-hook` helper binary. The helper reads Claude's JSON payload from stdin and appends one structured line to
//! `<session_temp_dir>/hook-events.jsonl`. **This module is the tailer that consumes that file** and emits the same [`ActivityEvent`] variants the
//! Copilot events tailer emits, routed through the same `session://activity` channel.
//!
//! ## Wire schema (helper → tailer)
//!
//! The helper writes one JSON object per line. The schema is intentionally small and owned by Arborist (we control both sides):
//!
//! ```jsonl
//! {"kind":"turnStart"}
//! {"kind":"turnEnd"}
//! {"kind":"awaitingPermission","toolUseId":"tu_abc","toolName":"Bash","permissionKind":"bash:execute","summary":"git push"}
//! {"kind":"toolStart","toolUseId":"tu_abc","toolName":"Bash"}
//! {"kind":"toolEnd","toolUseId":"tu_abc","success":true}
//! {"kind":"sessionEnd"}
//! ```
//!
//! Unknown `kind` values are silently ignored so a future helper can add fields without crashing older tailers.
//!
//! ## State machine (vs. Copilot tailer)
//!
//! Most rules mirror [`crate::copilot_events`]: append-only file, polled at [`POLL_INTERVAL`], catch-up suppresses transient pairs and only emits
//! the current open state through [`emit_current_state`]. The two Claude-specific rules:
//!
//! 1. **`toolStart` resolves a matching `awaitingPermission`.** Claude does not
//!    emit an explicit "permission approved" event — when the user approves, the
//!    very next event is the tool's `PreToolUse`. The tailer detects the
//!    matching `toolUseId` and synthesizes a [`ActivityEvent::PermissionResolved`]
//!    `{ approved: true }` before the [`ActivityEvent::ToolStart`].
//! 2. **`turnEnd` cancels any still-open permission.** Claude has no event for a
//!    user-denied permission either; the turn just ends. On `turnEnd` the tailer
//!    drains any still-open `awaitingPermission` entries and emits a synthetic
//!    [`ActivityEvent::PermissionResolved`] `{ approved: false }` for each before
//!    the [`ActivityEvent::TurnEnd`], so the sidebar shield clears.
//!
//! `sessionEnd` is a hard reset — the tailer clears all in-memory state but emits no synthesized events (the session is going away anyway).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde::Deserialize;

use crate::activity::ActivityEvent;
use crate::types::SessionId;

/// Polling cadence. Kept in line with [`crate::copilot_events::POLL_INTERVAL`] so the two watchers tick on the same rhythm.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Path to the per-session hook-events JSONL file. Lives under the session temp dir alongside `claude-settings.json`. Deleted with the rest of the
/// session temp tree on close.
#[must_use]
pub fn hook_events_path(session_id: &SessionId) -> PathBuf {
    crate::compose::session_temp_dir(session_id).join(crate::session_temp::CLAUDE_HOOK_EVENTS_FILE_NAME)
}

/// Validate that the per-session `claude-settings.json` at `settings_path` still references a helper-binary command path that exists on disk **in
/// the current process**. Used as the activity-events gate so a stale persisted settings file (app moved or updated since the session was last
/// spawned, partial install, packaging regression) doesn't park a polling thread on a `hook-events.jsonl` that will never be written to.
///
/// Identifies the Arborist-owned hook entry by `args[2]` matching the expected per-session events path — Arborist always writes the hook's args
/// as `[<event-kebab>, <session-uuid>, <events-jsonl-path>]`, so any entry whose third arg matches this session's `hook_events_path` is ours
/// (even after the user's settings are merged in alongside). Returns `false` on missing file, parse error, no matching entry, or a matching entry
/// whose `command` path doesn't exist as a file.
#[must_use]
pub fn settings_file_references_existing_helper(settings_path: &std::path::Path, session_id: &SessionId) -> bool {
    let Ok(bytes) = std::fs::read(settings_path) else {
        return false;
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    let Some(hooks) = json.get("hooks").and_then(|h| h.as_object()) else {
        return false;
    };
    let expected_events_path = hook_events_path(session_id);
    let expected_events_str = expected_events_path.to_string_lossy();

    for entries in hooks.values() {
        let Some(arr) = entries.as_array() else {
            continue;
        };
        for entry in arr {
            let Some(hook_list) = entry.get("hooks").and_then(|h| h.as_array()) else {
                continue;
            };
            for hook in hook_list {
                let is_ours = hook
                    .get("args")
                    .and_then(|a| a.as_array())
                    .and_then(|a| a.get(2))
                    .and_then(|v| v.as_str())
                    .map(|s| s == expected_events_str)
                    .unwrap_or(false);
                if is_ours {
                    if let Some(cmd) = hook.get("command").and_then(|c| c.as_str()) {
                        return std::path::Path::new(cmd).is_file();
                    }
                }
            }
        }
    }
    false
}

/// Callback shape mirrors [`crate::pty_pool::ActivityCb`] so production can wire the same emitter that already broadcasts on `session://activity`.
pub type ClaudeActivityCb = Arc<dyn Fn(&SessionId, ActivityEvent) + Send + Sync>;

/// Per-session tailer state. Public for unit tests; lives entirely on the watcher thread otherwise.
#[derive(Debug, Default)]
pub struct EventsState {
    /// Are we inside a `turnStart` ... `turnEnd` bracket?
    in_turn: bool,
    /// Open tool calls keyed by `toolUseId`.
    open_tools: HashMap<String, ToolInfo>,
    /// Open permission requests keyed by `toolUseId` (Claude reuses the tool-use id as the permission identifier).
    open_permissions: HashMap<String, PermissionInfo>,
}

#[derive(Debug, Clone)]
struct ToolInfo {
    name: String,
}

#[derive(Debug, Clone)]
struct PermissionInfo {
    kind: String,
    summary: Option<String>,
}

impl EventsState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Wire envelope. We control both producer and consumer, so the schema is flat and field names match the JSON exactly.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct HookLine<'a> {
    kind: &'a str,
    #[serde(default, borrow)]
    tool_use_id: Option<&'a str>,
    #[serde(default, borrow)]
    tool_name: Option<&'a str>,
    #[serde(default, borrow)]
    permission_kind: Option<&'a str>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    success: Option<bool>,
}

/// Process a single hook-events.jsonl line. Pure — no I/O, no clock. The closure receives every [`ActivityEvent`] this line should produce given the
/// current state.
///
/// `suppress_resolved` mirrors [`crate::copilot_events::ingest_line`]: when `true` we still update internal state for pairs that resolve within the
/// replay (so open counts stay correct) but emit nothing. Set `true` during catch-up and `false` thereafter.
pub fn ingest_line<F: FnMut(ActivityEvent)>(state: &mut EventsState, line: &[u8], suppress_resolved: bool, mut emit: F) {
    let env = match serde_json::from_slice::<HookLine<'_>>(line) {
        Ok(e) => e,
        Err(_) => return, // malformed — skip silently for forward compat
    };

    match env.kind {
        "turnStart" => {
            // Replace stale (un-paired) turn state if needed; only emit the transition.
            let was_in_turn = state.in_turn;
            state.in_turn = true;
            if !was_in_turn && !suppress_resolved {
                emit(ActivityEvent::TurnStart);
            }
        }
        "turnEnd" => {
            // Always drain `open_permissions` on `turnEnd`, regardless of `in_turn`. A `turnEnd` arriving outside a recorded turn (missing /
            // dropped `UserPromptSubmit` hook fire, late-attached watcher whose catch-up reset state, helper restart) would otherwise leave the
            // sidebar shield stuck indefinitely because `awaitingPermission` is accepted regardless of turn state on its own arm. The `TurnEnd`
            // event itself is only emitted when we *were* in a turn — we don't synthesize lifecycle markers we never claimed to be in.
            let was_in_turn = state.in_turn;
            state.in_turn = false;
            // Drain any still-open permission requests as denied — Claude has no explicit "denied" event when the user dismisses a prompt, so the
            // safe interpretation is "the turn ended without approval". Emit *before* TurnEnd so the frontend reducer sees the cleanup first and
            // doesn't display a dangling shield after the turn flips to awaiting.
            if !suppress_resolved {
                let drained: Vec<String> = state.open_permissions.keys().cloned().collect();
                for request_id in drained {
                    state.open_permissions.remove(&request_id);
                    emit(ActivityEvent::PermissionResolved { request_id, approved: false });
                }
                if was_in_turn {
                    emit(ActivityEvent::TurnEnd { duration_ms: None });
                }
            } else {
                // Catch-up phase: don't emit, but still keep the maps consistent.
                state.open_permissions.clear();
            }
        }
        "awaitingPermission" => {
            let Some(request_id) = env.tool_use_id else {
                return;
            };
            let kind = env
                .permission_kind
                .map(str::to_owned)
                .or_else(|| env.tool_name.map(str::to_owned))
                .unwrap_or_else(|| "permission".to_owned());
            let summary = env.summary.clone();
            state.open_permissions.insert(
                request_id.to_owned(),
                PermissionInfo {
                    kind: kind.clone(),
                    summary: summary.clone(),
                },
            );
            if !suppress_resolved {
                emit(ActivityEvent::AwaitingPermission {
                    request_id: request_id.to_owned(),
                    permission_kind: kind,
                    summary,
                });
            }
        }
        "toolStart" => {
            let Some(tool_call_id) = env.tool_use_id else {
                return;
            };
            let tool_name = env.tool_name.unwrap_or("tool").to_owned();
            // If the same tool_use_id had an open permission request, the user just approved it. Emit PermissionResolved(approved=true) before
            // ToolStart so the frontend sees the shield clear before the gear lights up.
            let resolved_perm = state.open_permissions.remove(tool_call_id).is_some();
            state.open_tools.insert(tool_call_id.to_owned(), ToolInfo { name: tool_name.clone() });
            if !suppress_resolved {
                if resolved_perm {
                    emit(ActivityEvent::PermissionResolved {
                        request_id: tool_call_id.to_owned(),
                        approved: true,
                    });
                }
                emit(ActivityEvent::ToolStart {
                    tool_call_id: tool_call_id.to_owned(),
                    tool_name,
                });
            }
        }
        "toolEnd" => {
            let Some(tool_call_id) = env.tool_use_id else {
                return;
            };
            let was_open = state.open_tools.remove(tool_call_id).is_some();
            // Defensive: drop a tool_use_id we never saw a start for (catch-up began mid-file).
            if was_open && !suppress_resolved {
                let success = env.success.unwrap_or(true);
                emit(ActivityEvent::ToolEnd {
                    tool_call_id: tool_call_id.to_owned(),
                    success,
                });
            }
        }
        "sessionEnd" => {
            // Hard reset. Don't emit synthesized cleanup events here — the session is being torn down, the frontend reducer drops everything on
            // status transitions away from `Running` anyway, and emitting a flurry of fake resolutions would be noise.
            state.in_turn = false;
            state.open_tools.clear();
            state.open_permissions.clear();
        }
        _ => {
            // Unknown kinds are silently ignored (forward compat with newer hook helpers).
        }
    }
}

/// Emit the *current* open state as a synthesized event after catch-up, so the sidebar reflects reality even when the watcher started mid-file.
/// Priority order matches the frontend selector: `AwaitingPermission` > `ToolStart` > `TurnStart`.
pub fn emit_current_state<F: FnMut(ActivityEvent)>(state: &EventsState, mut emit: F) {
    if let Some((req_id, info)) = state.open_permissions.iter().next() {
        emit(ActivityEvent::AwaitingPermission {
            request_id: req_id.clone(),
            permission_kind: info.kind.clone(),
            summary: info.summary.clone(),
        });
        return;
    }
    if let Some((tcid, info)) = state.open_tools.iter().next() {
        emit(ActivityEvent::ToolStart {
            tool_call_id: tcid.clone(),
            tool_name: info.name.clone(),
        });
        return;
    }
    if state.in_turn {
        emit(ActivityEvent::TurnStart);
    }
}

/// Run the hook-events.jsonl tailer for one Claude session. Blocking — call from a dedicated OS thread (see [`spawn_watcher`]).
///
/// `running` is checked at the top of each poll iteration; flipping it to `false` stops the watcher within at most one [`POLL_INTERVAL`].
///
/// Mirrors [`crate::copilot_events::run_watcher`] structure: per-iteration metadata snapshot, catch-up target aligned to the last `\n`, suppressed
/// emissions during catch-up, live emissions thereafter, per-id dedup so a rotate-and-replay doesn't double-count.
pub fn run_watcher(session_id: SessionId, events_path: PathBuf, emit: ClaudeActivityCb, running: Arc<AtomicBool>) {
    let mut state = EventsState::new();
    let mut cursor: u64 = 0;
    let mut catch_up_done = false;
    let mut catch_up_target: Option<u64> = None;
    let mut announced_tools: HashSet<String> = HashSet::new();
    let mut announced_perms: HashSet<String> = HashSet::new();

    while running.load(Ordering::SeqCst) {
        if let Ok(meta) = std::fs::metadata(&events_path) {
            let len = meta.len();
            if len < cursor {
                cursor = 0;
                state = EventsState::new();
                announced_tools.clear();
                announced_perms.clear();
                catch_up_done = false;
                catch_up_target = None;
            }
            if !catch_up_done && catch_up_target.is_none() {
                catch_up_target = Some(crate::copilot_events::align_snapshot_to_line_boundary(&events_path, len));
            }
            let read_end = if catch_up_done {
                len
            } else {
                std::cmp::min(len, catch_up_target.unwrap_or(len))
            };
            if read_end > cursor {
                let suppress = !catch_up_done;
                cursor = crate::session_metrics::tail_lines_pub(&events_path, cursor, read_end, |line| {
                    ingest_line(&mut state, line, suppress, |ev| {
                        let should_emit = match &ev {
                            ActivityEvent::ToolStart { tool_call_id, .. } => announced_tools.insert(tool_call_id.clone()),
                            ActivityEvent::ToolEnd { tool_call_id, .. } => {
                                announced_tools.remove(tool_call_id);
                                true
                            }
                            ActivityEvent::AwaitingPermission { request_id, .. } => announced_perms.insert(request_id.clone()),
                            ActivityEvent::PermissionResolved { request_id, .. } => {
                                announced_perms.remove(request_id);
                                true
                            }
                            _ => true,
                        };
                        if should_emit {
                            emit(&session_id, ev);
                        }
                    });
                });
            }
            if !catch_up_done {
                if let Some(target) = catch_up_target {
                    if cursor >= target {
                        catch_up_done = true;
                        emit_current_state(&state, |ev| {
                            match &ev {
                                ActivityEvent::ToolStart { tool_call_id, .. } => {
                                    announced_tools.insert(tool_call_id.clone());
                                }
                                ActivityEvent::AwaitingPermission { request_id, .. } => {
                                    announced_perms.insert(request_id.clone());
                                }
                                _ => {}
                            }
                            emit(&session_id, ev);
                        });
                    }
                }
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Spawn a dedicated OS thread running [`run_watcher`].
pub fn spawn_watcher(
    session_id: SessionId,
    events_path: PathBuf,
    emit: ClaudeActivityCb,
    running: Arc<AtomicBool>,
) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("arborist-claude-hooks-{}", session_id))
        .spawn(move || run_watcher(session_id, events_path, emit, running))
}

// --------------------------------------------------------------------------- Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(state: &mut EventsState, lines: &[&[u8]], suppress: bool) -> Vec<ActivityEvent> {
        let mut out = Vec::new();
        for l in lines {
            ingest_line(state, l, suppress, |ev| out.push(ev));
        }
        out
    }

    #[test]
    fn turn_start_then_end_emits_paired_events() {
        let mut s = EventsState::new();
        let evs = collect(&mut s, &[br#"{"kind":"turnStart"}"#, br#"{"kind":"turnEnd"}"#], false);
        assert_eq!(evs, vec![ActivityEvent::TurnStart, ActivityEvent::TurnEnd { duration_ms: None }]);
        assert!(!s.in_turn);
    }

    #[test]
    fn permission_then_matching_tool_start_emits_resolved_then_start() {
        let mut s = EventsState::new();
        let evs = collect(
            &mut s,
            &[
                br#"{"kind":"awaitingPermission","toolUseId":"tu1","toolName":"Bash","permissionKind":"bash:execute","summary":"git push"}"#,
                br#"{"kind":"toolStart","toolUseId":"tu1","toolName":"Bash"}"#,
            ],
            false,
        );
        assert_eq!(
            evs,
            vec![
                ActivityEvent::AwaitingPermission {
                    request_id: "tu1".into(),
                    permission_kind: "bash:execute".into(),
                    summary: Some("git push".into()),
                },
                ActivityEvent::PermissionResolved {
                    request_id: "tu1".into(),
                    approved: true,
                },
                ActivityEvent::ToolStart {
                    tool_call_id: "tu1".into(),
                    tool_name: "Bash".into(),
                },
            ]
        );
        assert!(s.open_permissions.is_empty());
        assert_eq!(s.open_tools.len(), 1);
    }

    #[test]
    fn tool_start_without_prior_permission_just_emits_start() {
        let mut s = EventsState::new();
        let evs = collect(&mut s, &[br#"{"kind":"toolStart","toolUseId":"tu1","toolName":"Read"}"#], false);
        assert_eq!(
            evs,
            vec![ActivityEvent::ToolStart {
                tool_call_id: "tu1".into(),
                tool_name: "Read".into(),
            }]
        );
    }

    #[test]
    fn tool_end_emits_with_default_success_when_field_missing() {
        let mut s = EventsState::new();
        let _ = collect(&mut s, &[br#"{"kind":"toolStart","toolUseId":"tu1","toolName":"Read"}"#], false);
        let evs = collect(&mut s, &[br#"{"kind":"toolEnd","toolUseId":"tu1"}"#], false);
        assert_eq!(
            evs,
            vec![ActivityEvent::ToolEnd {
                tool_call_id: "tu1".into(),
                success: true,
            }]
        );
    }

    #[test]
    fn tool_end_with_success_false_marks_failure() {
        let mut s = EventsState::new();
        let _ = collect(&mut s, &[br#"{"kind":"toolStart","toolUseId":"tu1","toolName":"Bash"}"#], false);
        let evs = collect(&mut s, &[br#"{"kind":"toolEnd","toolUseId":"tu1","success":false}"#], false);
        assert_eq!(
            evs,
            vec![ActivityEvent::ToolEnd {
                tool_call_id: "tu1".into(),
                success: false,
            }]
        );
    }

    #[test]
    fn tool_end_without_matching_start_is_dropped() {
        let mut s = EventsState::new();
        let evs = collect(&mut s, &[br#"{"kind":"toolEnd","toolUseId":"tu1","success":true}"#], false);
        assert!(evs.is_empty());
    }

    #[test]
    fn turn_end_clears_dangling_permission_as_denied() {
        let mut s = EventsState::new();
        let _ = collect(
            &mut s,
            &[
                br#"{"kind":"turnStart"}"#,
                br#"{"kind":"awaitingPermission","toolUseId":"tuX","toolName":"Bash","permissionKind":"bash:execute","summary":"rm -rf /"}"#,
            ],
            false,
        );
        let evs = collect(&mut s, &[br#"{"kind":"turnEnd"}"#], false);
        // PermissionResolved(approved=false) is emitted BEFORE TurnEnd so the frontend reducer clears the shield first.
        assert_eq!(
            evs,
            vec![
                ActivityEvent::PermissionResolved {
                    request_id: "tuX".into(),
                    approved: false,
                },
                ActivityEvent::TurnEnd { duration_ms: None },
            ]
        );
        assert!(s.open_permissions.is_empty());
    }

    #[test]
    fn turn_end_outside_turn_still_drains_dangling_permissions() {
        // Regression: a `turnEnd` arriving without a preceding `turnStart` (dropped UserPromptSubmit hook fire, helper restart, late watcher
        // attach) used to early-return and leave the permission shield stuck. Now it always drains `open_permissions`; only the `TurnEnd` event
        // itself is gated on whether we were in a turn.
        let mut s = EventsState::new();
        // Open a permission without a turnStart first.
        let _ = collect(
            &mut s,
            &[br#"{"kind":"awaitingPermission","toolUseId":"tuOrphan","toolName":"Bash","permissionKind":"bash:execute","summary":"ls"}"#],
            false,
        );
        assert!(!s.in_turn, "precondition: no turn recorded");
        assert_eq!(s.open_permissions.len(), 1, "precondition: orphan permission present");

        let evs = collect(&mut s, &[br#"{"kind":"turnEnd"}"#], false);
        // Drain happens; TurnEnd is suppressed because we never claimed to be in a turn.
        assert_eq!(
            evs,
            vec![ActivityEvent::PermissionResolved {
                request_id: "tuOrphan".into(),
                approved: false,
            }]
        );
        assert!(s.open_permissions.is_empty(), "shield must be cleared");
    }

    #[test]
    fn permission_kind_falls_back_to_tool_name_then_default() {
        let mut s = EventsState::new();
        // No permissionKind, but toolName present.
        let evs = collect(&mut s, &[br#"{"kind":"awaitingPermission","toolUseId":"tuA","toolName":"Edit"}"#], false);
        assert_eq!(
            evs[0],
            ActivityEvent::AwaitingPermission {
                request_id: "tuA".into(),
                permission_kind: "Edit".into(),
                summary: None,
            }
        );

        // Neither permissionKind nor toolName.
        let mut s2 = EventsState::new();
        let evs2 = collect(&mut s2, &[br#"{"kind":"awaitingPermission","toolUseId":"tuB"}"#], false);
        assert_eq!(
            evs2[0],
            ActivityEvent::AwaitingPermission {
                request_id: "tuB".into(),
                permission_kind: "permission".into(),
                summary: None,
            }
        );
    }

    #[test]
    fn session_end_resets_state() {
        let mut s = EventsState::new();
        let _ = collect(
            &mut s,
            &[
                br#"{"kind":"turnStart"}"#,
                br#"{"kind":"awaitingPermission","toolUseId":"tu1","toolName":"Bash"}"#,
                br#"{"kind":"toolStart","toolUseId":"tu2","toolName":"Read"}"#,
            ],
            false,
        );
        let evs = collect(&mut s, &[br#"{"kind":"sessionEnd"}"#], false);
        assert!(evs.is_empty(), "sessionEnd should not emit synthesized cleanup events");
        assert!(!s.in_turn);
        assert!(s.open_tools.is_empty());
        assert!(s.open_permissions.is_empty());
    }

    #[test]
    fn malformed_line_does_not_panic() {
        let mut s = EventsState::new();
        let evs = collect(&mut s, &[b"not json", b"{\"kind\":\"turnStart\"", b""], false);
        assert!(evs.is_empty());
        assert!(!s.in_turn);
    }

    #[test]
    fn unknown_kind_is_silently_ignored() {
        let mut s = EventsState::new();
        let evs = collect(
            &mut s,
            &[br#"{"kind":"someFutureEvent","toolUseId":"tu1"}"#, br#"{"kind":"diagnostic","x":1}"#],
            false,
        );
        assert!(evs.is_empty());
    }

    #[test]
    fn catch_up_suppresses_resolved_pairs_and_emits_only_open_state() {
        let mut s = EventsState::new();
        let suppressed = collect(
            &mut s,
            &[
                br#"{"kind":"turnStart"}"#,
                br#"{"kind":"toolStart","toolUseId":"tu1","toolName":"Read"}"#,
                br#"{"kind":"toolEnd","toolUseId":"tu1","success":true}"#,
                br#"{"kind":"turnEnd"}"#,
                br#"{"kind":"turnStart"}"#,
                br#"{"kind":"awaitingPermission","toolUseId":"tuOpen","toolName":"Bash","permissionKind":"bash:execute","summary":"git pull"}"#,
            ],
            true,
        );
        assert!(suppressed.is_empty(), "catch-up must not emit transient events; got {suppressed:?}");
        let mut out = Vec::new();
        emit_current_state(&s, |ev| out.push(ev));
        assert_eq!(
            out,
            vec![ActivityEvent::AwaitingPermission {
                request_id: "tuOpen".into(),
                permission_kind: "bash:execute".into(),
                summary: Some("git pull".into()),
            }]
        );
    }

    #[test]
    fn catch_up_with_open_turn_only_emits_turn_start() {
        let mut s = EventsState::new();
        let _ = collect(&mut s, &[br#"{"kind":"turnStart"}"#], true);
        let mut out = Vec::new();
        emit_current_state(&s, |ev| out.push(ev));
        assert_eq!(out, vec![ActivityEvent::TurnStart]);
    }

    #[test]
    fn catch_up_quiescent_emits_nothing() {
        let mut s = EventsState::new();
        let _ = collect(&mut s, &[br#"{"kind":"turnStart"}"#, br#"{"kind":"turnEnd"}"#], true);
        let mut out = Vec::new();
        emit_current_state(&s, |ev| out.push(ev));
        assert!(out.is_empty());
    }

    #[test]
    fn hook_events_path_lives_under_session_temp_dir() {
        let sid = SessionId::new();
        let p = hook_events_path(&sid);
        assert!(p.ends_with("hook-events.jsonl"));
        assert!(p.parent().unwrap().ends_with(sid.0.to_string()));
    }

    // ---- Gate validation for the activity-events watcher (`settings_file_references_existing_helper`).

    fn write_settings_with_helper(tmp: &tempfile::TempDir, helper_cmd: &str, sid: &SessionId) -> std::path::PathBuf {
        let events = hook_events_path(sid);
        let body = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "",
                    "hooks": [{ "type": "command", "command": helper_cmd, "args": ["pre-tool-use", sid.0.to_string(), events.to_string_lossy()] }],
                }],
            }
        });
        let p = tmp.path().join("claude-settings.json");
        std::fs::write(&p, serde_json::to_string(&body).unwrap()).unwrap();
        p
    }

    #[test]
    fn settings_validation_passes_when_helper_command_exists() {
        let tmp = tempfile::tempdir().unwrap();
        // Pretend the helper is an existing file (use the temp dir itself as a stand-in real file).
        let fake_helper = tmp.path().join("arborist-claude-hook");
        std::fs::write(&fake_helper, b"#!/bin/sh\n").unwrap();
        let sid = SessionId::new();
        let settings = write_settings_with_helper(&tmp, &fake_helper.to_string_lossy(), &sid);
        assert!(settings_file_references_existing_helper(&settings, &sid));
    }

    #[test]
    fn settings_validation_fails_when_helper_command_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SessionId::new();
        let settings = write_settings_with_helper(&tmp, "/nonexistent/path/to/arborist-claude-hook", &sid);
        assert!(!settings_file_references_existing_helper(&settings, &sid));
    }

    #[test]
    fn settings_validation_fails_when_settings_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SessionId::new();
        assert!(!settings_file_references_existing_helper(&tmp.path().join("does-not-exist.json"), &sid));
    }

    #[test]
    fn settings_validation_fails_when_settings_file_unparseable() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("claude-settings.json");
        std::fs::write(&path, b"not json").unwrap();
        let sid = SessionId::new();
        assert!(!settings_file_references_existing_helper(&path, &sid));
    }

    #[test]
    fn settings_validation_ignores_user_hook_entries_with_unrelated_args() {
        // A user-added PreToolUse hook (different args, helper doesn't match our shape) must not satisfy the gate even if its `command` happens to
        // exist on disk. We require the args[2] events-path-match to identify *our* entry.
        let tmp = tempfile::tempdir().unwrap();
        let user_helper = tmp.path().join("user-formatter");
        std::fs::write(&user_helper, b"echo").unwrap();
        let sid = SessionId::new();
        let body = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{ "type": "command", "command": user_helper.to_string_lossy() }],
                }],
            }
        });
        let path = tmp.path().join("claude-settings.json");
        std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
        assert!(!settings_file_references_existing_helper(&path, &sid));
    }

    // ---- End-to-end watcher harness — drives a real file from a background thread, asserts the emitted events.

    use std::io::Write;
    use std::sync::Mutex;

    fn drain<F: Fn(&Mutex<Vec<(SessionId, ActivityEvent)>>) -> bool>(
        bag: &Arc<Mutex<Vec<(SessionId, ActivityEvent)>>>,
        cond: F,
        max_wait: Duration,
    ) -> Vec<(SessionId, ActivityEvent)> {
        let start = std::time::Instant::now();
        loop {
            if cond(bag) {
                return bag.lock().unwrap().clone();
            }
            if start.elapsed() > max_wait {
                return bag.lock().unwrap().clone();
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn run_watcher_emits_live_events_after_catch_up() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hook-events.jsonl");
        // Pre-seed with a complete (resolved) history — catch-up should emit nothing for it.
        std::fs::write(&path, b"{\"kind\":\"turnStart\"}\n{\"kind\":\"turnEnd\"}\n").unwrap();

        let bag = Arc::new(Mutex::new(Vec::new()));
        let bag_for_cb = Arc::clone(&bag);
        let emit: ClaudeActivityCb = Arc::new(move |sid, ev| {
            bag_for_cb.lock().unwrap().push((*sid, ev));
        });

        let session_id = SessionId::new();
        let running = Arc::new(AtomicBool::new(true));
        let r = Arc::clone(&running);
        let p = path.clone();
        let join = thread::spawn(move || run_watcher(session_id, p, emit, r));

        thread::sleep(POLL_INTERVAL * 3);
        assert!(bag.lock().unwrap().is_empty(), "catch-up over a fully-resolved history must emit nothing");

        // Append a live permission request.
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"kind\":\"awaitingPermission\",\"toolUseId\":\"tuLive\",\"toolName\":\"Bash\",\"permissionKind\":\"bash:execute\",\"summary\":\"ls\"}\n")
            .unwrap();
        drop(f);

        let got = drain(&bag, |b| !b.lock().unwrap().is_empty(), Duration::from_secs(5));
        running.store(false, Ordering::SeqCst);
        let _ = join.join();

        assert!(
            got.iter()
                .any(|(_, ev)| matches!(ev, ActivityEvent::AwaitingPermission { request_id, .. } if request_id == "tuLive")),
            "expected live AwaitingPermission emission, got {got:?}",
        );
    }

    #[test]
    fn run_watcher_catch_up_synthesizes_pending_permission() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hook-events.jsonl");
        std::fs::write(
            &path,
            b"{\"kind\":\"turnStart\"}\n\
              {\"kind\":\"awaitingPermission\",\"toolUseId\":\"tuPending\",\"toolName\":\"Bash\",\"permissionKind\":\"bash:execute\",\"summary\":\"git pull\"}\n",
        )
        .unwrap();

        let bag = Arc::new(Mutex::new(Vec::new()));
        let bag_for_cb = Arc::clone(&bag);
        let emit: ClaudeActivityCb = Arc::new(move |sid, ev| {
            bag_for_cb.lock().unwrap().push((*sid, ev));
        });

        let session_id = SessionId::new();
        let running = Arc::new(AtomicBool::new(true));
        let r = Arc::clone(&running);
        let p = path.clone();
        let join = thread::spawn(move || run_watcher(session_id, p, emit, r));

        let got = drain(&bag, |b| !b.lock().unwrap().is_empty(), Duration::from_secs(5));
        running.store(false, Ordering::SeqCst);
        let _ = join.join();

        let perms: Vec<_> = got
            .iter()
            .filter(|(_, ev)| matches!(ev, ActivityEvent::AwaitingPermission { .. }))
            .collect();
        assert_eq!(perms.len(), 1, "expected exactly one synthesized AwaitingPermission, got {got:?}");
    }
}
