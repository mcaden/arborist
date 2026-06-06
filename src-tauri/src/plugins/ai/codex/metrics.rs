//! Codex metrics parsing (rollout JSONL).
//!
//! Codex persists every session as a JSONL "rollout" file under `<CODEX_HOME>/sessions/` (default `~/.codex/sessions/`), optionally nested in
//! `YYYY/MM/DD/` date subdirectories. Filenames follow `rollout-<TIMESTAMP>-<UUID>.jsonl`. The first line is a `session_meta` envelope carrying the
//! thread id and `cwd`; subsequent lines are `event_msg` / `turn_context` envelopes. The generic engine in [`crate::session_metrics`] discovers the
//! file by matching `cwd` and spawn instant (with backoff between misses), fires the AI-session discovery callback with the thread id, then tails
//! `token_count` (cumulative usage) and `turn_complete` (turn duration) events through [`CodexMetricsParser`]. `RolloutItem` is adjacently tagged
//! (`{"type":...,"payload":...}`) but the inner `EventMsg` is internally tagged, so token fields live at `payload.info.*`, not `payload.payload.info.*`.
//! Only token usage and model name are surfaced to the sidebar — feature parity with the Claude and Copilot watchers.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::session_metrics::{context_used_pct, now_unix_seconds, LocatedFile, MetricsParser, TurnCb};
use crate::types::{SessionId, SessionMetricsEvent};

/// Outer envelope for every rollout JSONL line: `{"timestamp": "...", "type": "...", "payload": {...}}`. Inner-payload shape varies by `type` and is
/// inspected as `serde_json::Value` to avoid a deserialize match per variant.
#[derive(Deserialize)]
struct CodexRolloutLine {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    payload: Option<serde_json::Value>,
}

/// Normalize a path into comparable components for cross-platform matching. On Windows, slashes are unified and each component is lowercased so
/// `C:\Repos\X` and `c:/repos/x/` normalize to the same `Vec<String>`. On Unix, components are preserved as-is.
fn normalize_path_components(path: &Path) -> Vec<String> {
    path.components()
        .map(|c| {
            let raw = c.as_os_str().to_string_lossy();
            if cfg!(windows) {
                raw.replace('/', "\\").to_ascii_lowercase()
            } else {
                raw.into_owned()
            }
        })
        .collect()
}

/// Discover the newest rollout JSONL under `sessions_dir` (including date-nested subdirs) whose first-line `SessionMeta.cwd` matches `cwd` and whose
/// mtime is >= `after`. Returns the full path when found. Called during rollout discovery while no file is bound; the engine applies a backoff
/// between misses to avoid re-scanning the entire Codex sessions tree on every poll tick.
fn newest_codex_rollout(sessions_dir: &Path, cwd: &Path, after: SystemTime) -> Option<PathBuf> {
    let slack = Duration::from_secs(5);
    let cutoff = after.checked_sub(slack).unwrap_or(after);
    let expected_norm = normalize_path_components(cwd);
    let mut best: Option<(SystemTime, PathBuf)> = None;

    // Helper to check a single directory for rollout files.
    fn scan_dir(dir: &Path, expected_norm: &[String], cutoff: SystemTime, best: &mut Option<(SystemTime, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !fname.starts_with("rollout-") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            if mtime < cutoff {
                continue;
            }
            // Check if the cwd matches by reading the first line (SessionMeta).
            if !codex_rollout_cwd_matches_norm(&path, expected_norm) {
                continue;
            }
            match best {
                Some((bt, _)) if *bt >= mtime => {}
                _ => *best = Some((mtime, path)),
            }
        }
    }

    // Scan top-level `sessions/` directory.
    scan_dir(sessions_dir, &expected_norm, cutoff, &mut best);

    // Also scan three-level nested subdirs (Codex commonly uses YYYY/MM/DD).
    if let Ok(years) = std::fs::read_dir(sessions_dir) {
        for year_entry in years.flatten() {
            let year_path = year_entry.path();
            if !year_path.is_dir() {
                continue;
            }
            if let Ok(months) = std::fs::read_dir(&year_path) {
                for month_entry in months.flatten() {
                    let month_path = month_entry.path();
                    if !month_path.is_dir() {
                        continue;
                    }
                    if let Ok(days) = std::fs::read_dir(&month_path) {
                        for day_entry in days.flatten() {
                            let day_path = day_entry.path();
                            if day_path.is_dir() {
                                scan_dir(&day_path, &expected_norm, cutoff, &mut best);
                            }
                        }
                    }
                }
            }
        }
    }

    best.map(|(_, p)| p)
}

