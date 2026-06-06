//! Per-session token-usage / context-window watcher.
//!
//! ## Responsibilities
//!
//! This module owns the **generic** parts of the metrics pipeline: thread lifecycle ([`MetricsRegistry`]), polling cadence ([`POLL_INTERVAL`]),
//! file tailing with truncation/rotation handling ([`tail_lines`]), file-discovery backoff timing, snapshot deduplication, and callback plumbing
//! (`session://metrics`, `session://activity` turn-end, AI-session discovery).
//!
//! All **format-specific** knowledge — where each tool writes its metrics, how to read a session id out of the file, how to parse a line, and how to
//! build a snapshot — lives behind the [`MetricsParser`] trait, implemented per tool in the plugin modules
//! (`plugins::ai::{claude,copilot,codex}::metrics`). The generic engine [`run_metrics_watcher`] drives one parser; adding a new AI tool means
//! dropping in a plugin + parser, not editing this file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::pty_pool::ActivityCb;
use crate::types::{SessionId, SessionMetricsEvent, Tool};

/// Polling cadence for the watcher. Trade-off: smaller is more responsive, larger is fewer syscalls.
pub const POLL_INTERVAL: Duration = Duration::from_millis(2000);
/// Minimum interval between file-discovery scans while a watcher is unbound (Codex's shared sessions tree, Copilot's first-poll bind).
const DISCOVERY_SCAN_MIN_INTERVAL: Duration = POLL_INTERVAL;
/// Maximum backoff interval between file-discovery scans while a watcher remains unbound.
const DISCOVERY_SCAN_MAX_INTERVAL: Duration = Duration::from_secs(30);

/// Callback the watcher invokes for each new (changed) snapshot. Production wires this into `app.emit("session://metrics", payload)`; tests pass a
/// channel sender.
pub type MetricsCb = Arc<dyn Fn(SessionMetricsEvent) + Send + Sync>;

/// Callback the watcher invokes when it discovers (or learns of a change to) the AI-side session id for an Arborist session. Production wires this
/// into `ConfigStore::update_session_ai_session_id` so the next app-restart restore can inject the tool's resume token (`--resume <id>` for Claude,
/// `--session-id <id>` for Copilot, `resume <id>` subcommand for Codex) and continue the AI conversation. Tests substitute a capturing closure.
///
/// Idempotent: the watcher fires this on every detected change, but `update_session_ai_session_id` is a no-op when the value already matches.
pub type AiSessionDiscoveryCb = Arc<dyn Fn(SessionId, String) + Send + Sync>;

/// Callback the watcher invokes when an agent turn completes. Production wires this into `app.emit("session://activity", { kind: "turnEnd", ... })`
/// via [`crate::activity::ActivityEvent::TurnEnd`]. Tests pass a channel sender.
///
/// `duration_ms` is the wall-clock duration of the turn when the source reports it (Copilot OTel `invoke_agent` span), or `None` when it does not
/// (Claude transcript — we only see per-message timestamps, not a reliable turn-start marker).
pub type TurnCb = Arc<dyn Fn(SessionId, Option<u64>) + Send + Sync>;

/// Per-session running watcher handle. Drop semantics: clearing the `running` flag stops the watcher thread on its next poll iteration. We also
/// retain the `JoinHandle` so callers that need a quiescence guarantee (e.g. `session_restart_impl`, which clears `Session.ai_session_id` and must
/// ensure no late discovery callback can persist the stale id back) can use [`MetricsRegistry::stop_and_join`].
///
/// `extra_joins` carries sibling watchers that share the same `running` flag — currently only the Copilot events.jsonl tailer
/// ([`crate::copilot_events::run_watcher`]). One flag drives stop for every sibling so a session can never be left with one watcher live and another
/// stopped.
struct WatcherHandle {
    running: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
    extra_joins: Vec<thread::JoinHandle<()>>,
}

