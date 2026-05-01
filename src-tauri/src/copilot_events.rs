//! Copilot CLI `events.jsonl` tailer (Issue #_richer_states_).
//!
//! Copilot writes a structured, append-only event stream to
//! `~/.copilot/session-state/<sessionId>/events.jsonl`. The tailer turns
//! this stream into [`crate::activity::ActivityEvent`]s the frontend's
//! sidebar state machine consumes via `session://activity`. We surface
//! three new sub-states the byte-rate scanner can't tell apart:
//!
//! - **`AwaitingPermission`** — the agent is *blocked on the user* (e.g.
//!   waiting for shell-command approval). This is the single most
//!   actionable cue we can give about a sidebar tab.
//! - **`ToolStart` / `ToolEnd`** — the agent is busy running a tool, not
//!   generating tokens. Tooltips can name the tool.
//! - **`TurnStart` / `TurnEnd`** — bracket "the model is generating".
//!
//! The tailer is **Copilot-only** by design: Claude's transcript JSONL
//! doesn't carry tool/permission events on the same schema. Until Claude
//! exposes hooks (ROADMAP §4.5 / Issue #4), Claude tabs continue using
//! the PTY-byte [`crate::activity::ActivityScanner`].
//!
//! ## Why path-by-session-id (not directory watch)?
//!
//! Pre-allocation (DESIGN §5.4) decides the conversation uuid at
//! `session_create`, splices it via `--resume <uuid>` at every spawn,
//! and persists it as `Session.ai_session_id`. So
//! `~/.copilot/session-state/<ai_session_id>/events.jsonl` is the
//! deterministic path from spawn second 0 — no directory-scan heuristic
//! needed.
//!
//! ## State machine and ordering tolerance
//!
//! Events are append-only and time-ordered, but the tailer **must not**
//! crash on:
//! - Unknown `type` values (Copilot adds new event kinds across versions).
//! - Missing or malformed `data.*` fields (defensive — schema isn't
//!   public-API stable).
//! - A `tool.execution_complete` arriving with no matching `_start` (we
//!   started tailing mid-file).
//! - File rotation / truncation under us (rare; same handling as
//!   [`crate::session_metrics`]).
//!
//! ## Catch-up on tail-start
//!
//! When the watcher starts after a restart (events.jsonl already exists
//! with prior content), we replay the file from byte 0, applying the
//! same state transitions but **suppressing** transient
//! `ToolStart→ToolEnd` and `AwaitingPermission→PermissionResolved`
//! pairs that have already resolved. After catch-up, only the *currently
//! open* state is emitted (a single `AwaitingPermission`, a final
//! `ToolStart`, or `TurnStart` if still in a turn). This keeps the UI
//! quiet during restore.
//!
//! ## Known limitation: `/clear` mid-session
//!
//! Copilot's `/clear` rotates the conversation to a *new* uuid and
//! starts writing to a fresh `events.jsonl` under the new path. The
//! sibling [`crate::session_metrics`] OTel watcher detects the new
//! `gen_ai.conversation.id` and persists it to
//! `Session.ai_session_id`, but **this watcher does not hot-swap its
//! events path** — it stays bound to the original uuid passed to
//! [`spawn_watcher`]. After `/clear`, sidebar sub-states
//! (awaiting-permission / running-tool / thinking) for that session
//! reflect the last known state of the *old* conversation until the
//! user issues a `session_restart` (which tears down and re-spawns the
//! watcher with the rotated id). Token / model / duration metrics
//! continue to update because the OTel watcher is conversation-id
//! agnostic. Tracked as a follow-up; out of scope for the initial
//! Phase 2.5 surface.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde::Deserialize;

use crate::activity::ActivityEvent;
use crate::types::SessionId;

/// Poll interval — kept in line with [`crate::session_metrics`] so the
/// two watchers tick on the same cadence and don't fight for the disk.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Path to the per-session events.jsonl given a Copilot conversation id.
/// Public so the production caller in `commands::session` can compute
/// it at watcher-start time without re-implementing the layout.
#[must_use]
pub fn events_path(home: &std::path::Path, ai_session_id: &str) -> PathBuf {
    home.join(".copilot")
        .join("session-state")
        .join(ai_session_id)
        .join("events.jsonl")
}