/// Read a rollout file's first line and decode it as a `session_meta` `CodexRolloutLine`. Returns the payload (a JSON object) when the line is
/// well-formed and the type matches; `None` otherwise. Shared by `codex_rollout_cwd_matches_norm` and `codex_rollout_thread_id`.
fn read_codex_session_meta(path: &Path) -> Option<serde_json::Value> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(f);
    let mut first_line = String::new();
    reader.read_line(&mut first_line).ok()?;
    if first_line.is_empty() {
        return None;
    }
    let line: CodexRolloutLine = serde_json::from_str(&first_line).ok()?;
    if line.r#type != "session_meta" {
        return None;
    }
    line.payload
}

/// Check whether a rollout file's `SessionMeta.cwd` matches the (already-normalized) expected cwd.
fn codex_rollout_cwd_matches_norm(path: &Path, expected_norm: &[String]) -> bool {
    let Some(payload) = read_codex_session_meta(path) else { return false };
    let Some(cwd_str) = payload.get("cwd").and_then(|v| v.as_str()) else {
        return false;
    };
    normalize_path_components(Path::new(cwd_str)) == expected_norm
}

/// Test-only wrapper around [`codex_rollout_cwd_matches_norm`] that normalizes `expected_cwd` per call. Kept for the unit tests that don't want to
/// pre-normalize.
#[cfg(test)]
fn codex_rollout_cwd_matches(path: &Path, expected_cwd: &Path) -> bool {
    codex_rollout_cwd_matches_norm(path, &normalize_path_components(expected_cwd))
}

