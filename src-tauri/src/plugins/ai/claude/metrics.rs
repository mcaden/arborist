//! Claude metrics parsing.
//!
//! Claude Code persists every session as a JSONL transcript at `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl`. Each `assistant`-type line
//! carries a `message.usage` object with `input_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens` and `output_tokens`. The model's
//! context-window limit can be read from `~/.claude/token_usage.json::actual_limit`, with a fallback table for when that file does not exist yet.
//!
//! The generic engine in [`crate::session_metrics`] drives a [`ClaudeMetricsParser`]: it periodically scans the project directory matching the
//! session's `cwd`, picks the JSONL file with the most recent mtime that's >= the session's spawn instant, tails new lines through
//! [`ClaudeMetricsParser::ingest_line`], and emits a snapshot on change.
//!
//! Limitation: two same-tool sessions in the same worktree cannot be disambiguated by cwd + mtime alone — both would observe the most-
//! recently-written JSONL. See follow-up issue #4 for the hook-driven authoritative version.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use crate::session_metrics::{context_used_pct, encode_cwd, now_unix_seconds, LocatedFile, MetricsParser, TurnCb};
use crate::types::{SessionId, SessionMetricsEvent};

#[derive(Debug, Default, Deserialize, Clone, Copy)]
pub(crate) struct UsageBlock {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

#[derive(Debug, Default)]
struct TurnTotals {
    /// Most recent observed turn — these reset to the values from the latest assistant line (each line is the cumulative state of that turn, not a
    /// delta).
    last_input: u64,
    last_output: u64,
    last_cache_creation: u64,
    last_cache_read: u64,
    /// Cumulative input/output across all observed assistant lines (sum of per-turn values). Useful for "session totals".
    sum_input: u64,
    sum_output: u64,
    /// Set true once we've ingested at least one usage block.
    seen: bool,
}

impl TurnTotals {
    fn add(&mut self, u: &UsageBlock) {
        self.sum_input = self.sum_input.saturating_add(u.input_tokens);
        self.sum_output = self.sum_output.saturating_add(u.output_tokens);
        self.last_input = u.input_tokens;
        self.last_output = u.output_tokens;
        self.last_cache_creation = u.cache_creation_input_tokens;
        self.last_cache_read = u.cache_read_input_tokens;
        self.seen = true;
    }

    fn has_any(&self) -> bool {
        self.seen
    }

    fn context_tokens_used(&self) -> u64 {
        self.last_input
            .saturating_add(self.last_cache_creation)
            .saturating_add(self.last_cache_read)
            .saturating_add(self.last_output)
    }
}

/// Try to parse a single JSONL line and return its `(usage, model)` if it is an `assistant` event with a `message.usage` block. Returns `None` for
/// any other line (system, user, sub-event, malformed, etc.). Never panics.
pub(crate) fn extract_assistant_usage(line: &[u8]) -> Option<(UsageBlock, Option<String>)> {
    #[derive(Deserialize)]
    struct Outer {
        #[serde(default)]
        r#type: String,
        #[serde(default)]
        message: Option<Message>,
    }
    #[derive(Deserialize)]
    struct Message {
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        usage: Option<UsageBlock>,
    }
    let outer: Outer = serde_json::from_slice(line).ok()?;
    if outer.r#type != "assistant" {
        return None;
    }
    let msg = outer.message?;
    let usage = msg.usage?;
    Some((usage, msg.model))
}

#[derive(Deserialize)]
struct TokenUsageFile {
    #[serde(default)]
    actual_limit: Option<u64>,
    #[serde(default)]
    expected_limit: Option<u64>,
}

/// Resolve the model's context-window limit. Tries `~/.claude/token_usage.json` first, then falls back to a small built-in table for known Anthropic
/// models. Returns `None` when neither source recognises the model.
pub(crate) fn resolve_limit(token_usage_path: &Path, model: Option<&str>) -> Option<u64> {
    if let Ok(bytes) = std::fs::read(token_usage_path) {
        if let Ok(file) = serde_json::from_slice::<TokenUsageFile>(&bytes) {
            if let Some(v) = file.actual_limit.or(file.expected_limit) {
                if v > 0 {
                    return Some(v);
                }
            }
        }
    }
    model.and_then(fallback_limit_for_model)
}

/// Fallback table for known model families. Keys match the substring form because Claude reports e.g. `"claude-sonnet-4-6"` while Anthropic also uses
/// `"claude-3-5-sonnet"`-style names.
fn fallback_limit_for_model(model: &str) -> Option<u64> {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") || m.contains("sonnet") || m.contains("haiku") {
        // All current Anthropic models default to 200k context.
        return Some(200_000);
    }
    None
}

