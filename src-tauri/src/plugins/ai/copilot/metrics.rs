//! Copilot metrics parsing (OTel file-exporter JSONL).
//!
//! GitHub Copilot CLI does **not** write token usage to its own session-state JSONL (`~/.copilot/session-state/<sid>/events.jsonl`). It does,
//! however, support OpenTelemetry export. We use the **file exporter** — enabled and configured via the env vars in [`super::CopilotPlugin::env`]
//! (injected by `pty_pool::spawn_internal`) — to redirect spans to a deterministic per-session file `<session_temp_dir>/otel.jsonl`.
//!
//! The generic engine in [`crate::session_metrics`] tails that file through a [`CopilotMetricsParser`], which extracts:
//!
//! * **Cumulative input/output token totals** from `chat <model>` span `attributes."gen_ai.usage.{input,output}_tokens"`. Each span is one LLM
//!   round-trip; we sum them.
//! * **Model name** from `attributes."gen_ai.response.model"` (fallback `"gen_ai.request.model"`).
//! * **Context-window state** from the inline event `github.copilot.session.usage_info`'s attributes: `github.copilot.token_limit` (the model's
//!   authoritative window) and `github.copilot.current_tokens` (the conversational context size at the moment that span was emitted — *not* the same
//!   as `gen_ai.usage.input_tokens`, which also includes cache-creation writes; we use `current_tokens` as the "context % used" numerator to match
//!   the user-visible Copilot status line).
//!
//! Two critical parsing rules:
//!
//! 1. **Filter to leaf `chat` spans only.** Copilot's `invoke_agent` parent span aggregates the *same* `gen_ai.usage.*` numbers as its child `chat`
//!    span(s). Counting both would double the totals. We require the semconv attribute `gen_ai.operation.name == "chat"` and ignore everything else
//!    (including `type: "metric"` lines, which are redundant with span attributes). The op attribute is the source of truth — `name` varies (`"chat"`
//!    vs `"chat <model>"` across Copilot versions) and a previous `name.starts_with("chat ")` filter silently dropped bare-name spans, breaking
//!    `aiSessionId` tracking after `/clear` or `/resume` and zeroing token totals for affected sessions.
//! 2. **Spans only emit at span CLOSE.** OTel's batch span processor flushes after the span ends, not while it's open. Use the existing PTY-stream
//!    activity scanner for "is the agent currently working" — OTel cannot answer that question.
//!
//! `OTEL_BSP_SCHEDULE_DELAY=1000` (set in [`super::CopilotPlugin::env`]) tightens the SDK's batch flush from its 5s default to ~1Hz so token totals
//! appear in the sidebar within a couple seconds of each agent turn.

use std::path::PathBuf;
use std::time::SystemTime;

use serde::Deserialize;

use crate::session_metrics::{context_used_pct, now_unix_seconds, LocatedFile, MetricsParser, TurnCb};
use crate::types::{SessionId, SessionMetricsEvent};

/// Running state accumulated by the Copilot parser across all ingested `chat <model>` spans for one session.
#[derive(Debug, Default)]
pub(crate) struct CopilotState {
    /// Cumulative input tokens (sum of `gen_ai.usage.input_tokens` across every leaf chat span). Includes cache-creation writes.
    sum_input: u64,
    /// Cumulative output tokens (sum of `gen_ai.usage.output_tokens`).
    sum_output: u64,
    /// Most recent model name observed (`gen_ai.response.model`, fallback `gen_ai.request.model`).
    last_model: Option<String>,
    /// Most recent value of `github.copilot.token_limit` from the inline `github.copilot.session.usage_info` event. Authoritative context window for
    /// the model that turn used.
    token_limit: Option<u64>,
    /// Most recent value of `github.copilot.current_tokens` — the size of the conversational context the agent had in front of it at the moment of
    /// the span. Drives the sidebar's "context % used".
    current_tokens: Option<u64>,
    /// Copilot's conversation/session id (`gen_ai.conversation.id`), matching the directory name under `~/.copilot/session-state/<id>/` and
    /// accepted by `copilot --session-id <id>` (copilot-cli >= 1.0.51). Captured for AI-session discovery; not part of the metrics snapshot.
    pub(crate) conversation_id: Option<String>,
    /// True once at least one chat span has been ingested.
    seen: bool,
}