/// Extract the thread_id from the first line of a Codex rollout file.
fn codex_rollout_thread_id(path: &Path) -> Option<String> {
    let payload = read_codex_session_meta(path)?;
    payload
        .get("thread_id")
        .or_else(|| payload.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
}

/// Codex rollout watcher state accumulated from `TokenCount` and `TurnContext` events.
#[derive(Debug, Default)]
struct CodexState {
    /// Cumulative input tokens (from total_token_usage.input_tokens).
    sum_input: u64,
    /// Cumulative output tokens (from total_token_usage.output_tokens).
    sum_output: u64,
    /// Model context window limit (from model_context_window or TurnStarted).
    context_window: Option<u64>,
    /// Current tokens occupying the context window — sourced from `last_token_usage.total_tokens` (the most recent turn),
    /// NOT the cumulative session counter `total_token_usage`. This mirrors Codex's own status card, which drives the
    /// context gauge from `last_token_usage`; using the cumulative total would pin the gauge at 100% after enough turns.
    context_tokens_used: Option<u64>,
    /// Most recent model name (from TurnContext).
    last_model: Option<String>,
    /// True once at least one TokenCount event has been ingested.
    seen: bool,
}

impl CodexState {
    fn has_any(&self) -> bool {
        self.seen
    }

    fn snapshot(&self, session_id: SessionId) -> SessionMetricsEvent {
        let used = self.context_tokens_used;
        let pct = context_used_pct(used, self.context_window);
        SessionMetricsEvent {
            session_id,
            model: self.last_model.clone(),
            context_used_pct: pct,
            context_tokens_used: used,
            context_tokens_limit: self.context_window,
            input_tokens: Some(self.sum_input),
            output_tokens: Some(self.sum_output),
            observed_at: now_unix_seconds(),
        }
    }
}

/// Extract token-count fields from a `token_count` event's `info` object. Returns a tuple of
/// `(input, output, context_tokens, model_context_window)`, each `Some` when present and well-typed. Missing info returns
/// an all-`None` tuple early.
///
/// Codex's `EventMsg` is *internally* tagged, so `TokenCountEvent`'s fields are inlined directly under the rollout
/// line's `payload` (i.e. `payload.info`). We still accept the legacy `payload.payload.info` shape defensively in case
/// an older CLI build emitted an extra wrapper.
///
/// `input`/`output` come from `total_token_usage` (Codex's cumulative session counters), while `context_tokens` — the
/// value that drives the context-window gauge — comes from `last_token_usage.total_tokens` (the live occupancy of the
/// window), falling back to the cumulative total only if `last_token_usage` is absent.
fn extract_codex_token_count(payload: &serde_json::Value) -> (Option<u64>, Option<u64>, Option<u64>, Option<u64>) {
    let Some(info) = payload.get("info").or_else(|| payload.get("payload").and_then(|p| p.get("info"))) else {
        return (None, None, None, None);
    };
    let total = info.get("total_token_usage");
    let input = total.and_then(|t| t.get("input_tokens")).and_then(|v| v.as_u64());
    let output = total.and_then(|t| t.get("output_tokens")).and_then(|v| v.as_u64());
    let context_tokens = info
        .get("last_token_usage")
        .and_then(|t| t.get("total_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| total.and_then(|t| t.get("total_tokens")).and_then(|v| v.as_u64()));
    let mcw = info.get("model_context_window").and_then(|v| v.as_u64()).filter(|&v| v > 0);
    (input, output, context_tokens, mcw)
}

/// Ingest a single Codex rollout JSONL line. Extracts token usage from `EventMsg::TokenCount` and model from `TurnContext`.
fn ingest_codex_rollout_line(line: &[u8], state: &mut CodexState) {
    // Lines in the rollout are `{"timestamp":"...","type":"<variant>","payload":{...}}`.
    // We care about:
    // - type = "event_msg" with payload.type = "token_count" → token usage
    // - type = "turn_context" → model name
    //
    // Codex's `RolloutItem` is adjacently tagged (`tag = "type", content = "payload"`) while the inner `EventMsg` is
    // internally tagged (`tag = "type"`, no content), and both use `rename_all = "snake_case"`. So the real event tag is
    // `token_count` / `task_started`, not the PascalCase `TokenCount` / `TurnStarted`. We accept both spellings so a
    // format change in either direction doesn't silently zero out the sidebar.
    let Ok(outer) = serde_json::from_slice::<CodexRolloutLine>(line) else {
        return;
    };

    match outer.r#type.as_str() {
        "event_msg" => {
            let Some(payload) = outer.payload else { return };
            // EventMsg is tagged: {"type":"token_count", ...inlined TokenCountEvent fields}
            let Some(event_type) = payload.get("type").and_then(|v| v.as_str()) else {
                return;
            };
            match event_type {
                "token_count" | "TokenCount" => {
                    let (input, output, total, mcw) = extract_codex_token_count(&payload);
                    if let Some(v) = input {
                        state.sum_input = v;
                    }
                    if let Some(v) = output {
                        state.sum_output = v;
                    }
                    if let Some(v) = total {
                        state.context_tokens_used = Some(v);
                    }
                    if let Some(v) = mcw {
                        state.context_window = Some(v);
                    }
                    state.seen = true;
                }
                "task_started" | "turn_started" | "TurnStarted" => {
                    // `TurnStartedEvent.model_context_window` is inlined under `payload` (internally-tagged EventMsg); the
                    // legacy `payload.payload.model_context_window` shape is accepted defensively.
                    if let Some(mcw) = payload
                        .get("model_context_window")
                        .or_else(|| payload.get("payload").and_then(|p| p.get("model_context_window")))
                        .and_then(|v| v.as_u64())
                        .filter(|&v| v > 0)
                    {
                        state.context_window = Some(mcw);
                    }
                }
                _ => {}
            }
        }
        "turn_context" => {
            let Some(payload) = outer.payload else { return };
            if let Some(model) = payload.get("model").and_then(|v| v.as_str()) {
                if !model.is_empty() {
                    state.last_model = Some(model.to_owned());
                }
            }
        }
        _ => {}
    }
}

/// Cheap byte-level prefilter to detect a `TurnComplete` event in a Codex rollout line. Avoids full JSON parse on the majority of lines.
fn maybe_codex_turn_complete(line: &[u8]) -> bool {
    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.len() >= needle.len() && hay.windows(needle.len()).any(|w| w == needle)
    }
    contains(line, b"\"TurnComplete\"") || contains(line, b"\"task_complete\"") || contains(line, b"\"turn_complete\"")
}

/// Parse a Codex turn-complete event.
///
/// Returns:
/// - `Some(Some(duration_ms))` for a real turn-complete event with duration
/// - `Some(None)` for a real turn-complete event without duration
/// - `None` when the line is not a turn-complete event (or cannot be parsed)
fn parse_codex_turn_duration_ms(line: &[u8]) -> Option<Option<u64>> {
    let outer: CodexRolloutLine = serde_json::from_slice(line).ok()?;
    if outer.r#type != "event_msg" {
        return None;
    }
    let payload = outer.payload?;
    let event_type = payload.get("type").and_then(|v| v.as_str())?;
    if event_type != "TurnComplete" && event_type != "task_complete" && event_type != "turn_complete" {
        return None;
    }
    let duration_ms = payload
        .get("duration_ms")
        .or_else(|| payload.get("payload").and_then(|inner| inner.get("duration_ms")))
        .and_then(|v| v.as_u64());
    Some(duration_ms)
}