fn build_snapshot(session_id: SessionId, totals: &TurnTotals, model: Option<String>, limit: Option<u64>) -> SessionMetricsEvent {
    let used = totals.context_tokens_used();
    let pct = context_used_pct(Some(used), limit);
    SessionMetricsEvent {
        session_id,
        model,
        context_used_pct: pct,
        context_tokens_used: Some(used),
        context_tokens_limit: limit,
        input_tokens: Some(totals.sum_input),
        output_tokens: Some(totals.sum_output),
        observed_at: now_unix_seconds(),
    }
}

/// Discover the newest `.jsonl` directly under `dir` whose mtime is >= `after` (with a small clock-skew slack window). Returns its full path.
fn newest_jsonl_after(dir: &Path, after: SystemTime) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        // Allow a small clock-skew slack window (5s) so the very first line — which can land before our spawn_instant due to mtime resolution / clock
        // differences — is still picked up.
        let slack = std::time::Duration::from_secs(5);
        let cutoff = after.checked_sub(slack).unwrap_or(after);
        if mtime < cutoff {
            continue;
        }
        match &best {
            Some((bt, _)) if *bt >= mtime => {}
            _ => best = Some((mtime, path)),
        }
    }
    best.map(|(_, p)| p)
}

/// Claude transcript metrics parser. Owns the project-dir scan and the per-turn usage accumulation; the engine owns the polling/tailing loop.
pub struct ClaudeMetricsParser {
    project_dir: PathBuf,
    token_usage_path: PathBuf,
    totals: TurnTotals,
    last_model: Option<String>,
}

impl ClaudeMetricsParser {
    #[must_use]
    pub fn new(home: &Path, cwd: &Path) -> Self {
        Self {
            project_dir: home.join(".claude").join("projects").join(encode_cwd(cwd)),
            token_usage_path: home.join(".claude").join("token_usage.json"),
            totals: TurnTotals::default(),
            last_model: None,
        }
    }
}

impl MetricsParser for ClaudeMetricsParser {
    fn relocate_each_poll(&self) -> bool {
        // A `/clear` makes Claude start a brand-new transcript file; the engine must rebind to the freshest matching JSONL each poll.
        true
    }

    fn rebind_on_disappear(&self) -> bool {
        false
    }

