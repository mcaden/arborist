//! Per-session token-usage / context-window watcher.
//!
//! ## Signal sources (v1)
//!
//! ### Claude
//!
//! Claude Code persists every session as a JSONL transcript at
//! `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl`. Each `assistant`-type
//! line carries a `message.usage` object with `input_tokens`,
//! `cache_creation_input_tokens`, `cache_read_input_tokens` and
//! `output_tokens`. The model's context-window limit can be read from
//! `~/.claude/token_usage.json::actual_limit`, with a fallback table for
//! when that file does not exist yet.
//!
//! For each Arborist Claude session we spin up a [`MetricsWatcher`] thread
//! that periodically scans the project directory matching the session's
//! `cwd`, picks the JSONL file with the most recent mtime that's >= the
//! session's spawn instant, and extracts the latest token totals. On
//! change, it emits a [`MetricsSnapshot`] through the supplied callback.
//!
//! Limitation: two same-tool sessions in the same worktree cannot be
//! disambiguated by cwd + mtime alone — both would observe the most-
//! recently-written JSONL. See follow-up issue #4 for the hook-driven
//! authoritative version.
//!
//! ### Copilot
//!
//! GitHub Copilot CLI does **not** write token usage to its own session-
//! state JSONL (`~/.copilot/session-state/<sid>/events.jsonl`). It does,
//! however, support OpenTelemetry export. We use the **file exporter** —
//! enabled and configured via the env vars in `compose::env_for_tool`
//! (injected by `pty_pool::spawn_internal`) — to redirect spans to a
//! deterministic per-session file `<session_temp_dir>/otel.jsonl`.
//!
//! The watcher tails that file and extracts:
//!
//! * **Cumulative input/output token totals** from `chat <model>` span
//!   `attributes."gen_ai.usage.{input,output}_tokens"`. Each span is one
//!   LLM round-trip; we sum them.
//! * **Model name** from `attributes."gen_ai.response.model"` (fallback
//!   `"gen_ai.request.model"`).
//! * **Context-window state** from the inline event
//!   `github.copilot.session.usage_info`'s attributes:
//!   `github.copilot.token_limit` (the model's authoritative window) and
//!   `github.copilot.current_tokens` (the conversational context size at
//!   the moment that span was emitted — *not* the same as
//!   `gen_ai.usage.input_tokens`, which also includes cache-creation
//!   writes; we use `current_tokens` as the "context % used" numerator
//!   to match the user-visible Copilot status line).
//!
//! Two critical parsing rules:
//!
//! 1. **Filter to leaf `chat` spans only.** Copilot's `invoke_agent` parent
//!    span aggregates the *same* `gen_ai.usage.*` numbers as its child
//!    `chat` span(s). Counting both would double the totals. We require
//!    `name` to start with `"chat "` and ignore everything else (including
//!    `type: "metric"` lines, which are redundant with span attributes).
//! 2. **Spans only emit at span CLOSE.** OTel's batch span processor
//!    flushes after the span ends, not while it's open. Use the existing
//!    PTY-stream activity scanner for "is the agent currently working" —
//!    OTel cannot answer that question.
//!
//! `OTEL_BSP_SCHEDULE_DELAY=1000` (set in `env_for_tool`) tightens the
//! SDK's batch flush from its 5s default to ~1Hz so token totals appear
//! in the sidebar within a couple seconds of each agent turn.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::compose;
use crate::types::{SessionId, SessionMetricsEvent, Tool};

/// Polling cadence for the watcher. Trade-off: smaller is more responsive,
/// larger is fewer syscalls.
pub const POLL_INTERVAL: Duration = Duration::from_millis(2000);

/// Callback the watcher invokes for each new (changed) snapshot. Production
/// wires this into `app.emit("session://metrics", payload)`; tests pass a
/// channel sender.
pub type MetricsCb = Arc<dyn Fn(SessionMetricsEvent) + Send + Sync>;

/// Callback the watcher invokes when an agent turn completes. Production
/// wires this into `app.emit("session://activity", { kind: "turnEnd", ... })`
/// via [`crate::activity::ActivityEvent::TurnEnd`]. Tests pass a channel
/// sender.
///
/// `duration_ms` is the wall-clock duration of the turn when the source
/// reports it (Copilot OTel `invoke_agent` span), or `None` when it does
/// not (Claude transcript — we only see per-message timestamps, not a
/// reliable turn-start marker).
pub type TurnCb = Arc<dyn Fn(SessionId, Option<u64>) + Send + Sync>;

/// Per-session running watcher handle. Drop semantics: clearing the
/// `running` flag stops the watcher thread on its next poll iteration; the
/// thread's `JoinHandle` is detached so dropping the registry entry never
/// blocks the caller.
struct WatcherHandle {
    running: Arc<AtomicBool>,
}

/// Registry of active per-session watchers. Stored on `AppContext`. Calls
/// to [`MetricsRegistry::stop`] are idempotent — closing a session whose
/// watcher could not be started in the first place (e.g. a Claude session
/// whose home dir could not be resolved, or any session for which
/// `start` returned `false`) is a no-op. **Both** Claude and Copilot
/// sessions get watchers when their inputs are available — Claude via
/// transcript tailing, Copilot via OTel JSONL tailing.
#[derive(Default)]
pub struct MetricsRegistry {
    inner: Mutex<HashMap<SessionId, WatcherHandle>>,
}