/// Registry of active per-session watchers. Stored on `AppContext`. Calls to [`MetricsRegistry::stop`] are idempotent — closing a session whose
/// watcher could not be started in the first place (e.g. a Claude session whose home dir could not be resolved, or any session for which `start`
/// returned `false`) is a no-op. **Both** Claude and Copilot sessions get watchers when their inputs are available — Claude via transcript tailing,
/// Copilot via OTel JSONL tailing.
#[derive(Default)]
pub struct MetricsRegistry {
    inner: Mutex<HashMap<SessionId, WatcherHandle>>,
}

impl MetricsRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a watcher for `session_id`. If one is already running it is stopped first so the new one starts from a clean slate (used by session
    /// restart). Returns `false` if no watcher could be started (e.g. Claude session whose home dir could not be resolved).
    ///
    /// For Copilot sessions with a known `ai_session_id` (pre-allocated at create / restart time), a sibling events.jsonl tailer is
    /// also spawned (see
    /// [`crate::copilot_events`]). It feeds richer per-state events
    /// (`AwaitingPermission`, `ToolStart`/`ToolEnd`, `TurnStart`) into the same `session://activity` channel via `activity_emit`. Failure to spawn
    /// the events tailer is non-fatal — the metrics watcher still runs.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &self,
        session_id: SessionId,
        tool: Tool,
        cwd: PathBuf,
        spawn_instant: SystemTime,
        emit: MetricsCb,
        emit_turn: TurnCb,
        discover: AiSessionDiscoveryCb,
        activity_emit: ActivityCb,
        ai_session_id: Option<String>,
    ) -> bool {
        // Stop any existing watcher first so the per-tool worker starts from a clean slate (used by session restart).
        self.stop(&session_id);

        let Some(parser) = crate::plugins::ai::metrics_parser(tool, session_id, &cwd, spawn_instant) else {
            tracing::debug!(session_id = %session_id, tool = ?tool, "AI metrics watcher not started; no parser available");
            return false;
        };

        let running = Arc::new(AtomicBool::new(true));
        let running_for_thread = Arc::clone(&running);
        let join = thread::Builder::new().name(format!("arborist-metrics-{}", session_id)).spawn(move || {
            run_metrics_watcher(session_id, parser, spawn_instant, emit, emit_turn, discover, running_for_thread);
        });
        match join {
            Ok(handle) => {
                // For tools that arm an activity-events tailer (Copilot today, Claude when the hook helper was wired in at compose time), dispatch
                // through `activity_events_kind` to resolve the per-tool tailer flavour + path. The watcher thread is spawned with the same
                // `running` flag and parked in `extra_joins` so `stop` / `stop_and_join` / `stop_all_and_join` tear down everything together.
                //
                // Gating note: when the plugin advertises a per-session settings file via `settings_file_path` (i.e. Claude), we additionally
                // require that file to exist, parse as JSON, and contain an Arborist-owned hook entry whose command path exists *and* whose args
                // include this session id + hook-events path — see [`crate::claude_hook_events::settings_file_hook_events_path`]. Plain `.exists()`
                // on the settings file is not enough
                // because on restart/restore we replay `materialise_temp_files(&session.temp_files)`, so a settings file persisted from a previous
                // install (or before the helper was moved/uninstalled/repackaged) will be on disk even when the helper itself isn't reachable.
                // Falling any of those checks disables the watcher so we don't park a per-session polling thread on a `hook-events.jsonl` no one
                // will write to.
                let mut extra_joins: Vec<thread::JoinHandle<()>> = Vec::new();
                if crate::plugins::ai::starts_activity_events_watcher(tool) {
                    // For Claude, resolve the hook-events path directly from the validated Arborist hook entry in the settings JSON so we tail the
                    // exact file the helper will append to (instead of assuming the current process's computed temp path string).
                    let mut claude_hook_events_path: Option<PathBuf> = None;
                    let hook_integration_disabled = match crate::plugins::ai::settings_file_path(tool, &session_id) {
                        Some(p) => {
                            claude_hook_events_path = crate::claude_hook_events::settings_file_hook_events_path(&p, &session_id);
                            claude_hook_events_path.is_none()
                        }
                        None => false,
                    };
                    let home_opt = home_dir();
                    let kind = if hook_integration_disabled {
                        None
                    } else if let Some(path) = claude_hook_events_path {
                        Some(crate::plugins::ai::ActivityEventsKind::ClaudeHookEventsJsonl(path))
                    } else {
                        crate::plugins::ai::activity_events_kind(tool, session_id, home_opt.as_deref(), ai_session_id.as_deref())
                    };
                    if let Some(kind) = kind {
                        let events_running = Arc::clone(&running);
                        let events_emit = Arc::clone(&activity_emit);
                        let spawn_res = match kind {
                            crate::plugins::ai::ActivityEventsKind::CopilotEventsJsonl(path) => {
                                crate::copilot_events::spawn_watcher(session_id, path, events_emit, events_running)
                            }
                            crate::plugins::ai::ActivityEventsKind::ClaudeHookEventsJsonl(path) => {
                                crate::claude_hook_events::spawn_watcher(session_id, path, events_emit, events_running)
                            }
                        };
                        match spawn_res {
                            Ok(h) => extra_joins.push(h),
                            Err(e) => {
                                tracing::warn!(
                                    session_id = %session_id,
                                    error = %e,
                                    "activity events watcher thread spawn failed",
                                );
                            }
                        }
                    } else if hook_integration_disabled {
                        tracing::debug!(
                            session_id = %session_id,
                            ?tool,
                            "activity events watcher not started (hook integration disabled — settings file missing, unparseable, missing an Arborist hook entry, invalid/missing hook args path, or references a helper command path that doesn't exist in the current process)",
                        );
                    } else {
                        tracing::debug!(
                            session_id = %session_id,
                            ?tool,
                            home_present = home_opt.is_some(),
                            ai_session_id_present = ai_session_id.is_some(),
                            "activity events watcher not started (plugin returned no kind; missing home dir or ai_session_id)",
                        );
                    }
                }
                self.inner.lock().expect("metrics registry lock").insert(
                    session_id,
                    WatcherHandle {
                        running,
                        join: Some(handle),
                        extra_joins,
                    },
                );
                true
            }
            Err(e) => {
                tracing::warn!(session_id = %session_id, error = %e, "metrics watcher thread spawn failed");
                false
            }
        }
    }

    /// Stop the watcher for `session_id` if any. Idempotent.
    ///
    /// Returns immediately after flipping the `running` flag — the worker thread observes it on its next poll. Use [`Self::stop_and_join`] when you
    /// need a guarantee that no further callbacks will fire.
    pub fn stop(&self, session_id: &SessionId) {
        let removed = self.inner.lock().expect("metrics registry lock").remove(session_id);
        if let Some(h) = removed {
            h.running.store(false, Ordering::SeqCst);
        }
    }

    /// Stop the watcher and block until its worker thread has fully exited. Idempotent. Use this when you need a quiescence guarantee — i.e., when
    /// subsequent code mutates state (like `Session.ai_session_id`) that the worker's discovery callback could otherwise overwrite from a final
    /// in-flight poll iteration.
    ///
    /// Worst-case wait is one `POLL_INTERVAL` (~2s with the current configuration) since the worker only re-checks `running` at the top of its loop.
    /// Errors from the underlying thread `join()` are swallowed — there is nothing meaningful the caller can do, and the registry entry has already
    /// been removed.
    pub fn stop_and_join(&self, session_id: &SessionId) {
        let removed = self.inner.lock().expect("metrics registry lock").remove(session_id);
        if let Some(mut h) = removed {
            h.running.store(false, Ordering::SeqCst);
            if let Some(handle) = h.join.take() {
                let _ = handle.join();
            }
            for handle in h.extra_joins.drain(..) {
                let _ = handle.join();
            }
        }
    }

    /// Stop every active watcher. Called on app shutdown / hot-reload.
    pub fn stop_all(&self) {
        let drained: Vec<WatcherHandle> = self.inner.lock().expect("metrics registry lock").drain().map(|(_, h)| h).collect();
        for h in drained {
            h.running.store(false, Ordering::SeqCst);
        }
    }

    /// Stop every active watcher and block until each worker thread has fully exited. Used by the Phase 7 workspace-switch path so the in-flight
    /// discover/turn callbacks (which read the workspace scope) cannot fire after the new workspace has been bound and inadvertently write into the
    /// *new* store with old workspace session ids.
    ///
    /// Worst-case wait is one `POLL_INTERVAL` per active watcher (joins are sequential — fine in practice given the small N of concurrent sessions).
    pub fn stop_all_and_join(&self) {
        let drained: Vec<WatcherHandle> = self.inner.lock().expect("metrics registry lock").drain().map(|(_, h)| h).collect();
        for mut h in drained {
            h.running.store(false, Ordering::SeqCst);
            if let Some(handle) = h.join.take() {
                let _ = handle.join();
            }
            // Also join sibling watchers (e.g. the Copilot `events.jsonl` tailer) that share the same `running` flag — without this they'd outlive
            // the workspace swap and could fire a discover / turn / metrics callback against the new binding using an old-workspace session id.
            for handle in h.extra_joins.drain(..) {
                let _ = handle.join();
            }
        }
    }
}

