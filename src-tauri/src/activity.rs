//! Per-session activity inference — scans raw PTY output for structured signals (window title, attention notifications) and tracks output byte-rate
//! to derive working/idle transitions.
//!
//! ## Design
//!
//! The scanner is a **forward-everything** layer: bytes flow through
//! [`ActivityScanner::feed_bytes`] unchanged on their way to xterm.js;
//! the scanner only *additionally* emits typed [`ActivityEvent`]s for the UI to consume.
//!
//! ## Signals
//!
//! Recognised today (informed by raw captures of `claude` and `copilot`, see `src-tauri/examples/pty_capture.rs`):
//!
//! - **`OSC 0;<title>`** / **`OSC 2;<title>`** → [`ActivityEvent::Title`]. Both
//!   terminate with either BEL (`\x07`) or ST (`\x1b\\`).
//! - **`OSC 9;<msg>`** (ConEmu notification) → [`ActivityEvent::Attention`].
//! - **`OSC 777;notify;...`** (rxvt/Kitty notification) →
//!   [`ActivityEvent::Attention`].
//! - **Output byte-rate**: a session that has not produced bytes for
//!   [`IDLE_THRESHOLD`] is reported as [`ActivityEvent::Idle`]; the next byte
//!   after an idle window flips it back to [`ActivityEvent::Working`].
//!
//! Future-proofed (not currently emitted by either tested CLI but cheap to parse): **OSC 133 A/B/C/D** → semantic prompt/command markers.
//!
//! ## State
//!
//! The OSC parser is a tiny state machine that survives across `feed_bytes` calls — sequences that span chunk boundaries are accumulated until their
//! terminator arrives. There is a hard cap on the buffered string length ([`OSC_MAX_LEN`]) so a malformed (un-terminated) sequence cannot grow
//! without bound.
//!
//! Working/idle inference is timestamp-driven: every byte updates `last_byte_at`; [`ActivityScanner::tick`] is called by an external timer and emits
//! transitions.

use std::time::{Duration, Instant};

/// How long without output before a session is considered idle.
pub const IDLE_THRESHOLD: Duration = Duration::from_millis(1500);

/// How often the external ticker should call [`ActivityScanner::tick`]. Public so the caller can size its timer accordingly.
pub const TICK_INTERVAL: Duration = Duration::from_millis(250);

/// Maximum length of an in-progress OSC payload before we give up and discard it. Real titles and notifications are well under this.
const OSC_MAX_LEN: usize = 4096;