    fn locate(&mut self, spawn_instant: SystemTime) -> Option<LocatedFile> {
        let path = newest_jsonl_after(&self.project_dir, spawn_instant)?;
        // The tracked file's stem is Claude's session id (`<encoded-cwd>/<sessionId>.jsonl`).
        let ai_session_id = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned);
        Some(LocatedFile { path, ai_session_id })
    }

    fn reset(&mut self) {
        self.totals = TurnTotals::default();
        self.last_model = None;
    }

    fn ingest_line(&mut self, line: &[u8], _session_id: SessionId, _emit_turn: &TurnCb) {
        if let Some((usage, model)) = extract_assistant_usage(line) {
            self.totals.add(&usage);
            if let Some(m) = model {
                self.last_model = Some(m);
            }
            // Intentionally do NOT emit a TurnEnd here. Claude's transcript writes one `assistant` line per round-trip with the model — so a single
            // user turn that goes user → assistant(tool_use) → user(tool_result) → assistant(tool_use) → user(tool_result) → assistant(end_turn)
            // produces three assistant lines. The original code (pre-hook-integration) emitted TurnEnd on each, which would clear `inTurn` and
            // promote the sidebar to `awaiting` between tool calls — exactly the "thinking → zzz while reading file" symptom users see. Authoritative
            // turn boundaries now come from the Claude hook-events tailer ([`crate::claude_hook_events`]) via `UserPromptSubmit` (turnStart) and
            // `Stop` (turnEnd). For sessions where hooks aren't active (sidecar missing, partial install) we lose the `awaiting` promotion entirely;
            // PTY-byte heuristics still drive `working` / `idle` so the sidebar remains informative, just without the explicit "agent finished" cue.
        }
    }

    fn snapshot(&self, session_id: SessionId) -> Option<SessionMetricsEvent> {
        if !self.totals.has_any() {
            return None;
        }
        let limit = resolve_limit(&self.token_usage_path, self.last_model.as_deref());
        Some(build_snapshot(session_id, &self.totals, self.last_model.clone(), limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn extract_assistant_usage_happy_path() {
        let line = br#"{"type":"assistant","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":12,"output_tokens":34,"cache_creation_input_tokens":56,"cache_read_input_tokens":78}}}"#;
        let (u, m) = extract_assistant_usage(line).expect("parsed");
        assert_eq!(u.input_tokens, 12);
        assert_eq!(u.output_tokens, 34);
        assert_eq!(u.cache_creation_input_tokens, 56);
        assert_eq!(u.cache_read_input_tokens, 78);
        assert_eq!(m.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn extract_assistant_usage_returns_none_for_user_lines() {
        let line = br#"{"type":"user","message":{"role":"user","content":"hi"}}"#;
        assert!(extract_assistant_usage(line).is_none());
    }

    #[test]
    fn extract_assistant_usage_returns_none_for_system_lines() {
        let line = br#"{"type":"system","subtype":"turn_duration","durationMs":42}"#;
        assert!(extract_assistant_usage(line).is_none());
    }

    #[test]
    fn extract_assistant_usage_tolerates_missing_optional_fields() {
        // No cache fields present — should default to 0.
        let line = br#"{"type":"assistant","message":{"usage":{"input_tokens":1,"output_tokens":2}}}"#;
        let (u, m) = extract_assistant_usage(line).expect("parsed");
        assert_eq!(u.input_tokens, 1);
        assert_eq!(u.output_tokens, 2);
        assert_eq!(u.cache_creation_input_tokens, 0);
        assert_eq!(u.cache_read_input_tokens, 0);
        assert!(m.is_none());
    }

    #[test]
    fn extract_assistant_usage_returns_none_for_garbage() {
        assert!(extract_assistant_usage(b"not json").is_none());
        assert!(extract_assistant_usage(b"{}").is_none());
    }

    #[test]
    fn turn_totals_track_last_and_sum() {
        let mut t = TurnTotals::default();
        t.add(&UsageBlock {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: 100,
            cache_read_input_tokens: 200,
        });
        t.add(&UsageBlock {
            input_tokens: 20,
            output_tokens: 7,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 500,
        });
        // Last = the second turn.
        assert_eq!(t.last_input, 20);
        assert_eq!(t.last_output, 7);
        assert_eq!(t.last_cache_read, 500);
        assert_eq!(t.last_cache_creation, 0);
        // Sums accumulate input+output (cache fields aren't summed — they're per-turn state).
        assert_eq!(t.sum_input, 30);
        assert_eq!(t.sum_output, 12);
        // Context tokens used = sum of the LAST turn's four fields.
        assert_eq!(t.context_tokens_used(), 20 + 500 + 7);
        assert!(t.has_any());
    }

    #[test]
    fn fallback_limit_known_models() {
        assert_eq!(fallback_limit_for_model("claude-sonnet-4-6"), Some(200_000));
        assert_eq!(fallback_limit_for_model("claude-opus-4-7"), Some(200_000));
        assert_eq!(fallback_limit_for_model("claude-haiku-4-5"), Some(200_000));
        assert_eq!(fallback_limit_for_model("gpt-4"), None);
    }

    #[test]
    fn resolve_limit_prefers_token_usage_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("token_usage.json");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(br#"{"actual_limit":128000,"expected_limit":200000}"#).unwrap();
        assert_eq!(resolve_limit(&p, Some("claude-opus-4-7")), Some(128_000));
    }

    #[test]
    fn resolve_limit_falls_back_when_file_missing() {
        let p = std::path::Path::new("/nonexistent/arborist-test/token_usage.json");
        assert_eq!(resolve_limit(p, Some("claude-sonnet-4-6")), Some(200_000));
        assert_eq!(resolve_limit(p, Some("unknown-model")), None);
        assert_eq!(resolve_limit(p, None), None);
    }

    #[test]
    fn resolve_limit_ignores_zero_in_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("token_usage.json");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(br#"{"actual_limit":0}"#).unwrap();
        // Falls through to fallback table.
        assert_eq!(resolve_limit(&p, Some("claude-sonnet-4-6")), Some(200_000));
    }

    #[test]
    fn build_snapshot_computes_pct_when_limit_known() {
        let mut totals = TurnTotals::default();
        totals.add(&UsageBlock {
            input_tokens: 50_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        });
        let snap = build_snapshot(SessionId::new(), &totals, Some("claude-sonnet-4-6".into()), Some(200_000));
        assert_eq!(snap.context_used_pct, Some(25));
        assert_eq!(snap.context_tokens_used, Some(50_000));
        assert_eq!(snap.context_tokens_limit, Some(200_000));
        assert_eq!(snap.input_tokens, Some(50_000));
        assert_eq!(snap.output_tokens, Some(0));
    }

    #[test]
    fn build_snapshot_caps_pct_at_100() {
        let mut totals = TurnTotals::default();
        totals.add(&UsageBlock {
            input_tokens: 999_999,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        });
        let snap = build_snapshot(SessionId::new(), &totals, None, Some(200_000));
        assert_eq!(snap.context_used_pct, Some(100));
    }

    #[test]
    fn build_snapshot_omits_pct_when_limit_unknown() {
        let mut totals = TurnTotals::default();
        totals.add(&UsageBlock {
            input_tokens: 100,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        });
        let snap = build_snapshot(SessionId::new(), &totals, None, None);
        assert!(snap.context_used_pct.is_none());
        assert_eq!(snap.context_tokens_used, Some(100));
        assert!(snap.context_tokens_limit.is_none());
    }

    #[test]
    fn newest_jsonl_after_picks_freshest_eligible_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("aaa.jsonl");
        let b = dir.path().join("bbb.jsonl");
        let c = dir.path().join("not-a-transcript.txt");
        std::fs::write(&a, b"x").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&b, b"y").unwrap();
        std::fs::write(&c, b"z").unwrap();

        // After=epoch-far-past => both eligible; b is newer => picked.
        let picked = newest_jsonl_after(dir.path(), SystemTime::UNIX_EPOCH).expect("some");
        assert_eq!(picked, b);

        // After=now+1day => nothing eligible.
        let future = SystemTime::now() + Duration::from_secs(86_400);
        assert!(newest_jsonl_after(dir.path(), future).is_none());
    }

    /// Regression for the gap noted in PR review: a half-written trailing JSON line at EOF must NOT be silently dropped. The watcher must only
    /// advance its cursor up to the last complete `\n`, so the rest of the line is re-read after the writer finishes it.
    #[test]
    fn claude_watcher_does_not_drop_partial_trailing_line() {
        use std::sync::mpsc;

        // Set up a fake $HOME so the parser's project_dir resolves into our tempdir. encode_cwd of the cwd we pass becomes the dir name.
        let home = tempfile::tempdir().expect("home");
        let cwd_dir = tempfile::tempdir().expect("cwd");
        let cwd = cwd_dir.path().to_path_buf();
        let project_dir = home.path().join(".claude").join("projects").join(encode_cwd(&cwd));
        std::fs::create_dir_all(&project_dir).unwrap();
        let jsonl = project_dir.join("session.jsonl");
        std::fs::write(&jsonl, b"").unwrap();

        let session_id = SessionId::new();
        let running = Arc::new(AtomicBool::new(true));
        let running_for_thread = Arc::clone(&running);
        let (tx, rx) = mpsc::channel::<SessionMetricsEvent>();
        let cb: crate::session_metrics::MetricsCb = Arc::new(move |ev| {
            let _ = tx.send(ev);
        });
        let home_path = home.path().to_path_buf();
        let cwd_for_thread = cwd.clone();
        let spawn_instant = SystemTime::now() - Duration::from_secs(60);
        let handle = std::thread::spawn(move || {
            let parser = Box::new(ClaudeMetricsParser::new(&home_path, &cwd_for_thread));
            crate::session_metrics::run_metrics_watcher(
                session_id,
                parser,
                spawn_instant,
                cb,
                Arc::new(|_, _| {}),
                Arc::new(|_, _| {}),
                running_for_thread,
            );
        });

        // 1) Write one complete line + a partial second line (no newline).
        let complete = br#"{"type":"assistant","message":{"model":"claude-opus-4.7","usage":{"input_tokens":10,"output_tokens":2}}}"#;
        let partial = br#"{"type":"assistant","message":{"model":"claude-opus-4.7","usage":{"input_tokens":99"#;
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&jsonl).unwrap();
            f.write_all(complete).unwrap();
            f.write_all(b"\n").unwrap();
            f.write_all(partial).unwrap();
        }

        // First emission must reflect ONLY the complete line.
        let first = rx.recv_timeout(Duration::from_secs(8)).expect("first snapshot");
        assert_eq!(first.input_tokens, Some(10));
        assert_eq!(first.output_tokens, Some(2));

        // 2) Finish the partial line. The cursor must have stayed at the end of the first line, so the now-complete second line gets parsed in full
        //    and added to the totals.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&jsonl).unwrap();
            f.write_all(b",\"output_tokens\":7}}}\n").unwrap();
        }

        // Drain until totals jump to 10+99 = 109 in.
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let snap = loop {
            assert!(std::time::Instant::now() < deadline, "timed out");
            let s = rx.recv_timeout(Duration::from_secs(8)).expect("post-completion snapshot");
            if s.input_tokens == Some(109) {
                break s;
            }
        };
        assert_eq!(snap.input_tokens, Some(109), "partial line was re-read");
        assert_eq!(snap.output_tokens, Some(9));

        running.store(false, Ordering::SeqCst);
        handle.join().expect("watcher thread joined");
    }
}