impl CopilotState {
    pub(crate) fn has_any(&self) -> bool {
        self.seen
    }

    pub(crate) fn snapshot(&self, session_id: SessionId) -> SessionMetricsEvent {
        let used = self.current_tokens;
        let pct = context_used_pct(used, self.token_limit);
        SessionMetricsEvent {
            session_id,
            model: self.last_model.clone(),
            context_used_pct: pct,
            context_tokens_used: used,
            context_tokens_limit: self.token_limit,
            input_tokens: Some(self.sum_input),
            output_tokens: Some(self.sum_output),
            observed_at: now_unix_seconds(),
        }
    }
}

/// Ingest a single OTel JSONL line into `state`. Silently ignores anything that isn't a leaf `chat` span (metric lines, `invoke_agent` parents, other
/// span kinds, malformed JSON). Never panics.
///
/// The `invoke_agent` filter is critical: that span carries the *same* `gen_ai.usage.*` numbers as its child `chat` span(s), so counting both would
/// double the totals. We filter on the semconv attribute `gen_ai.operation.name == "chat"` — `name` is unreliable across Copilot versions (sometimes
/// `"chat"`, sometimes `"chat <model>"`) and the previous `name.starts_with("chat ")` filter silently dropped bare-name spans, breaking `aiSessionId`
/// tracking after `/clear` or `/resume` and zeroing token totals for affected sessions.
pub(crate) fn ingest_otel_line(line: &[u8], state: &mut CopilotState) {
    #[derive(Deserialize)]
    struct Outer {
        #[serde(default)]
        r#type: String,
        #[serde(default)]
        attributes: Option<serde_json::Value>,
        #[serde(default)]
        events: Option<Vec<OtelEvent>>,
    }
    #[derive(Deserialize)]
    struct OtelEvent {
        #[serde(default)]
        name: String,
        #[serde(default)]
        attributes: Option<serde_json::Value>,
    }

    let Ok(outer) = serde_json::from_slice::<Outer>(line) else {
        return;
    };
    if outer.r#type != "span" {
        return;
    }
    // Filter on the semconv operation attribute, NOT on `name`. See the doc comment above for why `name` is unreliable across Copilot versions.
    let attrs = outer.attributes.as_ref();
    let op = attrs.and_then(|a| a.get("gen_ai.operation.name")).and_then(|v| v.as_str());
    if op != Some("chat") {
        return;
    }

    let input = attrs
        .and_then(|a| a.get("gen_ai.usage.input_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output = attrs
        .and_then(|a| a.get("gen_ai.usage.output_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    state.sum_input = state.sum_input.saturating_add(input);
    state.sum_output = state.sum_output.saturating_add(output);

    if let Some(model) = attrs
        .and_then(|a| a.get("gen_ai.response.model"))
        .and_then(|v| v.as_str())
        .or_else(|| attrs.and_then(|a| a.get("gen_ai.request.model")).and_then(|v| v.as_str()))
    {
        state.last_model = Some(model.to_owned());
    }

    if let Some(conv) = attrs.and_then(|a| a.get("gen_ai.conversation.id")).and_then(|v| v.as_str()) {
        if !conv.is_empty() {
            state.conversation_id = Some(conv.to_owned());
        }
    }

    if let Some(events) = outer.events.as_ref() {
        for ev in events {
            if ev.name != "github.copilot.session.usage_info" {
                continue;
            }
            let ev_attrs = ev.attributes.as_ref();
            if let Some(v) = ev_attrs.and_then(|a| a.get("github.copilot.token_limit")).and_then(|v| v.as_u64()) {
                state.token_limit = Some(v);
            }
            if let Some(v) = ev_attrs.and_then(|a| a.get("github.copilot.current_tokens")).and_then(|v| v.as_u64()) {
                state.current_tokens = Some(v);
            }
        }
    }

    state.seen = true;
}

/// Extract the wall-clock duration of an `invoke_agent` span (one full agent turn) in milliseconds. Returns `None` for any other line — chat spans,
/// metric lines, malformed JSON — so the caller can call this on every JSONL line it tails.
///
/// We deliberately key on `invoke_agent` (not `chat`): one agent turn can involve multiple `chat` round-trips, but exactly one `invoke_agent`. This
/// matches the user's intuition of "the agent finished" — the icon flips to *awaiting* on the outer span close, not on each LLM hop.
pub(crate) fn parse_invoke_agent_duration_ms(line: &[u8]) -> Option<u64> {
    #[derive(Deserialize)]
    struct Outer {
        #[serde(default)]
        r#type: String,
        #[serde(default)]
        name: String,
        #[serde(default, rename = "startTime")]
        start_time: Option<[u64; 2]>,
        #[serde(default, rename = "endTime")]
        end_time: Option<[u64; 2]>,
    }
    let outer: Outer = serde_json::from_slice(line).ok()?;
    if outer.r#type != "span" || outer.name != "invoke_agent" {
        return None;
    }
    let start = outer.start_time?;
    let end = outer.end_time?;
    // OTel times are `[seconds, nanos]`. Compute `end - start` in ns, saturating at 0 (some test/edge writers can produce slightly out-of- order
    // timestamps).
    let start_ns = start[0].saturating_mul(1_000_000_000).saturating_add(start[1]);
    let end_ns = end[0].saturating_mul(1_000_000_000).saturating_add(end[1]);
    Some(end_ns.saturating_sub(start_ns) / 1_000_000)
}

/// Cheap byte-level prefilter used to skip a full JSON parse on the majority of OTel lines (metrics, logs, chat spans). Tolerates either
/// `"name":"invoke_agent"` or `"name": "invoke_agent"` spacing — real emitters use the compact form, but the OTel SDK is allowed to insert a space
/// and we'd rather over-accept here and let [`parse_invoke_agent_duration_ms`] reject than miss a legitimate turn-end.
fn maybe_invoke_agent_span(line: &[u8]) -> bool {
    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.len() >= needle.len() && hay.windows(needle.len()).any(|w| w == needle)
    }
    contains(line, b"\"invoke_agent\"")
}

/// Copilot OTel metrics parser. The OTel file path is fixed per session (`COPILOT_OTEL_FILE_EXPORTER_PATH`), so there is nothing to discover — the
/// engine binds it on the first poll and tails it for the session's lifetime.
pub struct CopilotMetricsParser {
    otel_path: PathBuf,
    state: CopilotState,
}

impl CopilotMetricsParser {
    #[must_use]
    pub fn new(otel_path: PathBuf) -> Self {
        Self {
            otel_path,
            state: CopilotState::default(),
        }
    }
}

impl MetricsParser for CopilotMetricsParser {
    fn relocate_each_poll(&self) -> bool {
        false
    }

    fn rebind_on_disappear(&self) -> bool {
        // The per-session OTel file is created at spawn prep and never moves. A transient stat failure must not drop accumulated totals.
        false
    }

    fn locate(&mut self, _spawn_instant: SystemTime) -> Option<LocatedFile> {
        // Fixed path; the conversation id is discovered from parsed content, not the filename.
        Some(LocatedFile {
            path: self.otel_path.clone(),
            ai_session_id: None,
        })
    }

    fn reset(&mut self) {
        self.state = CopilotState::default();
    }

    fn ingest_line(&mut self, line: &[u8], session_id: SessionId, emit_turn: &TurnCb) {
        ingest_otel_line(line, &mut self.state);
        // Cheap byte-level pre-filter — we don't want to re-parse every JSONL line as JSON just to discover it isn't an `invoke_agent` span. Real
        // Copilot OTel logs are dominated by metric/log lines, so this saves a full serde_json::from_slice on the hot path.
        if maybe_invoke_agent_span(line) {
            if let Some(d) = parse_invoke_agent_duration_ms(line) {
                emit_turn(session_id, Some(d));
            }
        }
    }

    fn content_ai_session_id(&self) -> Option<&str> {
        // Surface the Copilot conversation id as soon as we see it. The OTel file is per-Arborist-session, so this is unambiguous even when multiple
        // Copilot sessions run concurrently — unlike a directory-scan of `~/.copilot/session-state/`.
        self.state.conversation_id.as_deref()
    }

    fn snapshot(&self, session_id: SessionId) -> Option<SessionMetricsEvent> {
        if !self.state.has_any() {
            return None;
        }
        Some(self.state.snapshot(session_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Real probe data captured from `copilot -p` with the OTel file exporter enabled. Two spans (chat + invoke_agent parent) plus metric lines. Used
    /// as the canonical fixture for the parser tests.
    const COPILOT_OTEL_FIXTURE: &[u8] = include_bytes!("../../../../tests/fixtures/copilot_otel_sample.jsonl");

    fn fixture_lines() -> Vec<&'static [u8]> {
        COPILOT_OTEL_FIXTURE.split(|&b| b == b'\n').filter(|l| !l.is_empty()).collect()
    }

    #[test]
    fn ingest_otel_chat_span_extracts_totals_and_context() {
        // Find the leaf chat span line.
        let chat_line = fixture_lines()
            .into_iter()
            .find(|l| std::str::from_utf8(l).unwrap_or("").contains(r#""name":"chat "#))
            .expect("chat span in fixture");
        let mut state = CopilotState::default();
        ingest_otel_line(chat_line, &mut state);
        assert!(state.has_any());
        assert_eq!(state.sum_input, 39_497);
        assert_eq!(state.sum_output, 24);
        assert_eq!(state.last_model.as_deref(), Some("claude-opus-4.7"));
        assert_eq!(state.token_limit, Some(168_000));
        assert_eq!(state.current_tokens, Some(29_461));
    }

    #[test]
    fn ingest_otel_chat_span_extracts_conversation_id() {
        // The chat span carries `gen_ai.conversation.id` — that's the Copilot session id we feed back into `--session-id` (copilot-cli >= 1.0.51).
        let chat_line = fixture_lines()
            .into_iter()
            .find(|l| std::str::from_utf8(l).unwrap_or("").contains(r#""name":"chat "#))
            .expect("chat span in fixture");
        let mut state = CopilotState::default();
        ingest_otel_line(chat_line, &mut state);
        assert!(state.conversation_id.is_some(), "chat span must populate conversation_id",);
        let id = state.conversation_id.as_deref().expect("present");
        assert!(!id.is_empty(), "conversation id must be non-empty");
    }

    #[test]
    fn ingest_otel_chat_span_bare_name_is_accepted() {
        // Copilot CLI emits chat spans with two `name` formats — the older `"chat <model>"` and the newer bare `"chat"`. The op attribute
        // (`gen_ai.operation.name`) is identical in both. A previous `name.starts_with("chat ")` filter silently dropped bare-name spans, breaking
        // aiSessionId tracking after /clear or /resume and zeroing token totals for affected sessions. This test pins the new op-based filter to the
        // bare-name shape.
        let line = br#"{"type":"span","name":"chat","attributes":{"gen_ai.operation.name":"chat","gen_ai.response.model":"claude-opus-4.7-high","gen_ai.conversation.id":"abc-123","gen_ai.usage.input_tokens":50,"gen_ai.usage.output_tokens":5}}"#;
        let mut state = CopilotState::default();
        ingest_otel_line(line, &mut state);
        assert!(state.has_any(), "bare-name chat span must be accepted");
        assert_eq!(state.sum_input, 50);
        assert_eq!(state.sum_output, 5);
        assert_eq!(state.last_model.as_deref(), Some("claude-opus-4.7-high"));
        assert_eq!(state.conversation_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn ingest_otel_invoke_agent_with_chat_name_prefix_is_rejected() {
        // Belt-and-suspenders: invoke_agent carries the same usage numbers as its chat children. If a future Copilot version named the parent
        // something like "chat agent invocation" while keeping op=invoke_agent, a name-prefix filter would double-count. The op-based filter prevents
        // that regardless of the name string.
        let line = br#"{"type":"span","name":"chat agent invocation","attributes":{"gen_ai.operation.name":"invoke_agent","gen_ai.usage.input_tokens":999,"gen_ai.usage.output_tokens":99}}"#;
        let mut state = CopilotState::default();
        ingest_otel_line(line, &mut state);
        assert!(!state.has_any(), "invoke_agent op must be ignored regardless of name",);
    }

    #[test]
    fn ingest_otel_unknown_op_is_rejected() {
        // Defense in depth for any future op kind we haven't yet characterized — only `chat` is known to carry leaf usage totals we want to sum.
        let line = br#"{"type":"span","name":"chat_completion","attributes":{"gen_ai.operation.name":"chat_completion","gen_ai.usage.input_tokens":1000,"gen_ai.usage.output_tokens":100}}"#;
        let mut state = CopilotState::default();
        ingest_otel_line(line, &mut state);
        assert!(!state.has_any(), "non-chat op must be ignored");
    }

    #[test]
    fn ingest_otel_span_without_op_attribute_is_rejected() {
        // The semconv attribute is the source of truth. A span missing it (e.g. malformed exporter output) must not be counted even if `name` happens
        // to start with "chat".
        let line = br#"{"type":"span","name":"chat foo","attributes":{"gen_ai.usage.input_tokens":10,"gen_ai.usage.output_tokens":1}}"#;
        let mut state = CopilotState::default();
        ingest_otel_line(line, &mut state);
        assert!(!state.has_any(), "missing gen_ai.operation.name must be treated as unknown",);
    }

    #[test]
    fn ingest_otel_invoke_agent_is_ignored() {
        // The invoke_agent span carries the same gen_ai.usage.* numbers as its child chat span. If we counted both we'd double-count.
        let parent_line = fixture_lines()
            .into_iter()
            .find(|l| std::str::from_utf8(l).unwrap_or("").contains(r#""name":"invoke_agent""#))
            .expect("invoke_agent span in fixture");
        let mut state = CopilotState::default();
        ingest_otel_line(parent_line, &mut state);
        assert!(!state.has_any(), "invoke_agent must not advance state");
        assert_eq!(state.sum_input, 0);
        assert_eq!(state.sum_output, 0);
    }

    #[test]
    fn ingest_otel_metric_lines_are_ignored() {
        let metric_line = fixture_lines()
            .into_iter()
            .find(|l| std::str::from_utf8(l).unwrap_or("").contains(r#""type":"metric""#))
            .expect("metric line in fixture");
        let mut state = CopilotState::default();
        ingest_otel_line(metric_line, &mut state);
        assert!(!state.has_any(), "metric lines must not advance state");
    }

    #[test]
    fn ingest_otel_full_fixture_no_double_counting() {
        // Replay the entire fixture (two chat spans + invoke_agent + metric lines). Both chat spans should contribute their tokens; the invoke_agent
        // must NOT (it carries duplicates of the first chat span's totals). This is the regression assertion for the subagent / aggregate-parent
        // shape AND for accepting both the `chat <model>` and bare `chat` name formats.
        let mut state = CopilotState::default();
        for line in fixture_lines() {
            ingest_otel_line(line, &mut state);
        }
        assert_eq!(state.sum_input, 39_497 + 12_345, "both chat spans counted");
        assert_eq!(state.sum_output, 24 + 67);
        // The bare-name span comes last in the fixture, so its `current_tokens` / `token_limit` win the latest-wins race.
        assert_eq!(state.token_limit, Some(170_000));
        assert_eq!(state.current_tokens, Some(42_000));
        assert_eq!(state.last_model.as_deref(), Some("claude-opus-4.7-high"));
    }

    #[test]
    fn ingest_otel_malformed_json_is_ignored() {
        let mut state = CopilotState::default();
        ingest_otel_line(b"not json", &mut state);
        ingest_otel_line(b"{}", &mut state);
        ingest_otel_line(b"{\"type\":\"span\"}", &mut state); // no name
        assert!(!state.has_any());
    }

    #[test]
    fn ingest_otel_two_chat_spans_sum_and_latest_wins() {
        let chat1 = br#"{"type":"span","name":"chat model-a","attributes":{"gen_ai.operation.name":"chat","gen_ai.response.model":"model-a","gen_ai.usage.input_tokens":100,"gen_ai.usage.output_tokens":10},"events":[{"name":"github.copilot.session.usage_info","attributes":{"github.copilot.token_limit":1000,"github.copilot.current_tokens":500}}]}"#;
        let chat2 = br#"{"type":"span","name":"chat model-b","attributes":{"gen_ai.operation.name":"chat","gen_ai.response.model":"model-b","gen_ai.usage.input_tokens":200,"gen_ai.usage.output_tokens":20},"events":[{"name":"github.copilot.session.usage_info","attributes":{"github.copilot.token_limit":2000,"github.copilot.current_tokens":700}}]}"#;
        let mut state = CopilotState::default();
        ingest_otel_line(chat1, &mut state);
        ingest_otel_line(chat2, &mut state);
        // Sums.
        assert_eq!(state.sum_input, 300);
        assert_eq!(state.sum_output, 30);
        // Latest wins for model + context state.
        assert_eq!(state.last_model.as_deref(), Some("model-b"));
        assert_eq!(state.token_limit, Some(2000));
        assert_eq!(state.current_tokens, Some(700));
    }

    #[test]
    fn ingest_otel_chat_span_without_usage_info_event() {
        // Totals should still update; context fields stay at their last observed values (None here).
        let line = br#"{"type":"span","name":"chat foo","attributes":{"gen_ai.operation.name":"chat","gen_ai.response.model":"foo","gen_ai.usage.input_tokens":7,"gen_ai.usage.output_tokens":3}}"#;
        let mut state = CopilotState::default();
        ingest_otel_line(line, &mut state);
        assert_eq!(state.sum_input, 7);
        assert_eq!(state.sum_output, 3);
        assert!(state.token_limit.is_none());
        assert!(state.current_tokens.is_none());
    }

    #[test]
    fn ingest_otel_falls_back_to_request_model() {
        let line = br#"{"type":"span","name":"chat fallback","attributes":{"gen_ai.operation.name":"chat","gen_ai.request.model":"req-only","gen_ai.usage.input_tokens":1,"gen_ai.usage.output_tokens":2}}"#;
        let mut state = CopilotState::default();
        ingest_otel_line(line, &mut state);
        assert_eq!(state.last_model.as_deref(), Some("req-only"));
    }

    #[test]
    fn copilot_state_snapshot_computes_pct_from_current_tokens() {
        // Critical: pct uses the LATEST current_tokens / token_limit, NOT sum_input. Confirms the "context_tokens_used != input_tokens" invariant
        // from the design. The fixture's bare-name span is the last chat span, so its 42_000 / 170_000 win the latest-wins race for context
        // numerator/denominator. Cumulative totals remain the SUM across both spans.
        let mut state = CopilotState::default();
        for line in fixture_lines() {
            ingest_otel_line(line, &mut state);
        }
        let snap = state.snapshot(SessionId::new());
        assert_eq!(snap.context_tokens_used, Some(42_000));
        assert_eq!(snap.context_tokens_limit, Some(170_000));
        // 42000 * 100 / 170000 = 24
        assert_eq!(snap.context_used_pct, Some(24));
        // Cumulative totals are independent of the context numerator.
        assert_eq!(snap.input_tokens, Some(39_497 + 12_345));
        assert_eq!(snap.output_tokens, Some(24 + 67));
    }

    #[test]
    fn copilot_state_snapshot_omits_pct_without_limit() {
        let line = br#"{"type":"span","name":"chat foo","attributes":{"gen_ai.operation.name":"chat","gen_ai.response.model":"foo","gen_ai.usage.input_tokens":1,"gen_ai.usage.output_tokens":1}}"#;
        let mut state = CopilotState::default();
        ingest_otel_line(line, &mut state);
        let snap = state.snapshot(SessionId::new());
        assert!(snap.context_used_pct.is_none());
        assert!(snap.context_tokens_used.is_none());
        assert!(snap.context_tokens_limit.is_none());
    }

    #[test]
    fn parse_invoke_agent_duration_extracts_ms() {
        // Real fixture: invoke_agent span starts at [1777474197, 905_000_000] and ends at [1777474200, 749_237_700]. Difference is 2_844_237_700 ns ≈
        // 2844 ms.
        let line = fixture_lines()
            .into_iter()
            .find(|l| {
                let needle = b"invoke_agent";
                l.starts_with(br#"{"type":"span","traceId"#) && l.windows(needle.len()).any(|w| w == needle)
            })
            .expect("invoke_agent span in fixture");
        let ms = parse_invoke_agent_duration_ms(line).expect("duration parsed");
        assert!((2_840..=2_850).contains(&ms), "duration_ms ~= 2844, got {ms}",);
    }

    #[test]
    fn parse_invoke_agent_duration_ignores_chat_span() {
        // chat <model> spans are excluded — they are LLM round-trips, not turn boundaries.
        let line = fixture_lines()
            .into_iter()
            .find(|l| {
                let needle: &[u8] = br#""name":"chat "#;
                l.starts_with(br#"{"type":"span","#) && l.windows(needle.len()).any(|w| w == needle)
            })
            .expect("chat span in fixture");
        assert!(parse_invoke_agent_duration_ms(line).is_none());
    }

    #[test]
    fn parse_invoke_agent_duration_ignores_metric_lines() {
        let line = b"{\"type\":\"metric\",\"name\":\"gen_ai.client.token.usage\"}";
        assert!(parse_invoke_agent_duration_ms(line).is_none());
    }

    #[test]
    fn parse_invoke_agent_duration_handles_missing_times() {
        let line = br#"{"type":"span","name":"invoke_agent"}"#;
        assert!(parse_invoke_agent_duration_ms(line).is_none());
    }

    #[test]
    fn parse_invoke_agent_duration_saturates_for_inverted_times() {
        // Defensive: out-of-order timestamps must yield 0, not panic.
        let line = br#"{"type":"span","name":"invoke_agent","startTime":[10,0],"endTime":[5,0]}"#;
        assert_eq!(parse_invoke_agent_duration_ms(line), Some(0));
    }

    #[test]
    fn maybe_invoke_agent_span_skips_unrelated_lines() {
        // The cheap prefilter must reject anything that doesn't even mention "invoke_agent" — that's the whole point of avoiding a
        // serde_json::from_slice on the hot path.
        assert!(!maybe_invoke_agent_span(b""));
        assert!(!maybe_invoke_agent_span(b"{\"type\":\"metric\"}"));
        assert!(!maybe_invoke_agent_span(br#"{"type":"span","name":"chat claude-opus"}"#));
    }

    #[test]
    fn maybe_invoke_agent_span_admits_real_invoke_agent_lines() {
        let line = br#"{"type":"span","name":"invoke_agent","startTime":[1,0],"endTime":[2,0]}"#;
        assert!(maybe_invoke_agent_span(line));
    }

    /// Run a Copilot watcher against an evolving JSONL file in a tempdir. Drives state transitions by appending to the file and waiting for the
    /// callback to fire — no virtual time, but the test only sleeps long enough to clear at most a couple of poll intervals.
    #[test]
    fn copilot_watcher_emits_on_new_chat_span() {
        use std::sync::mpsc;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("otel.jsonl");
        // Pre-create empty so the watcher's first poll sees a valid file.
        std::fs::write(&path, b"").unwrap();

        let session_id = SessionId::new();
        let running = Arc::new(AtomicBool::new(true));
        let running_for_thread = Arc::clone(&running);
        let (tx, rx) = mpsc::channel::<SessionMetricsEvent>();
        let cb: crate::session_metrics::MetricsCb = Arc::new(move |ev| {
            // Channel may be closed if the test already finished; ignore send errors so the watcher thread can shut down cleanly.
            let _ = tx.send(ev);
        });
        let path_for_thread = path.clone();
        let handle = std::thread::spawn(move || {
            let parser = Box::new(CopilotMetricsParser::new(path_for_thread));
            crate::session_metrics::run_metrics_watcher(
                session_id,
                parser,
                SystemTime::now(),
                cb,
                Arc::new(|_, _| {}),
                Arc::new(|_, _| {}),
                running_for_thread,
            );
        });

        // Append the fixture (two chat spans + invoke_agent + metrics). On Linux, `std::fs::write` is not atomic so the watcher can catch a partial
        // write (only the first span). Drain snapshots until we see the cumulative totals from both spans.
        std::fs::write(&path, COPILOT_OTEL_FIXTURE).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let snap = loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let s = match rx.recv_timeout(remaining) {
                Ok(s) => s,
                Err(_) => panic!("timed out waiting for cumulative snapshot (context_tokens_used never reached 42_000)"),
            };
            if s.context_tokens_used == Some(42_000) {
                break s;
            }
        };
        assert_eq!(snap.context_tokens_used, Some(42_000));
        assert_eq!(snap.context_tokens_limit, Some(170_000));
        assert_eq!(snap.input_tokens, Some(39_497 + 12_345));
        assert_eq!(snap.output_tokens, Some(24 + 67));
        assert_eq!(snap.model.as_deref(), Some("claude-opus-4.7-high"));

        // Shut the watcher down.
        running.store(false, Ordering::SeqCst);
        handle.join().expect("watcher thread joined");
    }

    #[test]
    fn copilot_watcher_emits_turn_end_for_invoke_agent_span() {
        use std::sync::mpsc;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("otel.jsonl");
        std::fs::write(&path, b"").unwrap();

        let session_id = SessionId::new();
        let running = Arc::new(AtomicBool::new(true));
        let running_for_thread = Arc::clone(&running);
        let (tx, rx) = mpsc::channel::<(SessionId, Option<u64>)>();
        let metrics_cb: crate::session_metrics::MetricsCb = Arc::new(|_| {});
        let turn_cb: TurnCb = Arc::new(move |sid, dur| {
            let _ = tx.send((sid, dur));
        });
        let path_for_thread = path.clone();
        let handle = std::thread::spawn(move || {
            let parser = Box::new(CopilotMetricsParser::new(path_for_thread));
            crate::session_metrics::run_metrics_watcher(
                session_id,
                parser,
                SystemTime::now(),
                metrics_cb,
                turn_cb,
                Arc::new(|_, _| {}),
                running_for_thread,
            );
        });

        // The full fixture contains exactly one invoke_agent span.
        std::fs::write(&path, COPILOT_OTEL_FIXTURE).unwrap();
        let (sid, dur) = rx.recv_timeout(Duration::from_secs(8)).expect("watcher emitted turn-end");
        assert_eq!(sid, session_id);
        let dur = dur.expect("invoke_agent carries a duration");
        assert!((2_840..=2_850).contains(&dur), "expected ~2844ms duration, got {dur}",);

        running.store(false, Ordering::SeqCst);
        handle.join().expect("watcher thread joined");
    }

    #[test]
    fn copilot_watcher_handles_truncate_and_resets_totals() {
        use std::sync::mpsc;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("otel.jsonl");
        std::fs::write(&path, b"").unwrap();

        let session_id = SessionId::new();
        let running = Arc::new(AtomicBool::new(true));
        let running_for_thread = Arc::clone(&running);
        let (tx, rx) = mpsc::channel::<SessionMetricsEvent>();
        let cb: crate::session_metrics::MetricsCb = Arc::new(move |ev| {
            let _ = tx.send(ev);
        });
        let path_for_thread = path.clone();
        let handle = std::thread::spawn(move || {
            let parser = Box::new(CopilotMetricsParser::new(path_for_thread));
            crate::session_metrics::run_metrics_watcher(
                session_id,
                parser,
                SystemTime::now(),
                cb,
                Arc::new(|_, _| {}),
                Arc::new(|_, _| {}),
                running_for_thread,
            );
        });

        // 1) Initial usage from the full fixture.
        std::fs::write(&path, COPILOT_OTEL_FIXTURE).unwrap();
        let _first = rx.recv_timeout(Duration::from_secs(8)).expect("first snapshot");

        // 2) Truncate to a fresh, smaller chat span. Watcher must reset.
        let smaller = br#"{"type":"span","name":"chat tiny","attributes":{"gen_ai.operation.name":"chat","gen_ai.response.model":"tiny","gen_ai.usage.input_tokens":42,"gen_ai.usage.output_tokens":1},"events":[{"name":"github.copilot.session.usage_info","attributes":{"github.copilot.token_limit":1000,"github.copilot.current_tokens":50}}]}
"#;
        std::fs::write(&path, smaller).unwrap();

        // Drain until we get the post-truncate snapshot. The first message received after the truncate should reflect the smaller numbers (not the
        // cumulative pre-truncate values).
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let snap = loop {
            assert!(std::time::Instant::now() < deadline, "timed out");
            let s = rx.recv_timeout(Duration::from_secs(8)).expect("post-truncate snapshot");
            if s.input_tokens == Some(42) {
                break s;
            }
        };
        assert_eq!(snap.input_tokens, Some(42));
        assert_eq!(snap.output_tokens, Some(1));
        assert_eq!(snap.context_tokens_used, Some(50));
        assert_eq!(snap.context_tokens_limit, Some(1000));
        assert_eq!(snap.model.as_deref(), Some("tiny"));

        running.store(false, Ordering::SeqCst);
        handle.join().expect("watcher thread joined");
    }
}
