//! Per-session token-usage / context-window watcher.
//!
//! ## Signal source (v1)
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
//! ### Limitations
//!
//! Two same-tool sessions in the same worktree cannot be disambiguated by
//! cwd + mtime alone — both would observe the most-recently-written JSONL.
//! See follow-up issue #4 for the hook-driven authoritative version.
//!
//! ## Copilot
//!
//! `~/.copilot/session-state/<sid>/events.jsonl` does **not** carry token
//! usage, so [`MetricsWatcher::start`] is a no-op for `Tool::Copilot`. A
//! follow-up will revisit this when the Copilot CLI exposes the data.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::types::{SessionId, SessionMetricsEvent, Tool};

/// Polling cadence for the watcher. Trade-off: smaller is more responsive,
/// larger is fewer syscalls.
pub const POLL_INTERVAL: Duration = Duration::from_millis(2000);

/// Callback the watcher invokes for each new (changed) snapshot. Production
/// wires this into `app.emit("session://metrics", payload)`; tests pass a
/// channel sender.
pub type MetricsCb = Arc<dyn Fn(SessionMetricsEvent) + Send + Sync>;

/// Per-session running watcher handle. Drop semantics: clearing the
/// `running` flag stops the watcher thread on its next poll iteration; the
/// thread's `JoinHandle` is detached so dropping the registry entry never
/// blocks the caller.
struct WatcherHandle {
    running: Arc<AtomicBool>,
}

/// Registry of active per-session watchers. Stored on `AppContext`. Calls
/// to [`MetricsRegistry::stop`] are idempotent — closing a session that
/// never had a watcher (e.g. a Copilot session, or a Claude session whose
/// home dir could not be resolved) is a no-op.
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
    /// (Copilot sessions, or no resolvable home dir).
    pub fn start(
        &self,
        session_id: SessionId,
        tool: Tool,
        cwd: PathBuf,
        spawn_instant: SystemTime,
        emit: MetricsCb,
    ) -> bool {
        if !matches!(tool, Tool::Claude) {
            return false;
        }
        let Some(home) = home_dir() else {
            tracing::debug!(session_id = %session_id, "no home dir; metrics watcher not started");
            return false;
        };

        // Stop any existing watcher first.
        self.stop(&session_id);

        let running = Arc::new(AtomicBool::new(true));
        let running_for_thread = Arc::clone(&running);
        let cwd_for_thread = cwd.clone();
        let join = thread::Builder::new()
            .name(format!("arborist-metrics-{}", session_id))
            .spawn(move || {
                run_claude_watcher(
                    session_id,
                    home,
                    cwd_for_thread,
                    spawn_instant,
                    emit,
                    running_for_thread,
                );
            });
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
                if len > tracked_len {
                    if let Ok(bytes) = read_range(&c, tracked_len, len) {
                        for line in bytes.split(|&b| b == b'\n') {
                            if line.is_empty() {
                                continue;
                            }
                            if let Some((usage, model)) = extract_assistant_usage(line) {
                                totals.add(&usage);
                                if let Some(m) = model {
                                    last_model = Some(m);
                                }
                            }
                        }
                    }
                    tracked_len = len;
                }
            }
        }

        if totals.has_any() {
            let limit = resolve_limit(&token_usage_path, last_model.as_deref());
            let snapshot = build_snapshot(session_id, &totals, last_model.clone(), limit);
            if Some(&snapshot) != last_emitted.as_ref() {
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
/// Code: every `\\`, `/`, and `:` in the absolute path becomes `-`. Other
/// characters pass through unchanged.
#[must_use]
pub fn encode_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(ch, '\\' | '/' | ':') {
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

fn read_range(path: &Path, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(start))?;
    let len = end.saturating_sub(start) as usize;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
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
    fn encode_cwd_idempotent_on_already_encoded() {
        // Already-encoded form has no `/`, `\`, or `:`; should pass through.
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
    fn registry_start_is_noop_for_copilot() {
        let reg = MetricsRegistry::new();
        let id = SessionId::new();
        let cb: MetricsCb = Arc::new(|_| {});
        let started = reg.start(
            id,
            Tool::Copilot,
            PathBuf::from("/tmp"),
            SystemTime::now(),
            cb,
        );
        assert!(!started, "metrics watcher must not start for Copilot");
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
}