// --------------------------------------------------------------------------- Per-session worker
// ---------------------------------------------------------------------------

/// A metrics-bearing file located by a [`MetricsParser`], plus any AI-session id derivable from its name/path.
pub struct LocatedFile {
    /// Path to the file the engine should tail.
    pub path: PathBuf,
    /// AI-session id derived from the file's *location* (Claude's JSONL stem, Codex's rollout thread id). `None` when the id is only readable from
    /// file content (Copilot reads its conversation id from parsed spans — see [`MetricsParser::content_ai_session_id`]).
    pub ai_session_id: Option<String>,
}

/// Per-tool wire-format strategy driven by the generic [`run_metrics_watcher`] engine.
///
/// Implementors own everything format-specific: where the tool writes metrics ([`locate`](MetricsParser::locate)), how to parse a line
/// ([`ingest_line`](MetricsParser::ingest_line)), and how to build a snapshot ([`snapshot`](MetricsParser::snapshot)). The engine owns the generic
/// machinery: thread lifecycle, polling cadence, file tailing with truncation handling, discovery backoff, and snapshot dedup. Implementations live
/// in the plugin metrics modules (`plugins::ai::{claude,copilot,codex}::metrics`).
pub trait MetricsParser: Send {
    /// When `true`, the engine re-runs [`locate`](MetricsParser::locate) every poll and rebinds whenever the freshest file changes (Claude: a
    /// `/clear` starts a new transcript). When `false`, the engine binds once and only rediscovers after the bound file disappears.
    fn relocate_each_poll(&self) -> bool;