impl MetricsRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a watcher for `session_id`. If one is already running it is
    /// stopped first so the new one starts from a clean slate (used by
    /// session restart). Returns `false` if no watcher could be started
    /// (e.g. Claude session whose home dir could not be resolved).
    pub fn start(
        &self,
        session_id: SessionId,
        tool: Tool,
        cwd: PathBuf,
        spawn_instant: SystemTime,
        emit: MetricsCb,
        emit_turn: TurnCb,
    ) -> bool {
        // Stop any existing watcher first so the per-tool worker starts
        // from a clean slate (used by session restart).
        self.stop(&session_id);

        let running = Arc::new(AtomicBool::new(true));
        let running_for_thread = Arc::clone(&running);
        let join = match tool {
            Tool::Claude => {
                let Some(home) = home_dir() else {
                    tracing::debug!(session_id = %session_id, "no home dir; Claude metrics watcher not started");
                    return false;
                };
                let cwd_for_thread = cwd.clone();
                thread::Builder::new()
                    .name(format!("arborist-metrics-{}", session_id))
                    .spawn(move || {
                        run_claude_watcher(
                            session_id,
                            home,
                            cwd_for_thread,
                            spawn_instant,
                            emit,
                            emit_turn,
                            running_for_thread,
                        );
                    })
            }
            Tool::Copilot => {
                let otel_path = compose::copilot_otel_path(&session_id);
                thread::Builder::new()
                    .name(format!("arborist-metrics-{}", session_id))
                    .spawn(move || {
                        run_copilot_watcher(
                            session_id,
                            otel_path,
                            emit,
                            emit_turn,
                            running_for_thread,
                        );
                    })
            }
        };
        match join {
            Ok(_handle) => {
                self.inner
                    .lock()
                    .expect("metrics registry lock")
                    .insert(session_id, WatcherHandle { running });
                true
            }
            Err(e) => {
                tracing::warn!(session_id = %session_id, error = %e, "metrics watcher thread spawn failed");
                false
            }
        }
    }

    /// Stop the watcher for `session_id` if any. Idempotent.
    pub fn stop(&self, session_id: &SessionId) {
        let removed = self
            .inner
            .lock()
            .expect("metrics registry lock")
            .remove(session_id);
        if let Some(h) = removed {
            h.running.store(false, Ordering::SeqCst);
        }
    }

    /// Stop every active watcher. Called on app shutdown / hot-reload.
    pub fn stop_all(&self) {
        let drained: Vec<WatcherHandle> = self
            .inner
            .lock()
            .expect("metrics registry lock")
            .drain()
            .map(|(_, h)| h)
            .collect();
        for h in drained {
            h.running.store(false, Ordering::SeqCst);
        }
    }
}

// ---------------------------------------------------------------------------
// Per-session worker
// ---------------------------------------------------------------------------

fn run_claude_watcher(
    session_id: SessionId,
    home: PathBuf,
    cwd: PathBuf,
    spawn_instant: SystemTime,
    emit: MetricsCb,
    emit_turn: TurnCb,
    running: Arc<AtomicBool>,
) {
    let project_dir = home.join(".claude").join("projects").join(encode_cwd(&cwd));
    let token_usage_path = home.join(".claude").join("token_usage.json");

    let mut last_emitted: Option<SessionMetricsEvent> = None;
    let mut tracked_path: Option<PathBuf> = None;
    let mut tracked_len: u64 = 0;
    let mut totals = TurnTotals::default();
    let mut last_model: Option<String> = None;

    while running.load(Ordering::SeqCst) {
        // Discover/refresh the JSONL we're tracking. The freshest file
        // whose mtime is >= our spawn instant wins.
        let candidate = newest_jsonl_after(&project_dir, spawn_instant);
        if let Some(c) = candidate {
            // If we switched files (e.g. the user typed `/clear` and Claude
            // started a new session) reset the read cursor and totals.
            if tracked_path.as_ref() != Some(&c) {
                tracked_path = Some(c.clone());
                tracked_len = 0;
                totals = TurnTotals::default();
                last_model = None;
            }

            if let Ok(meta) = std::fs::metadata(&c) {
                let len = meta.len();
                // Defensive: handle truncation/rotation. Same shape as the
                // Copilot watcher.
                if len < tracked_len {
                    tracked_len = 0;
                    totals = TurnTotals::default();
                    last_model = None;
                    last_emitted = None;
                }
                if len > tracked_len {
                    tracked_len = tail_lines(&c, tracked_len, len, |line| {
                        if let Some((usage, model)) = extract_assistant_usage(line) {
                            totals.add(&usage);
                            if let Some(m) = model {
                                last_model = Some(m);
                            }
                            // Each new assistant line is one completed agent
                            // turn. Claude's transcript does not carry a
                            // reliable turn-start timestamp distinct from
                            // the user message, so duration is omitted.
                            emit_turn(session_id, None);
                        }
                    });
                }
            }
        }

        if totals.has_any() {
            let limit = resolve_limit(&token_usage_path, last_model.as_deref());
            let snapshot = build_snapshot(session_id, &totals, last_model.clone(), limit);
            // Compare the data payload only — observed_at advances every
            // poll and would otherwise defeat dedup, causing a redundant
            // emission ~every POLL_INTERVAL even when nothing changed.
            if !last_emitted
                .as_ref()
                .is_some_and(|prev| prev.same_payload_as(&snapshot))
            {
                emit(snapshot.clone());
                last_emitted = Some(snapshot);
            }
        }

        thread::sleep(POLL_INTERVAL);
    }
}

// ---------------------------------------------------------------------------
// Copilot worker
// ---------------------------------------------------------------------------