/// Callback shape mirrors [`crate::pty_pool::ActivityCb`] so production
/// can wire the same emitter that already broadcasts on
/// `session://activity`.
pub type CopilotActivityCb = Arc<dyn Fn(&SessionId, ActivityEvent) + Send + Sync>;

/// Per-session tailer state. Public for unit tests; lives entirely on
/// the watcher thread otherwise.
#[derive(Debug, Default)]
pub struct EventsState {
    /// Are we inside an `assistant.turn_start` ... `assistant.turn_end`
    /// bracket? Tracked as a turn id (Some(_) = in turn) rather than a
    /// boolean so we can match start/end pairs and skip stray ends.
    in_turn: Option<String>,
    /// Open tool calls keyed by `toolCallId`.
    open_tools: HashMap<String, ToolInfo>,
    /// Open permission requests keyed by `requestId`.
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

/// Minimal envelope shared across every event type in events.jsonl. Every
/// concrete event has at least `type` and (optionally) `data`. Unknown
/// type values are silently skipped — the tailer is forward-compatible
/// with new Copilot CLI versions.
#[derive(Deserialize, Debug)]
struct Envelope<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(default)]
    data: Option<serde_json::Value>,
}

/// Process a single events.jsonl line. The closure is invoked with each
/// [`ActivityEvent`] that the line should produce *given current state*.
/// Pure (no I/O, no clock) so unit tests can drive synthetic histories.
///
/// `suppress_resolved` controls catch-up behavior: when `true`, we still
/// update internal state for pairs that resolve within the replay (so we
/// don't end up with bogus open counts), but we don't emit transient
/// events for them. Set `true` during the initial mid-file catch-up read
/// and `false` once we're tailing live.
pub fn ingest_line<F: FnMut(ActivityEvent)>(
    state: &mut EventsState,
    line: &[u8],
    suppress_resolved: bool,
    mut emit: F,
) {
    let env = match serde_json::from_slice::<Envelope<'_>>(line) {
        Ok(e) => e,
        Err(_) => return, // malformed line — skip silently
    };

    match env.kind {
        "assistant.turn_start" => {
            let turn_id = env
                .data
                .as_ref()
                .and_then(|d| d.get("turnId"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            // Stale (un-paired) turn from prior content? Replace.
            let was_in_turn = state.in_turn.is_some();
            state.in_turn = Some(turn_id);
            if !was_in_turn && !suppress_resolved {
                emit(ActivityEvent::TurnStart);
            }
        }
        "assistant.turn_end" => {
            // The metrics watcher is the canonical source of TurnEnd-with-duration
            // (Copilot OTel `invoke_agent` span). We emit a TurnEnd here without a
            // duration so the frontend's reducer flips out of `thinking` promptly
            // even when OTel is delayed or missing.
            #[allow(clippy::collapsible_match)]
            if state.in_turn.take().is_some() && !suppress_resolved {
                emit(ActivityEvent::TurnEnd { duration_ms: None });
            }
        }
        "tool.execution_start" => {
            let data = match env.data.as_ref() {
                Some(d) => d,
                None => return,
            };
            let Some(tool_call_id) = data.get("toolCallId").and_then(|v| v.as_str()) else {
                return;
            };
            let tool_name = data
                .get("toolName")
                .and_then(|v| v.as_str())
                .unwrap_or("tool")
                .to_owned();
            state.open_tools.insert(
                tool_call_id.to_owned(),
                ToolInfo {
                    name: tool_name.clone(),
                },
            );
            if !suppress_resolved {
                emit(ActivityEvent::ToolStart {
                    tool_call_id: tool_call_id.to_owned(),
                    tool_name,
                });
            }
        }
        "tool.execution_complete" => {
            let data = match env.data.as_ref() {
                Some(d) => d,
                None => return,
            };
            let Some(tool_call_id) = data.get("toolCallId").and_then(|v| v.as_str()) else {
                return;
            };
            let was_open = state.open_tools.remove(tool_call_id).is_some();
            // If we never saw the matching start (e.g. catch-up began
            // mid-file after the start line), drop the end on the floor.
            // The frontend's reducer is also defensive but emitting a
            // bogus ToolEnd would decrement a counter that was never
            // incremented.
            if was_open && !suppress_resolved {
                let success = data
                    .get("success")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                emit(ActivityEvent::ToolEnd {
                    tool_call_id: tool_call_id.to_owned(),
                    success,
                });
            }
        }
        "permission.requested" => {
            let data = match env.data.as_ref() {
                Some(d) => d,
                None => return,
            };
            let Some(request_id) = data.get("requestId").and_then(|v| v.as_str()) else {
                return;
            };
            let kind = extract_permission_kind(data);
            let summary = extract_permission_summary(data);
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
        "permission.completed" => {
            let data = match env.data.as_ref() {
                Some(d) => d,
                None => return,
            };
            let Some(request_id) = data.get("requestId").and_then(|v| v.as_str()) else {
                return;
            };
            let was_open = state.open_permissions.remove(request_id).is_some();
            if was_open && !suppress_resolved {
                let approved = data
                    .get("result")
                    .and_then(|r| r.get("kind"))
                    .and_then(|k| k.as_str())
                    .map(|k| k.eq_ignore_ascii_case("approved") || k.eq_ignore_ascii_case("allow"))
                    .unwrap_or(false);
                emit(ActivityEvent::PermissionResolved {
                    request_id: request_id.to_owned(),
                    approved,
                });
            }
        }
        "abort" => {
            // User pressed ESC (or similar). Treat as turn-end + drop
            // any open tools — they will never complete via
            // tool.execution_complete because the agent was killed
            // mid-call. Permissions stay open; the user's resolution
            // (or lack thereof) is what produced the abort.
            if state.in_turn.is_some() {
                state.in_turn = None;
                if !suppress_resolved {
                    emit(ActivityEvent::TurnEnd { duration_ms: None });
                }
            }
            let drained: Vec<String> = state.open_tools.keys().cloned().collect();
            for id in drained {
                state.open_tools.remove(&id);
                if !suppress_resolved {
                    emit(ActivityEvent::ToolEnd {
                        tool_call_id: id,
                        success: false,
                    });
                }
            }
        }
        _ => {
            // Unknown event kinds (session.start, session.shutdown,
            // session.model_change, hook.*, system.message, ...) are
            // intentionally ignored at this layer. The metrics watcher
            // owns model-change handling; lifecycle is owned by the
            // PTY pool. New unknown kinds in future Copilot versions
            // are forward-compatible by default.
        }
    }
}

/// Emit the *current* open state as a synthesized event after catch-up,
/// so the sidebar reflects reality even when the watcher started
/// mid-file. Priority order matches the frontend selector:
/// `AwaitingPermission` > `ToolStart` > `TurnStart`.
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
    if state.in_turn.is_some() {
        emit(ActivityEvent::TurnStart);
    }
}

/// Extract the user-visible kind label for a permission request. The
/// Copilot schema isn't public; we accept several shapes seen in the
/// wild and fall back to `"permission"` for anything we don't recognise.
fn extract_permission_kind(data: &serde_json::Value) -> String {
    if let Some(s) = data
        .get("permissionRequest")
        .and_then(|p| p.get("kind"))
        .and_then(|v| v.as_str())
    {
        return s.to_owned();
    }
    if let Some(s) = data.get("kind").and_then(|v| v.as_str()) {
        return s.to_owned();
    }
    if let Some(s) = data.get("toolName").and_then(|v| v.as_str()) {
        return s.to_owned();
    }
    "permission".to_owned()
}

/// Extract a one-line summary for a permission request. Best-effort —
/// returns `None` if no shape we recognise is present.
fn extract_permission_summary(data: &serde_json::Value) -> Option<String> {
    let pr = data.get("permissionRequest").or(Some(data))?;
    if let Some(s) = pr.get("command").and_then(|v| v.as_str()) {
        return Some(s.to_owned());
    }
    if let Some(s) = pr.get("summary").and_then(|v| v.as_str()) {
        return Some(s.to_owned());
    }
    if let Some(s) = pr.get("description").and_then(|v| v.as_str()) {
        return Some(s.to_owned());
    }
    None
}

// ---------------------------------------------------------------------------
// File I/O — small wrapper that drives `ingest_line` against the on-disk
// events.jsonl. Mirrors `session_metrics::tail_lines` shape so behavior
// is consistent across watchers.
// ---------------------------------------------------------------------------

/// Run the events.jsonl tailer for one Copilot session. Blocking — call
/// from a dedicated OS thread (see [`spawn_watcher`]).
///
/// `running` is checked at the top of each poll iteration; flipping it
/// to `false` stops the watcher within at most one [`POLL_INTERVAL`].
pub fn run_watcher(
    session_id: SessionId,
    events_path: PathBuf,
    emit: CopilotActivityCb,
    running: Arc<AtomicBool>,
) {
    let mut state = EventsState::new();
    let mut cursor: u64 = 0;
    let mut catch_up_done = false;
    // EOF snapshot taken on the first poll iteration (before any reads).
    // Catch-up "done" means cursor has drained past this snapshot —
    // *not* "we read at least one chunk". This matters because
    // `tail_lines_pub` caps each read at `MAX_READ_CHUNK` (10 MB), so on
    // a large pre-existing events.jsonl a single iteration won't drain
    // the file. Without this guard, history past the first chunk would
    // be emitted with `suppress=false` and surface as spurious live
    // tool/permission flickers in the sidebar after restore.
    let mut catch_up_target: Option<u64> = None;
    // Track which tools / permissions we've *already announced* on the
    // wire, so a second emission after a rotate-and-replay doesn't
    // double-count. The frontend reducer is also defensive (idempotent on
    // matching ids), but local dedup avoids unnecessary cross-process
    // chatter.
    let mut announced_tools: HashSet<String> = HashSet::new();
    let mut announced_perms: HashSet<String> = HashSet::new();

    while running.load(Ordering::SeqCst) {
        if let Ok(meta) = std::fs::metadata(&events_path) {
            let len = meta.len();
            // Truncated/rotated under us: reset and re-snapshot. Same
            // handling as session_metrics's OTel watcher.
            if len < cursor {
                cursor = 0;
                state = EventsState::new();
                announced_tools.clear();
                announced_perms.clear();
                catch_up_done = false;
                catch_up_target = None;
            }
            // Snapshot the catch-up target on the first iteration that
            // sees the file. Subsequent iterations keep the original
            // target so events appended *after* catch-up started are
            // treated as live (emitted), not catch-up (suppressed).
            if !catch_up_done && catch_up_target.is_none() {
                catch_up_target = Some(len);
            }
            // During catch-up, cap the read end at the snapshot so any
            // bytes appended after we started are deferred to the live
            // phase. After catch-up flips, read up to the current EOF.
            let read_end = if catch_up_done {
                len
            } else {
                std::cmp::min(len, catch_up_target.unwrap_or(len))
            };
            if read_end > cursor {
                let suppress = !catch_up_done;
                cursor = crate::session_metrics::tail_lines_pub(
                    &events_path,
                    cursor,
                    read_end,
                    |line| {
                        ingest_line(&mut state, line, suppress, |ev| {
                            // Defensive de-dup on tool/permission ids
                            // for live-tail emissions only — the catch-up
                            // path doesn't emit at all (suppress=true).
                            let should_emit = match &ev {
                                ActivityEvent::ToolStart { tool_call_id, .. } => {
                                    announced_tools.insert(tool_call_id.clone())
                                }
                                ActivityEvent::ToolEnd { tool_call_id, .. } => {
                                    announced_tools.remove(tool_call_id);
                                    true
                                }
                                ActivityEvent::AwaitingPermission { request_id, .. } => {
                                    announced_perms.insert(request_id.clone())
                                }
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
                    },
                );
            }
            // Catch-up flips done only once cursor has drained past the
            // pre-read EOF snapshot. For empty / no-new-bytes files the
            // condition is trivially true (target == cursor == 0 or
            // cursor already past target).
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

/// Spawn a dedicated OS thread running [`run_watcher`]. Returns the
/// handle so callers can `join` for quiescence (mirrors
/// [`crate::session_metrics::MetricsRegistry::stop_and_join`]).
pub fn spawn_watcher(
    session_id: SessionId,
    events_path: PathBuf,
    emit: CopilotActivityCb,
    running: Arc<AtomicBool>,
) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("arborist-events-{}", session_id))
        .spawn(move || run_watcher(session_id, events_path, emit, running))
}

// ---------------------------------------------------------------------------
// Tests
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
        let evs = collect(
            &mut s,
            &[
                br#"{"type":"assistant.turn_start","data":{"turnId":"t1"}}"#,
                br#"{"type":"assistant.turn_end","data":{"turnId":"t1"}}"#,
            ],
            false,
        );
        assert_eq!(
            evs,
            vec![
                ActivityEvent::TurnStart,
                ActivityEvent::TurnEnd { duration_ms: None },
            ]
        );
        assert!(s.in_turn.is_none());
    }

    #[test]
    fn nested_tool_inside_turn_emits_in_order() {
        let mut s = EventsState::new();
        let evs = collect(
            &mut s,
            &[
                br#"{"type":"assistant.turn_start","data":{"turnId":"t1"}}"#,
                br#"{"type":"tool.execution_start","data":{"toolCallId":"c1","toolName":"powershell"}}"#,
                br#"{"type":"tool.execution_complete","data":{"toolCallId":"c1","success":true}}"#,
                br#"{"type":"assistant.turn_end","data":{"turnId":"t1"}}"#,
            ],
            false,
        );
        assert_eq!(
            evs,
            vec![
                ActivityEvent::TurnStart,
                ActivityEvent::ToolStart {
                    tool_call_id: "c1".into(),
                    tool_name: "powershell".into(),
                },
                ActivityEvent::ToolEnd {
                    tool_call_id: "c1".into(),
                    success: true,
                },
                ActivityEvent::TurnEnd { duration_ms: None },
            ]
        );
        assert!(s.open_tools.is_empty());
    }

    #[test]
    fn permission_requested_then_completed_emits_pair() {
        let mut s = EventsState::new();
        let evs = collect(
            &mut s,
            &[
                br#"{"type":"permission.requested","data":{"requestId":"p1","permissionRequest":{"kind":"shell","command":"git status"}}}"#,
                br#"{"type":"permission.completed","data":{"requestId":"p1","result":{"kind":"approved"}}}"#,
            ],
            false,
        );
        assert_eq!(
            evs,
            vec![
                ActivityEvent::AwaitingPermission {
                    request_id: "p1".into(),
                    permission_kind: "shell".into(),
                    summary: Some("git status".into()),
                },
                ActivityEvent::PermissionResolved {
                    request_id: "p1".into(),
                    approved: true,
                },
            ]
        );
        assert!(s.open_permissions.is_empty());
    }

    #[test]
    fn permission_completed_with_denied_result_marks_not_approved() {
        let mut s = EventsState::new();
        let evs = collect(
            &mut s,
            &[
                br#"{"type":"permission.requested","data":{"requestId":"p1","permissionRequest":{"kind":"shell"}}}"#,
                br#"{"type":"permission.completed","data":{"requestId":"p1","result":{"kind":"denied"}}}"#,
            ],
            false,
        );
        let last = evs.last().unwrap();
        match last {
            ActivityEvent::PermissionResolved { approved, .. } => assert!(!approved),
            other => panic!("expected PermissionResolved, got {other:?}"),
        }
    }

    #[test]
    fn unknown_event_type_is_silently_ignored() {
        let mut s = EventsState::new();
        let evs = collect(
            &mut s,
            &[
                br#"{"type":"session.start","data":{"sessionId":"abc"}}"#,
                br#"{"type":"some.future.kind","data":{"x":1}}"#,
                br#"{"type":"system.message","data":{"text":"hi"}}"#,
            ],
            false,
        );
        assert!(evs.is_empty());
    }

    #[test]
    fn malformed_line_does_not_panic() {
        let mut s = EventsState::new();
        let evs = collect(
            &mut s,
            &[
                b"not json at all",
                b"{\"type\":\"assistant.turn_start\",\"data\":",
                b"",
            ],
            false,
        );
        assert!(evs.is_empty());
        assert!(s.in_turn.is_none());
    }

    #[test]
    fn missing_data_field_does_not_crash() {
        let mut s = EventsState::new();
        let evs = collect(
            &mut s,
            &[
                br#"{"type":"tool.execution_start"}"#,
                br#"{"type":"permission.requested"}"#,
            ],
            false,
        );
        assert!(evs.is_empty());
        assert!(s.open_tools.is_empty());
        assert!(s.open_permissions.is_empty());
    }

    #[test]
    fn tool_complete_without_matching_start_is_dropped() {
        // We started tailing mid-file: the start line was never seen.
        let mut s = EventsState::new();
        let evs = collect(
            &mut s,
            &[br#"{"type":"tool.execution_complete","data":{"toolCallId":"c1","success":true}}"#],
            false,
        );
        assert!(evs.is_empty());
    }

    #[test]
    fn abort_clears_open_tools_and_in_turn_state() {
        let mut s = EventsState::new();
        let _ = collect(
            &mut s,
            &[
                br#"{"type":"assistant.turn_start","data":{"turnId":"t1"}}"#,
                br#"{"type":"tool.execution_start","data":{"toolCallId":"c1","toolName":"powershell"}}"#,
            ],
            false,
        );
        let evs = collect(
            &mut s,
            &[br#"{"type":"abort","data":{"reason":"user"}}"#],
            false,
        );
        // Order: turn-end before tool-end (turn-end first by impl).
        assert_eq!(evs.len(), 2);
        assert!(matches!(
            evs[0],
            ActivityEvent::TurnEnd { duration_ms: None }
        ));
        assert!(matches!(
            evs[1],
            ActivityEvent::ToolEnd {
                ref tool_call_id,
                success: false,
            } if tool_call_id == "c1"
        ));
        assert!(s.in_turn.is_none());
        assert!(s.open_tools.is_empty());
    }

    #[test]
    fn catch_up_suppresses_resolved_pairs_and_emits_only_open_state() {
        // Catch-up: replay a complete history with `suppress=true`. The
        // tailer should track state correctly but emit nothing during
        // replay; emit_current_state then surfaces only what's still
        // pending.
        let mut s = EventsState::new();
        let suppressed = collect(
            &mut s,
            &[
                br#"{"type":"assistant.turn_start","data":{"turnId":"t1"}}"#,
                br#"{"type":"tool.execution_start","data":{"toolCallId":"c1","toolName":"powershell"}}"#,
                br#"{"type":"tool.execution_complete","data":{"toolCallId":"c1","success":true}}"#,
                br#"{"type":"assistant.turn_end","data":{"turnId":"t1"}}"#,
                br#"{"type":"assistant.turn_start","data":{"turnId":"t2"}}"#,
                br#"{"type":"permission.requested","data":{"requestId":"p1","permissionRequest":{"kind":"shell","command":"rm -rf /"}}}"#,
            ],
            true,
        );
        assert!(
            suppressed.is_empty(),
            "catch-up must not emit transient events, got {suppressed:?}"
        );
        let mut out = Vec::new();
        emit_current_state(&s, |ev| out.push(ev));
        // AwaitingPermission wins priority over the still-open turn.
        assert_eq!(
            out,
            vec![ActivityEvent::AwaitingPermission {
                request_id: "p1".into(),
                permission_kind: "shell".into(),
                summary: Some("rm -rf /".into()),
            }]
        );
    }

    #[test]
    fn catch_up_with_only_open_turn_emits_turn_start() {
        let mut s = EventsState::new();
        let _ = collect(
            &mut s,
            &[br#"{"type":"assistant.turn_start","data":{"turnId":"t1"}}"#],
            true,
        );
        let mut out = Vec::new();
        emit_current_state(&s, |ev| out.push(ev));
        assert_eq!(out, vec![ActivityEvent::TurnStart]);
    }

    #[test]
    fn catch_up_quiescent_emits_nothing() {
        let mut s = EventsState::new();
        let _ = collect(
            &mut s,
            &[
                br#"{"type":"assistant.turn_start","data":{"turnId":"t1"}}"#,
                br#"{"type":"assistant.turn_end","data":{"turnId":"t1"}}"#,
            ],
            true,
        );
        let mut out = Vec::new();
        emit_current_state(&s, |ev| out.push(ev));
        assert!(out.is_empty());
    }

    #[test]
    fn catch_up_with_open_tool_only_emits_tool_start() {
        let mut s = EventsState::new();
        let _ = collect(
            &mut s,
            &[br#"{"type":"tool.execution_start","data":{"toolCallId":"c1","toolName":"view"}}"#],
            true,
        );
        let mut out = Vec::new();
        emit_current_state(&s, |ev| out.push(ev));
        assert_eq!(
            out,
            vec![ActivityEvent::ToolStart {
                tool_call_id: "c1".into(),
                tool_name: "view".into(),
            }]
        );
    }

    #[test]
    fn permission_kind_falls_back_to_tool_name_then_permission() {
        // No permissionRequest.kind, no top-level kind, but toolName.
        let v: serde_json::Value =
            serde_json::from_slice(br#"{"toolName":"shell","other":"x"}"#).unwrap();
        assert_eq!(extract_permission_kind(&v), "shell");

        // Nothing recognisable.
        let v: serde_json::Value = serde_json::from_slice(br#"{"x":1}"#).unwrap();
        assert_eq!(extract_permission_kind(&v), "permission");
    }

    #[test]
    fn permission_summary_prefers_command_then_summary_then_description() {
        let v: serde_json::Value = serde_json::from_slice(
            br#"{"permissionRequest":{"command":"git pull","summary":"sync","description":"d"}}"#,
        )
        .unwrap();
        assert_eq!(extract_permission_summary(&v).as_deref(), Some("git pull"));

        let v: serde_json::Value =
            serde_json::from_slice(br#"{"permissionRequest":{"summary":"sync"}}"#).unwrap();
        assert_eq!(extract_permission_summary(&v).as_deref(), Some("sync"));

        let v: serde_json::Value =
            serde_json::from_slice(br#"{"permissionRequest":{"description":"d"}}"#).unwrap();
        assert_eq!(extract_permission_summary(&v).as_deref(), Some("d"));

        let v: serde_json::Value = serde_json::from_slice(br#"{"x":1}"#).unwrap();
        assert_eq!(extract_permission_summary(&v), None);
    }

    #[test]
    fn events_path_layout_matches_copilot_session_state() {
        let p = events_path(std::path::Path::new("/home/u"), "abc-uuid");
        assert!(p.ends_with(std::path::Path::new(
            ".copilot/session-state/abc-uuid/events.jsonl"
        )));
    }

    // -----------------------------------------------------------------
    // End-to-end watcher harness — drives a real file from a background
    // thread, asserts the emitted events.
    // -----------------------------------------------------------------

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
        let path = tmp.path().join("events.jsonl");
        // Pre-seed with a complete (resolved) history — catch-up should
        // emit nothing for it.
        std::fs::write(
            &path,
            b"{\"type\":\"assistant.turn_start\",\"data\":{\"turnId\":\"t0\"}}\n\
              {\"type\":\"assistant.turn_end\",\"data\":{\"turnId\":\"t0\"}}\n",
        )
        .unwrap();

        let bag = Arc::new(Mutex::new(Vec::new()));
        let bag_for_cb = Arc::clone(&bag);
        let emit: CopilotActivityCb = Arc::new(move |sid, ev| {
            bag_for_cb.lock().unwrap().push((*sid, ev));
        });

        let session_id = SessionId::new();
        let running = Arc::new(AtomicBool::new(true));
        let r = Arc::clone(&running);
        let p = path.clone();
        let emit_clone = Arc::clone(&emit);
        let join = thread::spawn(move || run_watcher(session_id, p, emit_clone, r));

        // Wait until catch-up has finished (no events from quiescent
        // history) — tail-poll cycle is ~POLL_INTERVAL.
        thread::sleep(POLL_INTERVAL * 3);
        assert!(
            bag.lock().unwrap().is_empty(),
            "catch-up over a fully-resolved history must emit nothing",
        );

        // Now append a live event — should be emitted.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"{\"type\":\"permission.requested\",\"data\":{\"requestId\":\"p1\",\"permissionRequest\":{\"kind\":\"shell\",\"command\":\"ls\"}}}\n").unwrap();
        drop(f);

        let got = drain(
            &bag,
            |b| !b.lock().unwrap().is_empty(),
            Duration::from_secs(5),
        );
        running.store(false, Ordering::SeqCst);
        let _ = join.join();

        assert!(
            got.iter()
                .any(|(_, ev)| matches!(ev, ActivityEvent::AwaitingPermission { request_id, .. } if request_id == "p1")),
            "expected live AwaitingPermission emission, got {got:?}",
        );
    }

    #[test]
    fn run_watcher_catch_up_synthesizes_pending_permission() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        // Pre-seed with an *unresolved* permission request — catch-up
        // should synthesize a single AwaitingPermission so the sidebar
        // reflects current state on restore.
        std::fs::write(
            &path,
            b"{\"type\":\"assistant.turn_start\",\"data\":{\"turnId\":\"t0\"}}\n\
              {\"type\":\"permission.requested\",\"data\":{\"requestId\":\"p9\",\"permissionRequest\":{\"kind\":\"shell\",\"command\":\"git pull\"}}}\n",
        )
        .unwrap();

        let bag = Arc::new(Mutex::new(Vec::new()));
        let bag_for_cb = Arc::clone(&bag);
        let emit: CopilotActivityCb = Arc::new(move |sid, ev| {
            bag_for_cb.lock().unwrap().push((*sid, ev));
        });

        let session_id = SessionId::new();
        let running = Arc::new(AtomicBool::new(true));
        let r = Arc::clone(&running);
        let p = path.clone();
        let join = thread::spawn(move || run_watcher(session_id, p, emit, r));

        let got = drain(
            &bag,
            |b| !b.lock().unwrap().is_empty(),
            Duration::from_secs(5),
        );
        running.store(false, Ordering::SeqCst);
        let _ = join.join();

        let perms: Vec<_> = got
            .iter()
            .filter(|(_, ev)| matches!(ev, ActivityEvent::AwaitingPermission { .. }))
            .collect();
        assert_eq!(
            perms.len(),
            1,
            "expected exactly one synthesized AwaitingPermission, got {got:?}",
        );
    }

    #[test]
    fn run_watcher_catch_up_over_max_chunk_does_not_emit_historical_events() {
        // Regression for the multi-chunk catch-up bug. `tail_lines_pub`
        // (and the underlying `read_range`) caps each read at 10 MB
        // (`MAX_READ_CHUNK`). The pre-fix code flipped `catch_up_done`
        // after the *first* read, so any history past the 10 MB mark
        // was emitted with `suppress=false` — i.e., as if it were live —
        // surfacing as spurious tool/permission flickers in the sidebar
        // immediately after a Copilot session was restored.
        //
        // The fix snapshots EOF on the first iteration and only flips
        // `catch_up_done` once cursor has drained past that snapshot.
        // We seed events.jsonl with ~10 MB of forward-compatible
        // padding lines (unknown event type — silently ignored by
        // `ingest_line`), then a fully-resolved tool pair *past* the
        // 10 MB boundary. With the bug, the resolved pair surfaces as
        // a live ToolStart/ToolEnd. With the fix, neither event fires.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let padding_line: &[u8] =
            br#"{"type":"system.message","data":{"text":"forward-compat-pad"}}"#;
        let bytes_per_line: u64 = (padding_line.len() + 1) as u64; // newline
                                                                   // Push clearly past MAX_READ_CHUNK (10 MB) so a single poll
                                                                   // can't drain it in one read. ~11 MB of padding → second poll
                                                                   // is required to reach EOF. Test takes ~2 polls × 500 ms.
        let target_bytes: u64 = 11 * 1024 * 1024;
        let pad_lines: u64 = target_bytes / bytes_per_line;
        use std::io::{BufWriter, Write};
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut w = BufWriter::with_capacity(1 << 20, f);
            for _ in 0..pad_lines {
                w.write_all(padding_line).unwrap();
                w.write_all(b"\n").unwrap();
            }
            // Resolved tool pair past the 10 MB boundary. With the bug
            // these lines are read in iteration 2 with suppress=false
            // and surface as live emissions.
            w.write_all(
                br#"{"type":"tool.execution_start","data":{"toolCallId":"hist","toolName":"shell"}}"#,
            )
            .unwrap();
            w.write_all(b"\n").unwrap();
            w.write_all(
                br#"{"type":"tool.execution_complete","data":{"toolCallId":"hist","success":true}}"#,
            )
            .unwrap();
            w.write_all(b"\n").unwrap();
            w.flush().unwrap();
        }
        let final_len = std::fs::metadata(&path).unwrap().len();
        assert!(
            final_len > 10 * 1024 * 1024,
            "test fixture must exceed MAX_READ_CHUNK (10MB); got {final_len} bytes",
        );

        let bag = Arc::new(Mutex::new(Vec::new()));
        let bag_for_cb = Arc::clone(&bag);
        let emit: CopilotActivityCb = Arc::new(move |sid, ev| {
            bag_for_cb.lock().unwrap().push((*sid, ev));
        });

        let session_id = SessionId::new();
        let running = Arc::new(AtomicBool::new(true));
        let r = Arc::clone(&running);
        let p = path.clone();
        let join = thread::spawn(move || run_watcher(session_id, p, emit, r));

        // Wait for several poll cycles so multi-chunk catch-up fully
        // drains. POLL_INTERVAL is 500 ms; 5 cycles == 2.5 s, enough
        // even on a slow CI runner to cover at least three reads
        // (initial → drain past 10 MB → flip `catch_up_done`).
        thread::sleep(POLL_INTERVAL * 5);
        running.store(false, Ordering::SeqCst);
        let _ = join.join();

        let got = bag.lock().unwrap().clone();
        assert!(
            got.is_empty(),
            "fully-resolved historical events must not bleed through as live emissions; got {got:?}",
        );
    }
}