    /// When `true`, a stat failure on the bound file unbinds it so the next poll rediscovers (Codex: rollout files can be rotated/renamed). When
    /// `false`, a transient stat failure is ignored and accumulated state is preserved (Copilot: the per-session OTel file never moves).
    fn rebind_on_disappear(&self) -> bool;

    /// Locate the file to tail. `spawn_instant` lets discovery ignore stale files predating this session. Returns `None` when nothing matches yet.
    fn locate(&mut self, spawn_instant: SystemTime) -> Option<LocatedFile>;

    /// Reset accumulated parser state (on rebind, truncation, or rotation).
    fn reset(&mut self);

    /// Parse one tailed line, accumulating state and optionally emitting a turn-end via `emit_turn`.
    fn ingest_line(&mut self, line: &[u8], session_id: SessionId, emit_turn: &TurnCb);

    /// AI-session id derivable from already-parsed file *content* (Copilot's conversation id). Defaults to `None` for tools whose id comes from the
    /// file location instead (see [`LocatedFile::ai_session_id`]).
    fn content_ai_session_id(&self) -> Option<&str> {
        None
    }

    /// Build the current metrics snapshot, or `None` when no usable data has accumulated yet.
    fn snapshot(&self, session_id: SessionId) -> Option<SessionMetricsEvent>;
}