fn run_copilot_watcher(
    session_id: SessionId,
    otel_path: PathBuf,
    emit: MetricsCb,
    emit_turn: TurnCb,
    running: Arc<AtomicBool>,
) {
    let mut state = CopilotState::default();
    let mut cursor: u64 = 0;
    let mut last_emitted: Option<SessionMetricsEvent> = None;

    while running.load(Ordering::SeqCst) {
        if let Ok(meta) = std::fs::metadata(&otel_path) {
            let len = meta.len();
            // Truncated or rotated under us: reset and reread from start.
            // Spawn-time prep removes a stale otel.jsonl, but a hostile
            // rotation mid-run shouldn't make us read garbage offsets.
            if len < cursor {
                cursor = 0;
                state = CopilotState::default();
                last_emitted = None;
            }
            if len > cursor {
                cursor = tail_lines(&otel_path, cursor, len, |line| {
                    ingest_otel_line(line, &mut state);
                    // Cheap byte-level pre-filter — we don't want to
                    // re-parse every JSONL line as JSON just to discover
                    // it isn't an `invoke_agent` span. Real Copilot OTel
                    // logs are dominated by metric/log lines, so this
                    // saves a full serde_json::from_slice on the hot path.
                    if maybe_invoke_agent_span(line) {
                        if let Some(d) = parse_invoke_agent_duration_ms(line) {
                            emit_turn(session_id, Some(d));
                        }
                    }
                });
            }
        }

        if state.has_any() {
            let snapshot = state.snapshot(session_id);
            // Same dedup fix as the Claude watcher: observed_at must not
            // be part of the comparison.
            if !last_emitted
                .as_ref()
                .is_some_and(|prev| prev.same_payload_as(&snapshot))
            {
                emit(snapshot.clone());
                last_emitted = Some(snapshot);
            }
        }

        thread::sleep(POLL_INTERVAL);
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Encode a cwd to the `~/.claude/projects/<dir>` form used by Claude
/// Code: every `\\`, `/`, `:`, and `.` in the absolute path becomes `-`.
/// The dot replacement is what produces the `--` between segments like
/// `\.worktrees\` (a real path written by `git worktree`); without it the
/// encoded directory name will not match Claude's on disk.
#[must_use]
pub fn encode_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(ch, '\\' | '/' | ':' | '.') {
            out.push('-');
        } else {
            out.push(ch);
        }
    }
    out
}

fn home_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("HOME") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Ok(p) = std::env::var("USERPROFILE") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    None
}

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
        // Allow a small clock-skew slack window (5s) so the very first
        // line — which can land before our spawn_instant due to mtime
        // resolution / clock differences — is still picked up.
        let slack = Duration::from_secs(5);
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

/// Defensive cap on a single watcher read. A runaway exporter (e.g. a
/// Copilot bug or a Claude session that grew enormous between polls)
/// must not be able to make us allocate gigabytes in one shot. If the
/// file is larger than this, we read this much and let the caller's
/// last-newline logic re-enter on the next poll to consume the rest.
const MAX_READ_CHUNK: u64 = 10 * 1024 * 1024;

fn read_range(path: &Path, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(start))?;
    let span = end.saturating_sub(start);
    let len = span.min(MAX_READ_CHUNK) as usize;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Tail `path` from `cursor` up to `end`, invoking `consume` for each
/// complete `\n`-terminated line found. Returns the new cursor position
/// (always `>= cursor`).
///
/// Three pitfalls handled:
/// 1. **Half-written trailing line.** The cursor only advances past the
///    last complete `\n`; partial trailing bytes are re-read on the next
///    call. Without this the line parser silently drops a partial JSON
///    object the writer is still finishing.
/// 2. **Read cap.** `read_range` is capped at [`MAX_READ_CHUNK`] so a
///    runaway exporter cannot make us allocate gigabytes in one shot.
///    The remaining bytes are picked up on subsequent polls.
/// 3. **Oversized line (> [`MAX_READ_CHUNK`] without a `\n` in the cap).**
///    If the capped chunk has no newline AND the file extends past what
///    we read, the line itself is bigger than the cap. Without special
///    handling, `rposition` would always be `None` and the watcher would
///    re-read the same chunk forever (wasted I/O, metrics never advance).
///    We scan forward from the end of the chunk *without* buffering to
///    find the next `\n`, log a warning, and skip the whole oversized
///    line. If no `\n` is found anywhere up to `end`, the writer is
///    still in flight, so we leave the cursor where it is and let the
///    next poll re-check.
fn tail_lines<F: FnMut(&[u8])>(path: &Path, cursor: u64, end: u64, mut consume: F) -> u64 {
    let bytes = match read_range(path, cursor, end) {
        Ok(b) => b,
        Err(_) => return cursor,
    };
    if let Some(nl) = bytes.iter().rposition(|&b| b == b'\n') {
        for line in bytes[..=nl].split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            consume(line);
        }
        return cursor + (nl as u64) + 1;
    }
    // No newline in the chunk. If there's more file beyond what we read,
    // the line is oversized — skip it. Otherwise the writer is still in
    // flight and we should retry next poll.
    let read = bytes.len() as u64;
    if cursor + read < end {
        tracing::warn!(
            path = %path.display(),
            max_bytes = MAX_READ_CHUNK,
            "session_metrics: skipping oversized line (>cap, no newline in chunk)"
        );
        match find_next_newline(path, cursor + read, end) {
            Some(nl_abs) => nl_abs + 1,
            None => cursor,
        }
    } else {
        cursor
    }
}

/// Scan `path` from `start` (inclusive) up to `end` (exclusive) without
/// buffering the whole range, looking for the next `\n`. Returns its
/// absolute byte offset, or `None` if no newline exists in the range.
fn find_next_newline(path: &Path, start: u64, end: u64) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = [0u8; 64 * 1024];
    let mut pos = start;
    while pos < end {
        let want = ((end - pos) as usize).min(buf.len());
        let n = f.read(&mut buf[..want]).ok()?;
        if n == 0 {
            return None;
        }
        if let Some(idx) = buf[..n].iter().position(|&b| b == b'\n') {
            return Some(pos + idx as u64);
        }
        pos += n as u64;
    }
    None
}

// ---------------------------------------------------------------------------
// JSONL parsing
// ---------------------------------------------------------------------------

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
    /// Most recent observed turn — these reset to the values from the
    /// latest assistant line (each line is the cumulative state of that
    /// turn, not a delta).
    last_input: u64,
    last_output: u64,
    last_cache_creation: u64,
    last_cache_read: u64,
    /// Cumulative input/output across all observed assistant lines (sum
    /// of per-turn values). Useful for "session totals".
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

/// Try to parse a single JSONL line and return its `(usage, model)` if it
/// is an `assistant` event with a `message.usage` block. Returns `None`
/// for any other line (system, user, sub-event, malformed, etc.). Never
/// panics.
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