/// Codex rollout metrics parser. Owns the rollout-tree scan (binding by `cwd` + spawn instant) and the token accumulation; the engine owns the
/// polling/tailing loop, the discovery backoff timer, and rebind-on-disappearance.
pub struct CodexMetricsParser {
    sessions_dir: PathBuf,
    cwd: PathBuf,
    state: CodexState,
}

impl CodexMetricsParser {
    #[must_use]
    pub fn new(codex_home: &Path, cwd: &Path) -> Self {
        Self {
            sessions_dir: codex_home.join("sessions"),
            cwd: cwd.to_path_buf(),
            state: CodexState::default(),
        }
    }
}

impl MetricsParser for CodexMetricsParser {
    fn relocate_each_poll(&self) -> bool {
        // Codex shares one `~/.codex/sessions/` tree across projects and nests files in dated subdirs, so a cold scan is O(N) over rollout history.
        // The engine binds once and only rediscovers after the bound file disappears, applying backoff between misses.
        false
    }

    fn rebind_on_disappear(&self) -> bool {
        true
    }

    fn locate(&mut self, spawn_instant: SystemTime) -> Option<LocatedFile> {
        let path = newest_codex_rollout(&self.sessions_dir, &self.cwd, spawn_instant)?;
        let ai_session_id = codex_rollout_thread_id(&path);
        Some(LocatedFile { path, ai_session_id })
    }

    fn reset(&mut self) {
        self.state = CodexState::default();
    }