/// Generic metrics-watcher engine. Drives one [`MetricsParser`] for the lifetime of `running`: discovers/binds the tool's metrics file, tails it
/// (handling truncation/rotation), feeds each new line to the parser, surfaces AI-session ids, and emits deduplicated snapshots.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_metrics_watcher(
    session_id: SessionId,
    mut parser: Box<dyn MetricsParser>,
    spawn_instant: SystemTime,
    emit: MetricsCb,
    emit_turn: TurnCb,
    discover: AiSessionDiscoveryCb,
    running: Arc<AtomicBool>,
) {
    let relocate_each_poll = parser.relocate_each_poll();
    let rebind_on_disappear = parser.rebind_on_disappear();

    let mut last_emitted: Option<SessionMetricsEvent> = None;
    let mut tracked_path: Option<PathBuf> = None;
    let mut tracked_len: u64 = 0;
    let mut last_announced_content: Option<String> = None;
    let mut discovery_due = Instant::now();
    let mut discovery_interval = DISCOVERY_SCAN_MIN_INTERVAL;

    while running.load(Ordering::SeqCst) {
        // ---- Discovery / (re)binding ----
        let tail_path: Option<PathBuf> = if relocate_each_poll {
            // Relocate every poll: the freshest matching file wins. Switching files resets the cursor and parser state but deliberately NOT
            // `last_emitted` — a Claude `/clear` starts a new transcript yet continues the same Arborist session's metrics stream.
            if let Some(loc) = parser.locate(spawn_instant) {
                if tracked_path.as_ref() != Some(&loc.path) {
                    tracked_path = Some(loc.path.clone());
                    tracked_len = 0;
                    parser.reset();
                    if let Some(id) = loc.ai_session_id {
                        discover(session_id, id);
                    }
                }
                tracked_path.clone()
            } else {
                None
            }
        } else {
            // Bind once; only rediscover while unbound, with backoff between misses. Codex shares one `~/.codex/sessions/` tree across projects, so a
            // cold scan is O(N) over rollout history — the backoff keeps idle sessions cheap.
            if tracked_path.is_none() && Instant::now() >= discovery_due {
                if let Some(loc) = parser.locate(spawn_instant) {
                    tracked_path = Some(loc.path);
                    tracked_len = 0;
                    parser.reset();
                    if let Some(id) = loc.ai_session_id {
                        discover(session_id, id);
                    }
                    discovery_interval = DISCOVERY_SCAN_MIN_INTERVAL;
                } else {
                    discovery_interval = next_discovery_interval(discovery_interval, DISCOVERY_SCAN_MAX_INTERVAL);
                    discovery_due = Instant::now().checked_add(discovery_interval).unwrap_or_else(Instant::now);
                }
            }
            tracked_path.clone()
        };

        // ---- Tail the bound file ----
        if let Some(path) = tail_path {
            match std::fs::metadata(&path) {
                Ok(meta) => {
                    let len = meta.len();
                    // Defensive: handle truncation/rotation. Reset and reread from the start.
                    if len < tracked_len {
                        tracked_len = 0;
                        parser.reset();
                        last_emitted = None;
                        last_announced_content = None;
                    }
                    if len > tracked_len {
                        let emit_turn_ref = &emit_turn;
                        tracked_len = tail_lines(&path, tracked_len, len, |line| {
                            parser.ingest_line(line, session_id, emit_turn_ref);
                        });
                    }
                }
                Err(_) if rebind_on_disappear => {
                    // File disappeared (deletion, rename) — drop tracking so the next tick rediscovers.
                    tracked_path = None;
                    tracked_len = 0;
                    parser.reset();
                    last_emitted = None;
                    last_announced_content = None;
                    discovery_interval = DISCOVERY_SCAN_MIN_INTERVAL;
                    discovery_due = Instant::now();
                }
                // Transient stat failure with no rebind: keep tracking and accumulated state (the file is expected to reappear).
                Err(_) => {}
            }
        }

        // ---- Content-derived AI-session id discovery (e.g. Copilot conversation id) ----
        // The parser may only learn its session id after parsing file content. Fire only on change to keep the per-poll work cheap.
        if let Some(content_id) = parser.content_ai_session_id() {
            if last_announced_content.as_deref() != Some(content_id) {
                let owned = content_id.to_owned();
                discover(session_id, owned.clone());
                last_announced_content = Some(owned);
            }
        }

        // ---- Snapshot + dedup ----
        if let Some(snapshot) = parser.snapshot(session_id) {
            // Compare the data payload only — observed_at advances every poll and would otherwise defeat dedup, causing a redundant emission ~every
            // POLL_INTERVAL even when nothing changed.
            if !last_emitted.as_ref().is_some_and(|prev| prev.same_payload_as(&snapshot)) {
                emit(snapshot.clone());
                last_emitted = Some(snapshot);
            }
        }

        thread::sleep(POLL_INTERVAL);
    }
}