// ---------------------------------------------------------------------------
// Context-limit lookup
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenUsageFile {
    #[serde(default)]
    actual_limit: Option<u64>,
    #[serde(default)]
    expected_limit: Option<u64>,
}

/// Resolve the model's context-window limit. Tries `~/.claude/token_usage.json`
/// first, then falls back to a small built-in table for known Anthropic
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

/// Fallback table for known model families. Keys match the substring form
/// because Claude reports e.g. `"claude-sonnet-4-6"` while Anthropic also
/// uses `"claude-3-5-sonnet"`-style names.
fn fallback_limit_for_model(model: &str) -> Option<u64> {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") || m.contains("sonnet") || m.contains("haiku") {
        // All current Anthropic models default to 200k context.
        return Some(200_000);
    }
    None
}

// ---------------------------------------------------------------------------
// Snapshot construction
// ---------------------------------------------------------------------------

fn build_snapshot(
    session_id: SessionId,
    totals: &TurnTotals,
    model: Option<String>,
    limit: Option<u64>,
) -> SessionMetricsEvent {
    let used = totals.context_tokens_used();
    let pct = limit.and_then(|lim| {
        used.saturating_mul(100)
            .checked_div(lim)
            .map(|raw| raw.min(100) as u8)
    });
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

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Copilot OTel parser
// ---------------------------------------------------------------------------

/// Running state accumulated by the Copilot watcher across all ingested
/// `chat <model>` spans for one session.
#[derive(Debug, Default)]
pub(crate) struct CopilotState {
    /// Cumulative input tokens (sum of `gen_ai.usage.input_tokens` across
    /// every leaf chat span). Includes cache-creation writes.
    sum_input: u64,
    /// Cumulative output tokens (sum of `gen_ai.usage.output_tokens`).
    sum_output: u64,
    /// Most recent model name observed (`gen_ai.response.model`, fallback
    /// `gen_ai.request.model`).
    last_model: Option<String>,
    /// Most recent value of `github.copilot.token_limit` from the inline
    /// `github.copilot.session.usage_info` event. Authoritative context
    /// window for the model that turn used.
    token_limit: Option<u64>,
    /// Most recent value of `github.copilot.current_tokens` — the size of
    /// the conversational context the agent had in front of it at the
    /// moment of the span. Drives the sidebar's "context % used".
    current_tokens: Option<u64>,
    /// True once at least one chat span has been ingested.
    seen: bool,
}

impl CopilotState {
    pub(crate) fn has_any(&self) -> bool {
        self.seen
    }

    pub(crate) fn snapshot(&self, session_id: SessionId) -> SessionMetricsEvent {
        let used = self.current_tokens;
        let pct = match (used, self.token_limit) {
            (Some(u), Some(lim)) if lim > 0 => Some(
                u.saturating_mul(100)
                    .checked_div(lim)
                    .map(|raw| raw.min(100) as u8)
                    .unwrap_or(0),
            ),
            _ => None,
        };
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

/// Ingest a single OTel JSONL line into `state`. Silently ignores anything
/// that isn't a leaf `chat` span (metric lines, `invoke_agent` parents,
/// other span kinds, malformed JSON). Never panics.
///
/// The `invoke_agent` filter is critical: that span carries the *same*
/// `gen_ai.usage.*` numbers as its child `chat` span(s), so counting both
/// would double the totals. Filtering on `name.starts_with("chat ")`
/// matches the leaf-only rule.
pub(crate) fn ingest_otel_line(line: &[u8], state: &mut CopilotState) {
    #[derive(Deserialize)]
    struct Outer {
        #[serde(default)]
        r#type: String,
        #[serde(default)]
        name: String,
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
    // Leaf chat spans only. The space after "chat" is intentional — it
    // matches `chat <model>` and excludes any future `chat_completion`-
    // style sibling that we'd want to treat differently.
    if !outer.name.starts_with("chat ") {
        return;
    }

    let attrs = outer.attributes.as_ref();
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
        .or_else(|| {
            attrs
                .and_then(|a| a.get("gen_ai.request.model"))
                .and_then(|v| v.as_str())
        })
    {
        state.last_model = Some(model.to_owned());
    }

    if let Some(events) = outer.events.as_ref() {
        for ev in events {
            if ev.name != "github.copilot.session.usage_info" {
                continue;
            }
            let ev_attrs = ev.attributes.as_ref();
            if let Some(v) = ev_attrs
                .and_then(|a| a.get("github.copilot.token_limit"))
                .and_then(|v| v.as_u64())
            {
                state.token_limit = Some(v);
            }
            if let Some(v) = ev_attrs
                .and_then(|a| a.get("github.copilot.current_tokens"))
                .and_then(|v| v.as_u64())
            {
                state.current_tokens = Some(v);
            }
        }
    }

    state.seen = true;
}

/// Extract the wall-clock duration of an `invoke_agent` span (one full
/// agent turn) in milliseconds. Returns `None` for any other line — chat
/// spans, metric lines, malformed JSON — so the caller can call this on
/// every JSONL line it tails.
///
/// We deliberately key on `invoke_agent` (not `chat`): one agent turn can
/// involve multiple `chat` round-trips, but exactly one `invoke_agent`.
/// This matches the user's intuition of "the agent finished" — the icon
/// flips to *awaiting* on the outer span close, not on each LLM hop.
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
    // OTel times are `[seconds, nanos]`. Compute `end - start` in ns,
    // saturating at 0 (some test/edge writers can produce slightly out-of-
    // order timestamps).
    let start_ns = start[0]
        .saturating_mul(1_000_000_000)
        .saturating_add(start[1]);
    let end_ns = end[0].saturating_mul(1_000_000_000).saturating_add(end[1]);
    Some(end_ns.saturating_sub(start_ns) / 1_000_000)
}

/// Cheap byte-level prefilter used to skip a full JSON parse on the
/// majority of OTel lines (metrics, logs, chat spans). Tolerates either
/// `"name":"invoke_agent"` or `"name": "invoke_agent"` spacing — real
/// emitters use the compact form, but the OTel SDK is allowed to insert
/// a space and we'd rather over-accept here and let
/// [`parse_invoke_agent_duration_ms`] reject than miss a legitimate
/// turn-end.
fn maybe_invoke_agent_span(line: &[u8]) -> bool {
    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.len() >= needle.len() && hay.windows(needle.len()).any(|w| w == needle)
    }
    contains(line, b"\"invoke_agent\"")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn encode_cwd_replaces_separators_and_drive_colon() {
        let p = if cfg!(windows) {
            Path::new("C:\\Users\\me\\proj")
        } else {
            Path::new("/Users/me/proj")
        };
        let s = encode_cwd(p);
        if cfg!(windows) {
            assert_eq!(s, "C--Users-me-proj");
        } else {
            assert_eq!(s, "-Users-me-proj");
        }
    }

    #[test]
    fn encode_cwd_replaces_dots_in_segments() {
        // Real-world git-worktree path: the leading `.` in `.worktrees` is
        // what produces the `--` Claude writes on disk.
        let p = if cfg!(windows) {
            Path::new("C:\\repos\\specd\\.worktrees\\fix-cursor-sync")
        } else {
            Path::new("/repos/specd/.worktrees/fix-cursor-sync")
        };
        let s = encode_cwd(p);
        if cfg!(windows) {
            assert_eq!(s, "C--repos-specd--worktrees-fix-cursor-sync");
        } else {
            assert_eq!(s, "-repos-specd--worktrees-fix-cursor-sync");
        }
    }

    #[test]
    fn encode_cwd_idempotent_on_already_encoded() {
        // Already-encoded form has no `/`, `\`, `:`, or `.`; should pass through.
        let s = encode_cwd(Path::new("C--Users-me-proj"));
        assert_eq!(s, "C--Users-me-proj");
    }

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
        let line =
            br#"{"type":"assistant","message":{"usage":{"input_tokens":1,"output_tokens":2}}}"#;
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
        // Sums accumulate input+output (cache fields aren't summed —
        // they're per-turn state).
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
        f.write_all(br#"{"actual_limit":128000,"expected_limit":200000}"#)
            .unwrap();
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
        let snap = build_snapshot(
            SessionId::new(),
            &totals,
            Some("claude-sonnet-4-6".into()),
            Some(200_000),
        );
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
    fn registry_start_spawns_watcher_for_copilot() {
        let reg = MetricsRegistry::new();
        let id = SessionId::new();
        let cb: MetricsCb = Arc::new(|_| {});
        let turn_cb: TurnCb = Arc::new(|_, _| {});
        let started = reg.start(
            id,
            Tool::Copilot,
            PathBuf::from("/tmp"),
            SystemTime::now(),
            cb,
            turn_cb,
        );
        assert!(started, "Copilot watcher must start");
        assert!(
            reg.inner.lock().unwrap().contains_key(&id),
            "registry should track the Copilot watcher",
        );
        // Stop it so the worker thread exits before the test ends.
        reg.stop(&id);
        assert!(
            !reg.inner.lock().unwrap().contains_key(&id),
            "registry should drop the entry on stop",
        );
    }

    #[test]
    fn registry_stop_is_idempotent_when_not_running() {
        let reg = MetricsRegistry::new();
        // Should not panic on unknown session id.
        reg.stop(&SessionId::new());
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
        let picked = newest_jsonl_after(dir.path(), UNIX_EPOCH).expect("some");
        assert_eq!(picked, b);

        // After=now+1day => nothing eligible.
        let future = SystemTime::now() + Duration::from_secs(86_400);
        assert!(newest_jsonl_after(dir.path(), future).is_none());
    }

    // -- Copilot OTel parser ---------------------------------------------

    /// Real probe data captured from `copilot -p` with the OTel file
    /// exporter enabled. Two spans (chat + invoke_agent parent) plus
    /// metric lines. Used as the canonical fixture for the parser tests.
    const COPILOT_OTEL_FIXTURE: &[u8] =
        include_bytes!("../tests/fixtures/copilot_otel_sample.jsonl");

    fn fixture_lines() -> Vec<&'static [u8]> {
        COPILOT_OTEL_FIXTURE
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .collect()
    }

    #[test]
    fn ingest_otel_chat_span_extracts_totals_and_context() {
        // Find the leaf chat span line.
        let chat_line = fixture_lines()
            .into_iter()
            .find(|l| {
                std::str::from_utf8(l)
                    .unwrap_or("")
                    .contains(r#""name":"chat "#)
            })
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
    fn ingest_otel_invoke_agent_is_ignored() {
        // The invoke_agent span carries the same gen_ai.usage.* numbers as
        // its child chat span. If we counted both we'd double-count.
        let parent_line = fixture_lines()
            .into_iter()
            .find(|l| {
                std::str::from_utf8(l)
                    .unwrap_or("")
                    .contains(r#""name":"invoke_agent""#)
            })
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
            .find(|l| {
                std::str::from_utf8(l)
                    .unwrap_or("")
                    .contains(r#""type":"metric""#)
            })
            .expect("metric line in fixture");
        let mut state = CopilotState::default();
        ingest_otel_line(metric_line, &mut state);
        assert!(!state.has_any(), "metric lines must not advance state");
    }

    #[test]
    fn ingest_otel_full_fixture_no_double_counting() {
        // Replay the entire fixture (chat span + invoke_agent + metric
        // lines). Only the chat span should contribute. This is the
        // regression assertion for the subagent / aggregate-parent shape.
        let mut state = CopilotState::default();
        for line in fixture_lines() {
            ingest_otel_line(line, &mut state);
        }
        assert_eq!(state.sum_input, 39_497, "exactly the chat span's tokens");
        assert_eq!(state.sum_output, 24);
        assert_eq!(state.token_limit, Some(168_000));
        assert_eq!(state.current_tokens, Some(29_461));
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
        let chat1 = br#"{"type":"span","name":"chat model-a","attributes":{"gen_ai.response.model":"model-a","gen_ai.usage.input_tokens":100,"gen_ai.usage.output_tokens":10},"events":[{"name":"github.copilot.session.usage_info","attributes":{"github.copilot.token_limit":1000,"github.copilot.current_tokens":500}}]}"#;
        let chat2 = br#"{"type":"span","name":"chat model-b","attributes":{"gen_ai.response.model":"model-b","gen_ai.usage.input_tokens":200,"gen_ai.usage.output_tokens":20},"events":[{"name":"github.copilot.session.usage_info","attributes":{"github.copilot.token_limit":2000,"github.copilot.current_tokens":700}}]}"#;
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
        // Totals should still update; context fields stay at their last
        // observed values (None here).
        let line = br#"{"type":"span","name":"chat foo","attributes":{"gen_ai.response.model":"foo","gen_ai.usage.input_tokens":7,"gen_ai.usage.output_tokens":3}}"#;
        let mut state = CopilotState::default();
        ingest_otel_line(line, &mut state);
        assert_eq!(state.sum_input, 7);
        assert_eq!(state.sum_output, 3);
        assert!(state.token_limit.is_none());
        assert!(state.current_tokens.is_none());
    }

    #[test]
    fn ingest_otel_falls_back_to_request_model() {
        let line = br#"{"type":"span","name":"chat fallback","attributes":{"gen_ai.request.model":"req-only","gen_ai.usage.input_tokens":1,"gen_ai.usage.output_tokens":2}}"#;
        let mut state = CopilotState::default();
        ingest_otel_line(line, &mut state);
        assert_eq!(state.last_model.as_deref(), Some("req-only"));
    }

    #[test]
    fn copilot_state_snapshot_computes_pct_from_current_tokens() {
        // Critical: pct uses current_tokens (29461) / token_limit (168000),
        // NOT sum_input. Confirms the "context_tokens_used != input_tokens"
        // invariant from the design.
        let mut state = CopilotState::default();
        for line in fixture_lines() {
            ingest_otel_line(line, &mut state);
        }
        let snap = state.snapshot(SessionId::new());
        assert_eq!(snap.context_tokens_used, Some(29_461));
        assert_eq!(snap.context_tokens_limit, Some(168_000));
        // 29461 * 100 / 168000 = 17
        assert_eq!(snap.context_used_pct, Some(17));
        // Cumulative totals are independent of the context numerator.
        assert_eq!(snap.input_tokens, Some(39_497));
        assert_eq!(snap.output_tokens, Some(24));
    }

    #[test]
    fn copilot_state_snapshot_omits_pct_without_limit() {
        let line = br#"{"type":"span","name":"chat foo","attributes":{"gen_ai.response.model":"foo","gen_ai.usage.input_tokens":1,"gen_ai.usage.output_tokens":1}}"#;
        let mut state = CopilotState::default();
        ingest_otel_line(line, &mut state);
        let snap = state.snapshot(SessionId::new());
        assert!(snap.context_used_pct.is_none());
        assert!(snap.context_tokens_used.is_none());
        assert!(snap.context_tokens_limit.is_none());
    }

    #[test]
    fn parse_invoke_agent_duration_extracts_ms() {
        // Real fixture: invoke_agent span starts at [1777474197, 905_000_000]
        // and ends at [1777474200, 749_237_700]. Difference is
        // 2_844_237_700 ns ≈ 2844 ms.
        let line = fixture_lines()
            .into_iter()
            .find(|l| {
                let needle = b"invoke_agent";
                l.starts_with(br#"{"type":"span","traceId"#)
                    && l.windows(needle.len()).any(|w| w == needle)
            })
            .expect("invoke_agent span in fixture");
        let ms = parse_invoke_agent_duration_ms(line).expect("duration parsed");
        assert!(
            (2_840..=2_850).contains(&ms),
            "duration_ms ~= 2844, got {ms}",
        );
    }

    #[test]
    fn parse_invoke_agent_duration_ignores_chat_span() {
        // chat <model> spans are excluded — they are LLM round-trips, not
        // turn boundaries.
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
        // The cheap prefilter must reject anything that doesn't even
        // mention "invoke_agent" — that's the whole point of avoiding a
        // serde_json::from_slice on the hot path.
        assert!(!maybe_invoke_agent_span(b""));
        assert!(!maybe_invoke_agent_span(b"{\"type\":\"metric\"}"));
        assert!(!maybe_invoke_agent_span(
            br#"{"type":"span","name":"chat claude-opus"}"#
        ));
    }

    #[test]
    fn maybe_invoke_agent_span_admits_real_invoke_agent_lines() {
        let line = br#"{"type":"span","name":"invoke_agent","startTime":[1,0],"endTime":[2,0]}"#;
        assert!(maybe_invoke_agent_span(line));
    }

    // -- Copilot watcher integration -------------------------------------

    /// Run a Copilot watcher against an evolving JSONL file in a tempdir.
    /// Drives state transitions by appending to the file and waiting for
    /// the callback to fire — no virtual time, but the test only sleeps
    /// long enough to clear at most a couple of poll intervals.
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
        let cb: MetricsCb = Arc::new(move |ev| {
            // Channel may be closed if the test already finished; ignore
            // send errors so the watcher thread can shut down cleanly.
            let _ = tx.send(ev);
        });
        let path_for_thread = path.clone();
        let handle = std::thread::spawn(move || {
            run_copilot_watcher(
                session_id,
                path_for_thread,
                cb,
                Arc::new(|_, _| {}),
                running_for_thread,
            );
        });

        // Append the fixture (one chat span + invoke_agent + metrics) and
        // wait for the watcher to surface a snapshot.
        std::fs::write(&path, COPILOT_OTEL_FIXTURE).unwrap();
        let snap = rx
            .recv_timeout(Duration::from_secs(8))
            .expect("watcher emitted snapshot");
        assert_eq!(snap.context_tokens_used, Some(29_461));
        assert_eq!(snap.context_tokens_limit, Some(168_000));
        assert_eq!(snap.input_tokens, Some(39_497));
        assert_eq!(snap.output_tokens, Some(24));
        assert_eq!(snap.model.as_deref(), Some("claude-opus-4.7"));

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
        let metrics_cb: MetricsCb = Arc::new(|_| {});
        let turn_cb: TurnCb = Arc::new(move |sid, dur| {
            let _ = tx.send((sid, dur));
        });
        let path_for_thread = path.clone();
        let handle = std::thread::spawn(move || {
            run_copilot_watcher(
                session_id,
                path_for_thread,
                metrics_cb,
                turn_cb,
                running_for_thread,
            );
        });

        // The full fixture contains exactly one invoke_agent span.
        std::fs::write(&path, COPILOT_OTEL_FIXTURE).unwrap();
        let (sid, dur) = rx
            .recv_timeout(Duration::from_secs(8))
            .expect("watcher emitted turn-end");
        assert_eq!(sid, session_id);
        let dur = dur.expect("invoke_agent carries a duration");
        assert!(
            (2_840..=2_850).contains(&dur),
            "expected ~2844ms duration, got {dur}",
        );

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
        let cb: MetricsCb = Arc::new(move |ev| {
            let _ = tx.send(ev);
        });
        let path_for_thread = path.clone();
        let handle = std::thread::spawn(move || {
            run_copilot_watcher(
                session_id,
                path_for_thread,
                cb,
                Arc::new(|_, _| {}),
                running_for_thread,
            );
        });

        // 1) Initial usage from the full fixture.
        std::fs::write(&path, COPILOT_OTEL_FIXTURE).unwrap();
        let _first = rx
            .recv_timeout(Duration::from_secs(8))
            .expect("first snapshot");

        // 2) Truncate to a fresh, smaller chat span. Watcher must reset.
        let smaller = br#"{"type":"span","name":"chat tiny","attributes":{"gen_ai.response.model":"tiny","gen_ai.usage.input_tokens":42,"gen_ai.usage.output_tokens":1},"events":[{"name":"github.copilot.session.usage_info","attributes":{"github.copilot.token_limit":1000,"github.copilot.current_tokens":50}}]}
"#;
        std::fs::write(&path, smaller).unwrap();

        // Drain until we get the post-truncate snapshot. The first message
        // received after the truncate should reflect the smaller numbers
        // (not the cumulative pre-truncate values).
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let snap = loop {
            assert!(std::time::Instant::now() < deadline, "timed out");
            let s = rx
                .recv_timeout(Duration::from_secs(8))
                .expect("post-truncate snapshot");
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

    // ----- read_range cap (defensive against runaway exporter) -------------

    #[test]
    fn read_range_caps_at_max_chunk_size() {
        // A pathological 50MB file. The cap should clamp the buffer to
        // MAX_READ_CHUNK and let the watcher's last-newline logic drain
        // the rest on subsequent polls.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("huge.bin");
        let total: u64 = 50 * 1024 * 1024;
        // Write a sparse-ish payload: chunks of 'a' separated by newlines
        // every 1MB so the watcher would be able to make forward progress.
        let chunk = vec![b'a'; 1024 * 1024 - 1];
        let mut f = std::fs::File::create(&path).unwrap();
        use std::io::Write;
        for _ in 0..50 {
            f.write_all(&chunk).unwrap();
            f.write_all(b"\n").unwrap();
        }
        drop(f);
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len, total);

        let bytes = read_range(&path, 0, len).expect("read");
        assert_eq!(
            bytes.len() as u64,
            MAX_READ_CHUNK,
            "read_range must cap at MAX_READ_CHUNK to bound allocation"
        );
    }

    // ----- tail_lines: oversized-line skip ---------------------------------

    /// Regression for the infinite-loop hazard noted in PR review: if a
    /// single line exceeds `MAX_READ_CHUNK`, both watchers used to be
    /// stuck forever — `rposition('\n')` would always be `None` on the
    /// capped chunk and the cursor would never move. `tail_lines` must
    /// detect "no newline + more file beyond the chunk", scan forward
    /// without buffering, and skip the oversized line.
    #[test]
    fn tail_lines_skips_line_larger_than_max_read_chunk() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("oversized.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // Oversized line: MAX_READ_CHUNK + 1MB of 'x' bytes, no \n inside.
        let oversized_len = (MAX_READ_CHUNK + 1024 * 1024) as usize;
        let oversized = vec![b'x'; oversized_len];
        f.write_all(&oversized).unwrap();
        f.write_all(b"\n").unwrap();
        // A normal line afterwards that MUST be observed by the consumer.
        let normal = br#"{"normal":true}"#;
        f.write_all(normal).unwrap();
        f.write_all(b"\n").unwrap();
        drop(f);
        let total = std::fs::metadata(&path).unwrap().len();

        let mut seen: Vec<Vec<u8>> = Vec::new();
        let mut cursor: u64 = 0;
        // Loop the watcher's poll body until the cursor reaches EOF or we
        // give up. A working implementation drains in 1–2 iterations;
        // a broken one (the bug under regression) loops forever.
        for _ in 0..10 {
            let new_cursor = tail_lines(&path, cursor, total, |line| {
                seen.push(line.to_vec());
            });
            if new_cursor == cursor {
                break;
            }
            cursor = new_cursor;
            if cursor == total {
                break;
            }
        }
        assert_eq!(cursor, total, "cursor must reach EOF (no infinite loop)");
        assert_eq!(seen.len(), 1, "oversized line skipped, only normal seen");
        assert_eq!(seen[0], normal, "the post-oversized line was delivered");
    }

    /// `tail_lines` must NOT skip a "long" line that hasn't been
    /// terminated yet — that's a partial trailing line, not an oversized
    /// one. Cursor stays put; we'll re-read after the writer commits a
    /// `\n`. The oversized-line skip only triggers when there's already
    /// more data on the far side of the cap.
    #[test]
    fn tail_lines_does_not_skip_in_flight_partial_line() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("inflight.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // Oversized partial line: > MAX_READ_CHUNK, NO trailing \n, NO
        // extra bytes after. This is "writer still in flight on a huge
        // line", which is rare but distinct from "committed oversized
        // line followed by more data".
        let partial = vec![b'x'; (MAX_READ_CHUNK + 4096) as usize];
        f.write_all(&partial).unwrap();
        drop(f);
        let len = std::fs::metadata(&path).unwrap().len();

        let mut consumed: usize = 0;
        let new_cursor = tail_lines(&path, 0, len, |_line| consumed += 1);
        assert_eq!(consumed, 0, "no complete line yet");
        assert_eq!(new_cursor, 0, "cursor must NOT advance on in-flight EOF");
    }

    /// Sanity: `tail_lines` must also drain a normal multi-line chunk
    /// in one call and leave a trailing partial in place.
    #[test]
    fn tail_lines_drains_complete_lines_and_preserves_partial() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("normal.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"line1\nline2\nline3-partial").unwrap();
        drop(f);
        let len = std::fs::metadata(&path).unwrap().len();

        let mut seen: Vec<Vec<u8>> = Vec::new();
        let new_cursor = tail_lines(&path, 0, len, |l| seen.push(l.to_vec()));
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], b"line1");
        assert_eq!(seen[1], b"line2");
        // 6 bytes for "line1\n" + 6 bytes for "line2\n" = 12.
        assert_eq!(new_cursor, 12);
    }

    // ----- Claude watcher partial-line protection --------------------------

    /// Regression for the gap noted in PR review: a half-written trailing
    /// JSON line at EOF must NOT be silently dropped. The watcher must
    /// only advance its cursor up to the last complete `\n`, so the rest
    /// of the line is re-read after the writer finishes it.
    #[test]
    fn claude_watcher_does_not_drop_partial_trailing_line() {
        use std::io::Write;
        use std::sync::mpsc;

        // Set up a fake $HOME so the watcher's project_dir resolves into
        // our tempdir. encode_cwd of the cwd we pass becomes the dir name.
        let home = tempfile::tempdir().expect("home");
        let cwd_dir = tempfile::tempdir().expect("cwd");
        let cwd = cwd_dir.path().to_path_buf();
        let project_dir = home
            .path()
            .join(".claude")
            .join("projects")
            .join(encode_cwd(&cwd));
        std::fs::create_dir_all(&project_dir).unwrap();
        let jsonl = project_dir.join("session.jsonl");
        std::fs::write(&jsonl, b"").unwrap();

        let session_id = SessionId::new();
        let running = Arc::new(AtomicBool::new(true));
        let running_for_thread = Arc::clone(&running);
        let (tx, rx) = mpsc::channel::<SessionMetricsEvent>();
        let cb: MetricsCb = Arc::new(move |ev| {
            let _ = tx.send(ev);
        });
        let home_for_thread = home.path().to_path_buf();
        let cwd_for_thread = cwd.clone();
        let spawn_instant = SystemTime::now() - Duration::from_secs(60);
        let handle = std::thread::spawn(move || {
            run_claude_watcher(
                session_id,
                home_for_thread,
                cwd_for_thread,
                spawn_instant,
                cb,
                Arc::new(|_, _| {}),
                running_for_thread,
            );
        });

        // 1) Write one complete line + a partial second line (no newline).
        let complete = br#"{"type":"assistant","message":{"model":"claude-opus-4.7","usage":{"input_tokens":10,"output_tokens":2}}}"#;
        let partial = br#"{"type":"assistant","message":{"model":"claude-opus-4.7","usage":{"input_tokens":99"#;
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&jsonl)
                .unwrap();
            f.write_all(complete).unwrap();
            f.write_all(b"\n").unwrap();
            f.write_all(partial).unwrap();
        }

        // First emission must reflect ONLY the complete line.
        let first = rx
            .recv_timeout(Duration::from_secs(8))
            .expect("first snapshot");
        assert_eq!(first.input_tokens, Some(10));
        assert_eq!(first.output_tokens, Some(2));

        // 2) Finish the partial line. The cursor must have stayed at the
        //    end of the first line, so the now-complete second line gets
        //    parsed in full and added to the totals.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&jsonl)
                .unwrap();
            f.write_all(b",\"output_tokens\":7}}}\n").unwrap();
        }

        // Drain until totals jump to 10+99 = 109 in.
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let snap = loop {
            assert!(std::time::Instant::now() < deadline, "timed out");
            let s = rx
                .recv_timeout(Duration::from_secs(8))
                .expect("post-completion snapshot");
            if s.input_tokens == Some(109) {
                break s;
            }
        };
        assert_eq!(snap.input_tokens, Some(109), "partial line was re-read");
        assert_eq!(snap.output_tokens, Some(9));

        running.store(false, Ordering::SeqCst);
        handle.join().expect("watcher thread joined");
    }

    // ----- dedup regression -------------------------------------------------

    /// Bug regression: `observed_at` must not be compared as part of the
    /// dedup check. Two snapshots with the same data but different
    /// timestamps must compare as equal payload, otherwise both watchers
    /// would emit a redundant event every POLL_INTERVAL forever.
    #[test]
    fn same_payload_as_ignores_observed_at() {
        let id = SessionId::new();
        let a = SessionMetricsEvent {
            session_id: id,
            model: Some("claude-opus-4.7".to_owned()),
            context_used_pct: Some(17),
            context_tokens_used: Some(29_461),
            context_tokens_limit: Some(168_000),
            input_tokens: Some(39_497),
            output_tokens: Some(24),
            observed_at: 1_700_000_000,
        };
        let mut b = a.clone();
        b.observed_at = 1_700_000_002; // 2 seconds later (one poll interval)
        assert!(
            a.same_payload_as(&b),
            "same data must compare equal regardless of observed_at"
        );

        // Sanity: any change in actual data flips the comparison.
        let mut c = a.clone();
        c.input_tokens = Some(39_498);
        assert!(!a.same_payload_as(&c));
    }
}