/// What [`ActivityScanner`] reports.
//
// `rename_all` controls only variant names. `rename_all_fields` controls the named fields *inside* each variant — without it, a field like
// `tool_call_id` would serialize as `tool_call_id` on the wire while the TS mirror in `src/types/arborist.ts` expects `toolCallId`. The frontend
// reducer (`session-store.ts::applyActivity`) reads camelCase keys, so missing this rename silently zeroes every multi-word field. Pinned by the
// `activity_event_serde_uses_camelcase_field_keys` regression test below.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ActivityEvent {
    /// Window title set via `OSC 0;<title>` or `OSC 2;<title>`.
    Title { value: String },
    /// Generic "the user should look at this tab" cue: explicit notification escapes only (`OSC 9`, `OSC 777;notify;...`). **Standalone BEL is
    /// intentionally ignored** — both `claude` and `copilot` ring the bell as part of normal readline-style behavior (autocomplete misses,
    /// backspace at column 0, scrollback edge), which produced an unacceptable rate of false-positive "attention required" cues while the agent
    /// was simply thinking. If a CLI wants to demand attention, it must do so via a real notification OSC.
    Attention,
    /// Output is flowing. Emitted on the first byte after an idle window (or the very first byte of the session). Idempotent — only fires on the
    /// idle→working transition.
    Working,
    /// No output for [`IDLE_THRESHOLD`]. Emitted once per working→idle transition by [`ActivityScanner::tick`].
    Idle,
    /// `OSC 133;A` — start of prompt. Future-proofed; not currently emitted by `claude` or `copilot`.
    PromptStart,
    /// `OSC 133;C` — start of command (user submitted prompt). Future-proofed.
    CommandStart,
    /// `OSC 133;D[;<exit>]` — command ended with optional exit code. Future-proofed.
    CommandEnd { exit: Option<i32> },
    /// An agent turn just completed. Emitted by the per-tool metrics watcher (Copilot OTel `invoke_agent` span close; Claude transcript
    /// `assistant`-line arrival), not by the PTY-stream scanner. Carries the wall-clock duration of the turn when the source provides it.
    TurnEnd { duration_ms: Option<u64> },

    /// Agent invoked a tool; user is not yet blocked on input. Emitted by the Copilot events.jsonl tailer on `tool.execution_start`. Tracked by
    /// frontend in a per-session open-tool map; the icon flips to `runningTool` while the count > 0 and no permission is pending.
    ToolStart { tool_call_id: String, tool_name: String },
    /// Tool finished. Pairs with [`Self::ToolStart`] by `tool_call_id`. Emitted on `tool.execution_complete`.
    ToolEnd { tool_call_id: String, success: bool },
    /// Agent requested a permission (most commonly: shell-command approval); user is **blocked**. Emitted on `permission.requested` from the Copilot
    /// events.jsonl tailer. The frontend promotes this to the highest non-error display priority — this is the single most actionable cue we can give
    /// the user about a sidebar tab.
    AwaitingPermission {
        request_id: String,
        /// Short human-readable identifier for what's being approved (e.g. tool name, or `"shell"`). Surfaced in tooltips. Field is `permission_kind`
        /// (not `kind`) to avoid colliding with the serde tag on the parent enum.
        #[serde(rename = "permissionKind")]
        permission_kind: String,
        /// Optional one-line summary (e.g. the shell command). Best- effort — may be empty if the source didn't include enough detail to render
        /// meaningfully.
        summary: Option<String>,
    },
    /// Permission resolved (approved or denied). Pairs with
    /// [`Self::AwaitingPermission`] by `request_id`. Emitted on
    /// `permission.completed`.
    PermissionResolved { request_id: String, approved: bool },
    /// An assistant turn began. Emitted on `assistant.turn_start` from the Copilot events.jsonl tailer. The frontend uses this together with the
    /// open-tool/open-permission counts to derive the `thinking` display state (in-turn AND nothing else open).
    TurnStart,
}

#[derive(Debug)]
enum ParseState {
    /// Default: scanning for `\x1b` (ESC). Stray BEL bytes are intentionally consumed and ignored here.
    Ground,
    /// Saw `\x1b`, awaiting the next byte to disambiguate (OSC `]`, CSI `[`, or something we don't care about).
    Esc,
    /// Inside an OSC payload. The buffered string is everything between `\x1b]` and the terminator (BEL or `\x1b\\`). We saw an `\x1b` inside the
    /// payload and are checking whether it's the start of `\\`.
    OscPayload { saw_esc: bool },
}

/// Streaming activity scanner. One per session. Not `Send` across awaits; designed to live on the same OS thread as the PTY read loop.
pub struct ActivityScanner {
    state: ParseState,
    osc_buf: String,
    last_byte_at: Option<Instant>,
    /// Whether the last [`ActivityEvent::Working`]/[`ActivityEvent::Idle`] transition we *announced* was Working. Used to make working/idle emission
    /// idempotent.
    is_working: bool,
}