/// Doubling backoff for file-discovery scans, capped at `max`.
fn next_discovery_interval(current: Duration, max: Duration) -> Duration {
    current.saturating_mul(2).min(max)
}

// --------------------------------------------------------------------------- Path helpers
// ---------------------------------------------------------------------------

/// Encode a cwd to the `~/.claude/projects/<dir>` form used by Claude Code: every `\\`, `/`, `:`, and `.` in the absolute path becomes `-`. The dot
/// replacement is what produces the `--` between segments like `\.worktrees\` (a real path written by `git worktree`); without it the encoded
/// directory name will not match Claude's on disk.
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

pub(crate) fn home_dir() -> Option<PathBuf> {
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

/// Defensive cap on a single watcher read. A runaway exporter (e.g. a Copilot bug or a Claude session that grew enormous between polls) must not be
/// able to make us allocate gigabytes in one shot. If the file is larger than this, we read this much and let the caller's last-newline logic
/// re-enter on the next poll to consume the rest.
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

/// Crate-internal re-export of [`tail_lines`] so the Copilot events.jsonl tailer in [`crate::copilot_events`] can reuse the same chunked-read +
/// oversized-line-skip behavior without duplicating it. Kept as a thin alias rather than `pub fn tail_lines` to keep the surface area honest — this
/// is a sibling-module helper, not a public API.
#[doc(hidden)]
pub(crate) fn tail_lines_pub<F: FnMut(&[u8])>(path: &Path, cursor: u64, end: u64, consume: F) -> u64 {
    tail_lines(path, cursor, end, consume)
}

/// Tail `path` from `cursor` up to `end`, invoking `consume` for each complete `\n`-terminated line found. Returns the new cursor position (always
/// `>= cursor`).
///
/// Three pitfalls handled:
/// 1. **Half-written trailing line.** The cursor only advances past the last
///    complete `\n`; partial trailing bytes are re-read on the next call.
///    Without this the line parser silently drops a partial JSON object the
///    writer is still finishing.
/// 2. **Read cap.** `read_range` is capped at [`MAX_READ_CHUNK`] so a runaway
///    exporter cannot make us allocate gigabytes in one shot. The remaining
///    bytes are picked up on subsequent polls.
/// 3. **Oversized line (> [`MAX_READ_CHUNK`] without a `\n` in the cap).** If
///    the capped chunk has no newline AND the file extends past what we read,
///    the line itself is bigger than the cap. Without special handling,
///    `rposition` would always be `None` and the watcher would re-read the same
///    chunk forever (wasted I/O, metrics never advance). We scan forward from
///    the end of the chunk *without* buffering to find the next `\n`, log a
///    warning, and skip the whole oversized line. If no `\n` is found anywhere
///    up to `end`, the writer is still in flight, so we leave the cursor where
///    it is and let the next poll re-check.
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
    // No newline in the chunk. If there's more file beyond what we read, the line is oversized — skip it. Otherwise the writer is still in flight and
    // we should retry next poll.
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

/// Scan `path` from `start` (inclusive) up to `end` (exclusive) without buffering the whole range, looking for the next `\n`. Returns its absolute
/// byte offset, or `None` if no newline exists in the range.
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

// --------------------------------------------------------------------------- Time helper
// ---------------------------------------------------------------------------

pub(crate) fn now_unix_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Compute "percent of context window used", clamped to 100. Returns `None` when either value is unknown or the limit is zero. Uses `u128` so absurd
/// token counts can't overflow the intermediate multiply. Shared by every tool's snapshot builder — this is generic arithmetic, not wire-format
/// knowledge, so it belongs in the engine rather than being re-derived per plugin.
pub(crate) fn context_used_pct(used: Option<u64>, limit: Option<u64>) -> Option<u8> {
    match (used, limit) {
        (Some(u), Some(lim)) if lim > 0 => Some(((u as u128 * 100 / lim as u128).min(100)) as u8),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Real-world git-worktree path: the leading `.` in `.worktrees` is what produces the `--` Claude writes on disk.
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
            Arc::new(|_, _| {}),
            Arc::new(|_, _| {}),
            None,
        );
        assert!(started, "Copilot watcher must start");
        assert!(reg.inner.lock().unwrap().contains_key(&id), "registry should track the Copilot watcher",);
        // Stop it so the worker thread exits before the test ends.
        reg.stop(&id);
        assert!(!reg.inner.lock().unwrap().contains_key(&id), "registry should drop the entry on stop",);
    }

    #[test]
    fn registry_stop_is_idempotent_when_not_running() {
        let reg = MetricsRegistry::new();
        // Should not panic on unknown session id.
        reg.stop(&SessionId::new());
    }

    // ----- read_range cap (defensive against runaway exporter) -------------

    #[test]
    fn read_range_caps_at_max_chunk_size() {
        // A pathological 50MB file. The cap should clamp the buffer to MAX_READ_CHUNK and let the watcher's last-newline logic drain the rest on
        // subsequent polls.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("huge.bin");
        let total: u64 = 50 * 1024 * 1024;
        // Write a sparse-ish payload: chunks of 'a' separated by newlines every 1MB so the watcher would be able to make forward progress.
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

    /// Regression for the infinite-loop hazard noted in PR review: if a single line exceeds `MAX_READ_CHUNK`, both watchers used to be stuck forever
    /// — `rposition('\n')` would always be `None` on the capped chunk and the cursor would never move. `tail_lines` must detect "no newline + more
    /// file beyond the chunk", scan forward without buffering, and skip the oversized line.
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
        // Loop the watcher's poll body until the cursor reaches EOF or we give up. A working implementation drains in 1–2 iterations; a broken one
        // (the bug under regression) loops forever.
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

    /// `tail_lines` must NOT skip a "long" line that hasn't been terminated yet — that's a partial trailing line, not an oversized one. Cursor stays
    /// put; we'll re-read after the writer commits a `\n`. The oversized-line skip only triggers when there's already more data on the far side of
    /// the cap.
    #[test]
    fn tail_lines_does_not_skip_in_flight_partial_line() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("inflight.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // Oversized partial line: > MAX_READ_CHUNK, NO trailing \n, NO extra bytes after. This is "writer still in flight on a huge line", which is
        // rare but distinct from "committed oversized line followed by more data".
        let partial = vec![b'x'; (MAX_READ_CHUNK + 4096) as usize];
        f.write_all(&partial).unwrap();
        drop(f);
        let len = std::fs::metadata(&path).unwrap().len();

        let mut consumed: usize = 0;
        let new_cursor = tail_lines(&path, 0, len, |_line| consumed += 1);
        assert_eq!(consumed, 0, "no complete line yet");
        assert_eq!(new_cursor, 0, "cursor must NOT advance on in-flight EOF");
    }

    /// Sanity: `tail_lines` must also drain a normal multi-line chunk in one call and leave a trailing partial in place.
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

    // ----- dedup regression -------------------------------------------------

    /// Bug regression: `observed_at` must not be compared as part of the dedup check. Two snapshots with the same data but different timestamps must
    /// compare as equal payload, otherwise both watchers would emit a redundant event every POLL_INTERVAL forever.
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
        assert!(a.same_payload_as(&b), "same data must compare equal regardless of observed_at");

        // Sanity: any change in actual data flips the comparison.
        let mut c = a.clone();
        c.input_tokens = Some(39_498);
        assert!(!a.same_payload_as(&c));
    }

    /// Regression for PR #32 review finding: `stop_all_and_join` must join `extra_joins` (the Copilot events.jsonl tailer) in addition to the primary
    /// watcher. If it doesn't, the sibling thread can outlive the workspace swap and emit stale activity into the new binding.
    #[test]
    fn stop_all_and_join_waits_for_extra_joins() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let registry = MetricsRegistry::new();
        let session_id = SessionId::new();

        // Sentinel flipped by the sibling thread on its way out. After `stop_all_and_join` returns, this MUST be true — otherwise the sibling
        // outlived the join.
        let sibling_exited = Arc::new(AtomicBool::new(false));
        let sibling_exited_for_thread = Arc::clone(&sibling_exited);

        let running = Arc::new(AtomicBool::new(true));
        let running_for_primary = Arc::clone(&running);
        let running_for_extra = Arc::clone(&running);

        let primary = thread::spawn(move || {
            while running_for_primary.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(2));
            }
        });
        let extra = thread::spawn(move || {
            while running_for_extra.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(2));
            }
            // Sleep a touch longer than the primary so that a buggy `stop_all_and_join` that only joins the primary would observe `false` here when
            // it returns.
            thread::sleep(Duration::from_millis(30));
            sibling_exited_for_thread.store(true, Ordering::SeqCst);
        });

        registry.inner.lock().expect("lock").insert(
            session_id,
            WatcherHandle {
                running,
                join: Some(primary),
                extra_joins: vec![extra],
            },
        );

        registry.stop_all_and_join();

        assert!(
            sibling_exited.load(Ordering::SeqCst),
            "stop_all_and_join must join extra_joins, not just the primary thread"
        );
    }

    #[test]
    fn next_discovery_interval_doubles_until_cap() {
        let next = next_discovery_interval(DISCOVERY_SCAN_MIN_INTERVAL, DISCOVERY_SCAN_MAX_INTERVAL);
        assert_eq!(next, DISCOVERY_SCAN_MIN_INTERVAL.saturating_mul(2));

        let capped = next_discovery_interval(DISCOVERY_SCAN_MAX_INTERVAL, DISCOVERY_SCAN_MAX_INTERVAL);
        assert_eq!(capped, DISCOVERY_SCAN_MAX_INTERVAL);
    }

    #[test]
    fn context_used_pct_handles_unknowns_clamp_and_overflow() {
        assert_eq!(context_used_pct(None, Some(100)), None);
        assert_eq!(context_used_pct(Some(50), None), None);
        assert_eq!(context_used_pct(Some(50), Some(0)), None);
        assert_eq!(context_used_pct(Some(50), Some(200)), Some(25));
        // Used exceeds the window (e.g. post-overflow Codex cumulative) — must clamp, not wrap.
        assert_eq!(context_used_pct(Some(500), Some(200)), Some(100));
        // u128 intermediate must not overflow on absurd inputs.
        assert_eq!(
            context_used_pct(Some(5_000_000_000_000_000_000), Some(10_000_000_000_000_000_000)),
            Some(50)
        );
    }
}