    fn ingest_line(&mut self, line: &[u8], session_id: SessionId, emit_turn: &TurnCb) {
        ingest_codex_rollout_line(line, &mut self.state);
        if maybe_codex_turn_complete(line) {
            if let Some(duration) = parse_codex_turn_duration_ms(line) {
                emit_turn(session_id, duration);
            }
        }
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

    #[test]
    fn ingest_codex_token_count_event() {
        // Real Codex format: `RolloutItem` is adjacently tagged (`payload`), `EventMsg` is internally tagged
        // (`token_count`), so `info` is inlined directly under `payload` — there is no second `payload` wrapper.
        // Context occupancy is the LAST turn's total (950), while input/output are the cumulative session totals.
        let line = br#"{"timestamp":"2025-05-18T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1500,"cached_input_tokens":200,"output_tokens":800,"reasoning_output_tokens":100,"total_tokens":2600},"last_token_usage":{"input_tokens":500,"cached_input_tokens":100,"output_tokens":300,"reasoning_output_tokens":50,"total_tokens":950},"model_context_window":128000},"rate_limits":null}}"#;
        let mut state = CodexState::default();
        ingest_codex_rollout_line(line, &mut state);
        assert!(state.seen);
        assert_eq!(state.sum_input, 1500);
        assert_eq!(state.sum_output, 800);
        assert_eq!(state.context_tokens_used, Some(950));
        assert_eq!(state.context_window, Some(128000));
    }

    #[test]
    fn ingest_codex_token_count_event_legacy_double_payload() {
        // Defensive: an older/alternative shape with PascalCase tag and a nested `payload.payload.info` wrapper.
        let line = br#"{"timestamp":"2025-05-18T10:00:00Z","type":"event_msg","payload":{"type":"TokenCount","payload":{"info":{"total_token_usage":{"input_tokens":1500,"cached_input_tokens":200,"output_tokens":800,"reasoning_output_tokens":100,"total_tokens":2600},"last_token_usage":{"input_tokens":500,"cached_input_tokens":100,"output_tokens":300,"reasoning_output_tokens":50,"total_tokens":950},"model_context_window":128000}}}}"#;
        let mut state = CodexState::default();
        ingest_codex_rollout_line(line, &mut state);
        assert!(state.seen);
        assert_eq!(state.sum_input, 1500);
        assert_eq!(state.sum_output, 800);
        assert_eq!(state.context_tokens_used, Some(950));
        assert_eq!(state.context_window, Some(128000));
    }

    #[test]
    fn ingest_codex_context_occupancy_tracks_last_turn_not_cumulative() {
        // Across turns, `total_token_usage` accumulates without bound while `last_token_usage` reflects the live window.
        // The context gauge must follow the latter, so it can never get pinned at 100% once the session exceeds the window.
        let window = 100_000u64;
        let turn1 = br#"{"timestamp":"t1","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":60000,"output_tokens":20000,"total_tokens":80000},"last_token_usage":{"input_tokens":60000,"output_tokens":20000,"total_tokens":80000},"model_context_window":100000}}}"#;
        // After a /compact the cumulative session total (190000) exceeds the window, but only 30000 tokens occupy it.
        let turn2 = br#"{"timestamp":"t2","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":140000,"output_tokens":50000,"total_tokens":190000},"last_token_usage":{"input_tokens":25000,"output_tokens":5000,"total_tokens":30000},"model_context_window":100000}}}"#;
        let mut state = CodexState::default();
        ingest_codex_rollout_line(turn1, &mut state);
        ingest_codex_rollout_line(turn2, &mut state);
        assert_eq!(state.context_tokens_used, Some(30_000));
        assert_eq!(state.context_window, Some(window));
        let snap = state.snapshot(SessionId::new());
        assert_eq!(snap.context_used_pct, Some(30));
        // Cumulative session totals still surface for the input/output displays.
        assert_eq!(snap.input_tokens, Some(140_000));
        assert_eq!(snap.output_tokens, Some(50_000));
    }

    #[test]
    fn ingest_codex_turn_context_extracts_model() {
        let line = br#"{"timestamp":"2025-05-18T10:00:00Z","type":"turn_context","payload":{"model":"o3-mini","cwd":"/tmp","approval_policy":"OnRequest","sandbox_policy":{"type":"workspace-write"},"summary":"auto"}}"#;
        let mut state = CodexState::default();
        ingest_codex_rollout_line(line, &mut state);
        assert_eq!(state.last_model.as_deref(), Some("o3-mini"));
    }

    #[test]
    fn ingest_codex_turn_started_extracts_context_window() {
        // Real Codex format: `task_started` tag, `model_context_window` inlined under `payload`.
        let line = br#"{"timestamp":"2025-05-18T10:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"abc","model_context_window":200000}}"#;
        let mut state = CodexState::default();
        ingest_codex_rollout_line(line, &mut state);
        assert_eq!(state.context_window, Some(200000));
    }

    #[test]
    fn ingest_codex_turn_started_extracts_context_window_legacy() {
        // Defensive: PascalCase tag with a nested `payload.payload.model_context_window`.
        let line = br#"{"timestamp":"2025-05-18T10:00:00Z","type":"event_msg","payload":{"type":"TurnStarted","payload":{"turn_id":"abc","model_context_window":200000}}}"#;
        let mut state = CodexState::default();
        ingest_codex_rollout_line(line, &mut state);
        assert_eq!(state.context_window, Some(200000));
    }

    #[test]
    fn codex_state_snapshot_computes_pct() {
        let state = CodexState {
            sum_input: 1000,
            sum_output: 500,
            context_window: Some(100_000),
            context_tokens_used: Some(50_000),
            last_model: Some("o3".into()),
            seen: true,
        };
        let snap = state.snapshot(SessionId::new());
        assert_eq!(snap.context_used_pct, Some(50));
        assert_eq!(snap.model.as_deref(), Some("o3"));
        assert_eq!(snap.input_tokens, Some(1000));
        assert_eq!(snap.output_tokens, Some(500));
    }

    #[test]
    fn codex_state_snapshot_pct_handles_large_values_without_overflow() {
        let state = CodexState {
            sum_input: 1000,
            sum_output: 500,
            context_window: Some(10_000_000_000_000_000_000),
            context_tokens_used: Some(5_000_000_000_000_000_000),
            last_model: Some("o3".into()),
            seen: true,
        };
        let snap = state.snapshot(SessionId::new());
        assert_eq!(snap.context_used_pct, Some(50));
    }

    #[test]
    fn parse_codex_turn_complete_duration() {
        // Real Codex format: `turn_complete` tag, `duration_ms` inlined under `payload`.
        let line = br#"{"timestamp":"2025-05-18T10:00:00Z","type":"event_msg","payload":{"type":"turn_complete","turn_id":"t1","duration_ms":4200}}"#;
        assert_eq!(parse_codex_turn_duration_ms(line), Some(Some(4200)));
    }

    #[test]
    fn parse_codex_turn_complete_duration_legacy_double_payload() {
        // Defensive: PascalCase tag with a nested `payload.payload.duration_ms`.
        let line = br#"{"timestamp":"2025-05-18T10:00:00Z","type":"event_msg","payload":{"type":"TurnComplete","payload":{"turn_id":"t1","last_agent_message":"done","duration_ms":4200}}}"#;
        assert_eq!(parse_codex_turn_duration_ms(line), Some(Some(4200)));
    }

    #[test]
    fn parse_codex_turn_complete_no_duration() {
        let line = br#"{"timestamp":"2025-05-18T10:00:00Z","type":"event_msg","payload":{"type":"turn_complete","turn_id":"t1"}}"#;
        assert_eq!(parse_codex_turn_duration_ms(line), Some(None));
    }

    #[test]
    fn parse_codex_turn_complete_ignores_non_turn_complete_event_with_keyword() {
        let line = br#"{"timestamp":"2025-05-18T10:00:00Z","type":"event_msg","payload":{"type":"token_count","note":"TurnComplete"}}"#;
        assert!(maybe_codex_turn_complete(line));
        assert_eq!(parse_codex_turn_duration_ms(line), None);
    }

    #[test]
    fn codex_rollout_cwd_match_and_thread_id_extraction() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rollout-2025-05-18T10-00-00-abcdef12-3456-7890-abcd-ef1234567890.jsonl");
        let cwd = if cfg!(windows) { r"C:\repos\myproject" } else { "/repos/myproject" };
        let first_line = format!(
            r#"{{"timestamp":"2025-05-18T10:00:00Z","type":"session_meta","payload":{{"id":"thread-uuid-123","cwd":"{}","originator":"codex","cli_version":"1.0","source":"cli"}}}}"#,
            cwd.replace('\\', "\\\\")
        );
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", first_line).unwrap();
        drop(f);

        assert!(codex_rollout_cwd_matches(&path, Path::new(cwd)));
        assert!(!codex_rollout_cwd_matches(&path, Path::new("/other/path")));
        assert_eq!(codex_rollout_thread_id(&path), Some("thread-uuid-123".to_string()));
    }