impl Default for ActivityScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivityScanner {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: ParseState::Ground,
            osc_buf: String::new(),
            last_byte_at: None,
            is_working: false,
        }
    }

    /// Feed raw PTY bytes. Returns any structured events recognised in this chunk. Always uses the wall clock via [`Instant::now`]; tests drive
    /// working/idle through [`Self::feed_bytes_at`] +
    /// [`Self::tick_at`].
    pub fn feed_bytes(&mut self, bytes: &[u8]) -> Vec<ActivityEvent> {
        self.feed_bytes_at(bytes, Instant::now())
    }

    /// As [`Self::feed_bytes`] but with an injected clock — for tests.
    pub fn feed_bytes_at(&mut self, bytes: &[u8], now: Instant) -> Vec<ActivityEvent> {
        let mut out = Vec::new();
        if !bytes.is_empty() {
            // Working transition fires on the first byte (cold start) or the first byte after an idle window.
            if !self.is_working {
                self.is_working = true;
                out.push(ActivityEvent::Working);
            }
            self.last_byte_at = Some(now);
        }

        for &b in bytes {
            self.consume_byte(b, &mut out);
        }
        out
    }

    /// Periodic tick. Emits [`ActivityEvent::Idle`] once if the session has been quiescent for at least [`IDLE_THRESHOLD`].
    pub fn tick(&mut self) -> Option<ActivityEvent> {
        self.tick_at(Instant::now())
    }

    /// As [`Self::tick`] but with an injected clock — for tests.
    pub fn tick_at(&mut self, now: Instant) -> Option<ActivityEvent> {
        if !self.is_working {
            return None;
        }
        let last = self.last_byte_at?;
        if now.duration_since(last) >= IDLE_THRESHOLD {
            self.is_working = false;
            return Some(ActivityEvent::Idle);
        }
        None
    }

    fn consume_byte(&mut self, b: u8, out: &mut Vec<ActivityEvent>) {
        match self.state {
            ParseState::Ground => {
                // 0x07 (BEL) is intentionally consumed silently here — see `ActivityEvent::Attention` doc. Real "look at this tab" cues come via
                // OSC 9 / OSC 777;notify, which terminate with BEL inside the OscPayload state below.
                if b == 0x1b {
                    self.state = ParseState::Esc;
                }
            }
            ParseState::Esc => match b {
                b']' => {
                    self.osc_buf.clear();
                    self.state = ParseState::OscPayload { saw_esc: false };
                }
                _ => {
                    // CSI (`[`), single-char escapes, and anything else we don't care about. Returning to Ground here is a simplification — we
                    // don't need to fully parse CSI since we only care about OSC. (We used to also attribute standalone BEL inside CSI as a
                    // false attention cue; BEL is no longer surfaced from Ground at all, so the trade-off is moot.)
                    self.state = ParseState::Ground;
                }
            },
            ParseState::OscPayload { saw_esc } => {
                if saw_esc {
                    // Last byte was ESC; this should be `\` to terminate (ST), otherwise the ESC was spurious — fold it into the buffer and continue.
                    if b == b'\\' {
                        self.finalize_osc(out);
                        self.state = ParseState::Ground;
                        return;
                    }
                    self.push_osc_byte(0x1b);
                    self.state = ParseState::OscPayload { saw_esc: false };
                    // fall through to handle b normally below
                }
                if b == 0x07 {
                    // BEL terminates OSC.
                    self.finalize_osc(out);
                    self.state = ParseState::Ground;
                } else if b == 0x1b {
                    self.state = ParseState::OscPayload { saw_esc: true };
                } else {
                    self.push_osc_byte(b);
                }
            }
        }
    }

    fn push_osc_byte(&mut self, b: u8) {
        if self.osc_buf.len() >= OSC_MAX_LEN {
            // Truncate silently — better than unbounded growth from a malformed stream. We will still attempt to parse what we have when the
            // terminator arrives.
            return;
        }
        // OSC payloads are conventionally text; non-UTF-8 bytes are replaced with `?` to keep `osc_buf` valid as a `String`.
        if b.is_ascii() {
            self.osc_buf.push(b as char);
        } else {
            self.osc_buf.push('?');
        }
    }

    fn finalize_osc(&mut self, out: &mut Vec<ActivityEvent>) {
        let payload = std::mem::take(&mut self.osc_buf);
        // OSC bodies look like `<Ps>;<rest>` where `Ps` is the numeric command identifier.
        let (ps, rest) = match payload.split_once(';') {
            Some((a, b)) => (a, b),
            None => (payload.as_str(), ""),
        };
        match ps {
            "0" | "2" => out.push(ActivityEvent::Title { value: rest.to_owned() }),
            "9" => out.push(ActivityEvent::Attention),
            "777" if rest.starts_with("notify") => out.push(ActivityEvent::Attention),
            "133" => {
                // OSC 133;<letter>[;<args>]
                let mut parts = rest.split(';');
                match parts.next() {
                    Some("A") => out.push(ActivityEvent::PromptStart),
                    Some("C") => out.push(ActivityEvent::CommandStart),
                    Some("D") => {
                        let exit = parts.next().and_then(|s| s.parse::<i32>().ok());
                        out.push(ActivityEvent::CommandEnd { exit });
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn activity_event_serde_uses_camelcase_field_keys() {
        // The TS mirror in `src/types/arborist.ts` and the reducer in `src/store/session-store.ts` read camelCase keys (`toolCallId`, `toolName`,
        // `requestId`, `durationMs`, etc.). The parent enum's `#[serde(rename_all = "camelCase")]` only renames *variants*; without
        // `rename_all_fields = "camelCase"`, multi-word field names serialize in snake_case and the frontend silently sees `undefined` for every such
        // field. This test pins the wire shape so a future maintainer can't regress it.
        let cases: &[(ActivityEvent, &[&str], &[&str])] = &[
            (
                ActivityEvent::ToolStart {
                    tool_call_id: "c1".into(),
                    tool_name: "shell".into(),
                },
                &["\"toolCallId\":\"c1\"", "\"toolName\":\"shell\""],
                &["tool_call_id", "tool_name"],
            ),
            (
                ActivityEvent::ToolEnd {
                    tool_call_id: "c1".into(),
                    success: true,
                },
                &["\"toolCallId\":\"c1\"", "\"success\":true"],
                &["tool_call_id"],
            ),
            (
                ActivityEvent::AwaitingPermission {
                    request_id: "r1".into(),
                    permission_kind: "shell".into(),
                    summary: Some("ls".into()),
                },
                &["\"requestId\":\"r1\"", "\"permissionKind\":\"shell\"", "\"summary\":\"ls\""],
                &["request_id", "permission_kind"],
            ),
            (
                ActivityEvent::PermissionResolved {
                    request_id: "r1".into(),
                    approved: false,
                },
                &["\"requestId\":\"r1\"", "\"approved\":false"],
                &["request_id"],
            ),
            (
                ActivityEvent::TurnEnd { duration_ms: Some(123) },
                &["\"durationMs\":123"],
                &["duration_ms"],
            ),
            (ActivityEvent::CommandEnd { exit: Some(1) }, &["\"exit\":1"], &[]),
        ];
        for (event, must_contain, must_not_contain) in cases {
            let json = serde_json::to_string(event).unwrap();
            for needle in *must_contain {
                assert!(json.contains(needle), "{event:?} → {json} missing `{needle}`",);
            }
            for forbidden in *must_not_contain {
                assert!(!json.contains(forbidden), "{event:?} → {json} contained snake_case `{forbidden}`",);
            }
        }
    }

    #[test]
    fn title_via_bel_terminator() {
        let mut s = ActivityScanner::new();
        let evs = s.feed_bytes(b"\x1b]0;hello world\x07");
        // First byte triggers Working; the OSC produces Title.
        assert_eq!(
            evs,
            vec![
                ActivityEvent::Working,
                ActivityEvent::Title {
                    value: "hello world".to_owned()
                },
            ]
        );
    }

    #[test]
    fn title_via_st_terminator() {
        let mut s = ActivityScanner::new();
        let evs = s.feed_bytes(b"\x1b]2;some title\x1b\\");
        assert_eq!(
            evs,
            vec![
                ActivityEvent::Working,
                ActivityEvent::Title {
                    value: "some title".to_owned()
                },
            ]
        );
    }

    #[test]
    fn osc_split_across_two_chunks() {
        let mut s = ActivityScanner::new();
        let a = s.feed_bytes(b"\x1b]0;part");
        let b = s.feed_bytes(b"itioned\x07");
        assert_eq!(a, vec![ActivityEvent::Working]);
        assert_eq!(
            b,
            vec![ActivityEvent::Title {
                value: "partitioned".to_owned()
            }]
        );
    }

    #[test]
    fn osc_split_at_terminator() {
        let mut s = ActivityScanner::new();
        let a = s.feed_bytes(b"\x1b]0;a\x1b");
        let b = s.feed_bytes(b"\\");
        assert_eq!(a, vec![ActivityEvent::Working]);
        assert_eq!(b, vec![ActivityEvent::Title { value: "a".to_owned() }]);
    }

    #[test]
    fn standalone_bel_is_ignored() {
        // Both `claude` and `copilot` ring BEL during normal readline-style operation (no autocomplete match, backspace at col 0, etc.). Surfacing
        // those as "attention required" produced too many false positives, so a bare BEL in Ground is now a no-op. Real attention cues come via
        // OSC 9 / OSC 777;notify (covered by their own tests below).
        let mut s = ActivityScanner::new();
        let evs = s.feed_bytes(b"hi\x07there");
        assert_eq!(evs, vec![ActivityEvent::Working]);
    }

    #[test]
    fn bel_inside_osc_does_not_emit_attention() {
        let mut s = ActivityScanner::new();
        let evs = s.feed_bytes(b"\x1b]0;t\x07");
        assert_eq!(evs, vec![ActivityEvent::Working, ActivityEvent::Title { value: "t".to_owned() }]);
    }

    #[test]
    fn osc_9_is_attention() {
        let mut s = ActivityScanner::new();
        let evs = s.feed_bytes(b"\x1b]9;done\x07");
        assert_eq!(evs, vec![ActivityEvent::Working, ActivityEvent::Attention]);
    }

    #[test]
    fn osc_777_notify_is_attention() {
        let mut s = ActivityScanner::new();
        let evs = s.feed_bytes(b"\x1b]777;notify;Build;Finished\x07");
        assert_eq!(evs, vec![ActivityEvent::Working, ActivityEvent::Attention]);
    }

    #[test]
    fn osc_777_non_notify_is_ignored() {
        let mut s = ActivityScanner::new();
        let evs = s.feed_bytes(b"\x1b]777;raise\x07");
        assert_eq!(evs, vec![ActivityEvent::Working]);
    }

    #[test]
    fn osc_133_prompt_start() {
        let mut s = ActivityScanner::new();
        let evs = s.feed_bytes(b"\x1b]133;A\x07");
        assert_eq!(evs, vec![ActivityEvent::Working, ActivityEvent::PromptStart]);
    }

    #[test]
    fn osc_133_command_end_with_exit() {
        let mut s = ActivityScanner::new();
        let evs = s.feed_bytes(b"\x1b]133;D;42\x07");
        assert_eq!(evs, vec![ActivityEvent::Working, ActivityEvent::CommandEnd { exit: Some(42) },]);
    }

    #[test]
    fn osc_133_command_end_without_exit() {
        let mut s = ActivityScanner::new();
        let evs = s.feed_bytes(b"\x1b]133;D\x07");
        assert_eq!(evs, vec![ActivityEvent::Working, ActivityEvent::CommandEnd { exit: None },]);
    }

    #[test]
    fn unknown_osc_is_silently_dropped() {
        let mut s = ActivityScanner::new();
        let evs = s.feed_bytes(b"\x1b]52;c;abc\x07");
        // No Title/Attention; just the bare Working from the first byte.
        assert_eq!(evs, vec![ActivityEvent::Working]);
    }

    #[test]
    fn csi_does_not_corrupt_state_or_swallow_subsequent_osc() {
        // Originally asserted that a BEL after a CSI surfaced as Attention. Standalone BEL is no longer an attention trigger, but the underlying
        // invariant we care about — CSI doesn't leave the parser in a stuck state — is still worth pinning. After a CSI, a follow-up OSC 9 must
        // still parse cleanly.
        let mut s = ActivityScanner::new();
        let evs = s.feed_bytes(b"\x1b[12;3H\x07\x1b]9;ping\x07");
        assert_eq!(evs, vec![ActivityEvent::Working, ActivityEvent::Attention]);
    }

    #[test]
    fn malformed_unterminated_osc_is_truncated_safely() {
        let mut s = ActivityScanner::new();
        // Way more bytes than OSC_MAX_LEN — must not OOM the buffer or crash. The terminator never arrives, so no Title is emitted.
        let mut payload = b"\x1b]0;".to_vec();
        payload.extend(std::iter::repeat_n(b'x', OSC_MAX_LEN * 2));
        let evs = s.feed_bytes(&payload);
        assert_eq!(evs, vec![ActivityEvent::Working]);
    }

    #[test]
    fn working_emitted_only_on_idle_to_active_transition() {
        let mut s = ActivityScanner::new();
        let a = s.feed_bytes(b"hello");
        let b = s.feed_bytes(b"world");
        // Working only on the first chunk.
        assert_eq!(a, vec![ActivityEvent::Working]);
        assert_eq!(b, Vec::<ActivityEvent>::new());
    }

    #[test]
    fn tick_emits_idle_after_threshold_and_only_once() {
        let mut s = ActivityScanner::new();
        let start = t0();
        let _ = s.feed_bytes_at(b"x", start);
        // Just under threshold — no idle yet.
        assert_eq!(s.tick_at(start + IDLE_THRESHOLD - Duration::from_millis(1)), None);
        // At threshold — idle fires.
        assert_eq!(s.tick_at(start + IDLE_THRESHOLD), Some(ActivityEvent::Idle));
        // Already idle — second tick is a no-op.
        assert_eq!(s.tick_at(start + IDLE_THRESHOLD * 2), None);
    }

    #[test]
    fn next_byte_after_idle_re_fires_working() {
        let mut s = ActivityScanner::new();
        let start = t0();
        let _ = s.feed_bytes_at(b"x", start);
        let _ = s.tick_at(start + IDLE_THRESHOLD);
        let evs = s.feed_bytes_at(b"y", start + IDLE_THRESHOLD + Duration::from_millis(50));
        assert_eq!(evs, vec![ActivityEvent::Working]);
    }

    #[test]
    fn empty_chunk_does_not_emit_working() {
        let mut s = ActivityScanner::new();
        let evs = s.feed_bytes(b"");
        assert!(evs.is_empty());
    }

    #[test]
    fn tick_before_any_bytes_is_noop() {
        let mut s = ActivityScanner::new();
        assert_eq!(s.tick_at(t0() + Duration::from_secs(60)), None);
    }

    #[test]
    fn captured_copilot_title_sequence() {
        // Sequence lifted directly from the copilot capture in session-state files/captures/copilot.bin: the second of the two title sets it does at
        // startup.
        let mut s = ActivityScanner::new();
        let evs = s.feed_bytes(b"\x1b]0;GitHub Copilot\x07");
        assert_eq!(
            evs,
            vec![
                ActivityEvent::Working,
                ActivityEvent::Title {
                    value: "GitHub Copilot".to_owned()
                },
            ]
        );
    }

    #[test]
    fn captured_claude_title_sequence() {
        let mut s = ActivityScanner::new();
        let evs = s.feed_bytes(b"\x1b]0;claude\x07");
        assert_eq!(evs, vec![ActivityEvent::Working, ActivityEvent::Title { value: "claude".to_owned() },]);
    }
}