    #[cfg(windows)]
    #[test]
    fn codex_rollout_cwd_match_normalizes_windows_separators_and_trailing_separator() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rollout-2025-05-18T10-00-00-abcdef12-3456-7890-abcd-ef1234567890.jsonl");
        let expected_cwd = r"C:\repos\myproject";
        let rollout_cwd = format!("{}/", expected_cwd.replace('\\', "/"));
        let first_line = format!(
            r#"{{"timestamp":"2025-05-18T10:00:00Z","type":"session_meta","payload":{{"id":"thread-uuid-123","cwd":"{}","originator":"codex","cli_version":"1.0","source":"cli"}}}}"#,
            rollout_cwd
        );
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", first_line).unwrap();
        drop(f);

        assert!(codex_rollout_cwd_matches(&path, Path::new(expected_cwd)));
    }

    #[test]
    fn codex_rollout_thread_id_requires_session_meta() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rollout-2025-05-18T10-00-00-abcdef12-3456-7890-abcd-ef1234567890.jsonl");
        let first_line = r#"{"timestamp":"2025-05-18T10:00:00Z","type":"event_msg","payload":{"id":"not-a-thread-id"}}"#;
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", first_line).unwrap();
        drop(f);

        assert_eq!(codex_rollout_thread_id(&path), None);
    }

    #[test]
    fn codex_rollout_thread_id_accepts_thread_id_field() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rollout-2025-05-18T10-00-00-abcdef12-3456-7890-abcd-ef1234567890.jsonl");
        let first_line = r#"{"timestamp":"2025-05-18T10:00:00Z","type":"session_meta","payload":{"thread_id":"thread-from-thread-id","cwd":"/tmp"}}"#;
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", first_line).unwrap();
        drop(f);

        assert_eq!(codex_rollout_thread_id(&path), Some("thread-from-thread-id".to_owned()));
    }

    #[test]
    fn newest_codex_rollout_picks_matching_cwd() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let cwd = if cfg!(windows) { r"C:\repos\target" } else { "/repos/target" };
        let other_cwd = if cfg!(windows) { r"C:\repos\other" } else { "/repos/other" };

        // Create a rollout matching our cwd.
        let good_path = sessions_dir.join("rollout-2025-05-18T10-00-00-11111111-1111-1111-1111-111111111111.jsonl");
        let good_line = format!(
            r#"{{"timestamp":"2025-05-18T10:00:00Z","type":"session_meta","payload":{{"id":"good-thread","cwd":"{}","originator":"codex","cli_version":"1.0","source":"cli"}}}}"#,
            cwd.replace('\\', "\\\\")
        );
        let mut f = std::fs::File::create(&good_path).unwrap();
        writeln!(f, "{}", good_line).unwrap();
        drop(f);

        // Create a rollout with a different cwd.
        let bad_path = sessions_dir.join("rollout-2025-05-18T10-00-01-22222222-2222-2222-2222-222222222222.jsonl");
        let bad_line = format!(
            r#"{{"timestamp":"2025-05-18T10:00:01Z","type":"session_meta","payload":{{"id":"bad-thread","cwd":"{}","originator":"codex","cli_version":"1.0","source":"cli"}}}}"#,
            other_cwd.replace('\\', "\\\\")
        );
        let mut f = std::fs::File::create(&bad_path).unwrap();
        writeln!(f, "{}", bad_line).unwrap();
        drop(f);

        let after = SystemTime::now() - Duration::from_secs(60);
        let result = newest_codex_rollout(&sessions_dir, Path::new(cwd), after);
        assert_eq!(result, Some(good_path));
    }

    /// Engine integration: the codex parser drives the generic watcher through a discovery miss (no rollout exists yet), then binds the freshly
    /// created rollout, fires AI-session discovery with the thread id, and emits a metrics snapshot. Exercises the codex-only engine branches
    /// (non-relocate bind-once-with-backoff, discovery miss, bind-on-appear, thread-id discovery from the filename/meta) that the parser unit tests
    /// don't cover — this is the regression guard that the metrics-parser refactor didn't break codex end-to-end.
    #[test]
    fn codex_watcher_discovers_rollout_and_emits_snapshot() {
        use std::io::Write;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{mpsc, Arc};

        let dir = tempfile::tempdir().expect("tempdir");
        let codex_home = dir.path().to_path_buf();
        let sessions_dir = codex_home.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let cwd = if cfg!(windows) { r"C:\repos\codex-proj" } else { "/repos/codex-proj" };

        let session_id = SessionId::new();
        let running = Arc::new(AtomicBool::new(true));
        let running_for_thread = Arc::clone(&running);
        let (tx, rx) = mpsc::channel::<SessionMetricsEvent>();
        let (disc_tx, disc_rx) = mpsc::channel::<(SessionId, String)>();
        let metrics_cb: crate::session_metrics::MetricsCb = Arc::new(move |ev| {
            let _ = tx.send(ev);
        });
        let discover_cb: crate::session_metrics::AiSessionDiscoveryCb = Arc::new(move |sid, id| {
            let _ = disc_tx.send((sid, id));
        });
        let codex_home_for_thread = codex_home.clone();
        let cwd_for_thread = cwd.to_owned();
        let handle = std::thread::spawn(move || {
            let parser = Box::new(CodexMetricsParser::new(&codex_home_for_thread, Path::new(&cwd_for_thread)));
            crate::session_metrics::run_metrics_watcher(
                session_id,
                parser,
                SystemTime::now() - Duration::from_secs(60),
                metrics_cb,
                Arc::new(|_, _| {}),
                discover_cb,
                running_for_thread,
            );
        });

        // The first poll(s) find no matching rollout (discovery miss). Then create one: `session_meta` (cwd match + thread id) followed by a
        // `token_count` event the parser should ingest into a snapshot.
        let rollout = sessions_dir.join("rollout-2025-05-18T10-00-00-33333333-3333-3333-3333-333333333333.jsonl");
        let session_meta = format!(
            r#"{{"timestamp":"2025-05-18T10:00:00Z","type":"session_meta","payload":{{"id":"codex-thread-xyz","cwd":"{}","originator":"codex","cli_version":"1.0","source":"cli"}}}}"#,
            cwd.replace('\\', "\\\\")
        );
        let token_count = r#"{"timestamp":"2025-05-18T10:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1500,"output_tokens":800,"total_tokens":2600},"last_token_usage":{"input_tokens":500,"output_tokens":300,"total_tokens":950},"model_context_window":128000}}}"#;
        let mut f = std::fs::File::create(&rollout).unwrap();
        writeln!(f, "{}", session_meta).unwrap();
        writeln!(f, "{}", token_count).unwrap();
        drop(f);

        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        let snap = loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(s) if s.context_tokens_used == Some(950) => break s,
                Ok(_) => continue,
                Err(_) => panic!("timed out waiting for codex snapshot (context_tokens_used never reached 950)"),
            }
        };
        assert_eq!(snap.context_tokens_used, Some(950));
        assert_eq!(snap.context_tokens_limit, Some(128_000));
        assert_eq!(snap.input_tokens, Some(1500));
        assert_eq!(snap.output_tokens, Some(800));

        // The engine must surface the rollout thread id so restore-on-launch can `codex resume <id>`.
        let (disc_sid, disc_id) = disc_rx.recv_timeout(Duration::from_secs(2)).expect("discovery callback fired");
        assert_eq!(disc_sid, session_id);
        assert_eq!(disc_id, "codex-thread-xyz");

        running.store(false, Ordering::SeqCst);
        handle.join().expect("watcher thread joined");
    }
}
