//! Append-only, tamper-evident JSONL audit log for MCP activity.
//!
//! Two streams under `<workspace_state_dir>/`:
//! * `mcp-audit.jsonl` — read-only tool calls (`list_worktrees`, `workspace_status`), 10 MB / 5
//!   generations.
//! * `mcp-audit-destructive.jsonl` — write tool calls (`create_worktree`,
//!   `cleanup_merged_worktrees`, `merge_main_into_worktrees`), 50 MB / 10 generations.
//!
//! Each row is the canonical `arborist_types::mcp::McpAuditRecord` (serialized as one line of
//! camelCase JSON). The audit log stamps `seq` and `prevHashHex` on every row from a
//! monotonically-increasing counter and a SHA-256 chain over the canonical JSON of the prior
//! row; callers pass an `AuditEntryInput` without those fields. `verify_chain` recomputes the
//! chain on load and flags the first row whose prevHashHex doesn't match. This makes silent
//! tampering (e.g., editing a row in place) detectable on next startup.
//!
//! Files are opened in append-only mode with `O_APPEND`-equivalent semantics; rotation walks
//! generations from oldest to newest and renames in place.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::mcp::types::{McpAuditDecision, McpAuditFilter, McpAuditPage, McpAuditRecord};

const READ_LOG_NAME: &str = "mcp-audit.jsonl";
const READ_MAX_BYTES: u64 = 10 * 1024 * 1024;
const READ_GENERATIONS: usize = 5;
const DESTRUCTIVE_LOG_NAME: &str = "mcp-audit-destructive.jsonl";
const DESTRUCTIVE_MAX_BYTES: u64 = 50 * 1024 * 1024;
const DESTRUCTIVE_GENERATIONS: usize = 10;

/// Caller-supplied fields for an audit row. `seq` and `prevHashHex` are assigned by the audit
/// log itself so callers can never accidentally desynchronize the chain (or maliciously
/// reuse a low seq to overwrite history).
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEntryInput {
    pub ts: String,
    pub session_id: String,
    pub session_label: String,
    pub tool: String,
    pub decision: McpAuditDecision,
    pub args_summary: String,
    pub result: Value,
    pub duration_ms: u64,
    pub request_id: String,
    pub confirmation_token_sha256: Option<String>,
    pub audit_id: String,
}

impl AuditEntryInput {
    fn into_record(self, seq: u64, prev_hash_hex: String) -> McpAuditRecord {
        McpAuditRecord {
            seq,
            prev_hash_hex,
            ts: self.ts,
            session_id: self.session_id,
            session_label: self.session_label,
            tool: self.tool,
            decision: self.decision,
            args_summary: self.args_summary,
            result: self.result,
            duration_ms: self.duration_ms,
            request_id: self.request_id,
            confirmation_token_sha256: self.confirmation_token_sha256,
            audit_id: self.audit_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TamperedAt {
    pub line: usize,
    pub reason: String,
}

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

pub struct AuditLog {
    inner: Mutex<AuditLogInner>,
    startup_tampered: Vec<PathBuf>,
}

struct AuditLogInner {
    read: AuditStream,
    destructive: AuditStream,
}

struct AuditStream {
    path: PathBuf,
    max_bytes: u64,
    generations: usize,
    file: Option<File>,
    next_seq: u64,
    prev_hash: [u8; 32],
}

impl AuditLog {
    pub fn new(workspace_state_dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&workspace_state_dir)?;
        let read_path = workspace_state_dir.join(READ_LOG_NAME);
        let destructive_path = workspace_state_dir.join(DESTRUCTIVE_LOG_NAME);

        let (read, read_tampered) = AuditStream::open(read_path, READ_MAX_BYTES, READ_GENERATIONS)?;
        let (destructive, destructive_tampered) = AuditStream::open(destructive_path, DESTRUCTIVE_MAX_BYTES, DESTRUCTIVE_GENERATIONS)?;

        let mut startup_tampered = Vec::new();
        if read_tampered {
            startup_tampered.push(read.path.clone());
        }
        if destructive_tampered {
            startup_tampered.push(destructive.path.clone());
        }

        Ok(Self {
            inner: Mutex::new(AuditLogInner { read, destructive }),
            startup_tampered,
        })
    }

    #[must_use]
    pub fn tampered_logs(&self) -> Vec<PathBuf> {
        self.startup_tampered.clone()
    }

    pub fn append_read(&self, input: AuditEntryInput) -> Result<u64, AuditError> {
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.read.append(input)
    }

    pub fn append_destructive(&self, input: AuditEntryInput) -> Result<u64, AuditError> {
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.destructive.append(input)
    }

    pub fn read_page(&self, filter: &McpAuditFilter) -> io::Result<McpAuditPage> {
        let (read_path, destructive_path) = {
            let guard = match self.inner.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            (guard.read.path.clone(), guard.destructive.path.clone())
        };
        read_page(&read_path, &destructive_path, filter)
    }
}

impl AuditStream {
    fn open(path: PathBuf, max_bytes: u64, generations: usize) -> io::Result<(Self, bool)> {
        let tampered = verify_chain(&path).is_err();
        let state = scan_state_loose(&path)?;
        let file = Some(open_append(&path)?);
        Ok((
            Self {
                path,
                max_bytes,
                generations,
                file,
                next_seq: state.next_seq,
                prev_hash: state.prev_hash,
            },
            tampered,
        ))
    }

    fn append(&mut self, input: AuditEntryInput) -> Result<u64, AuditError> {
        self.rotate_if_needed()?;

        let seq = self.next_seq;
        let prev_hash_hex = hex::encode(self.prev_hash);
        let record = input.into_record(seq, prev_hash_hex);
        // Serialize through serde so the on-disk shape matches the canonical wire shape
        // (camelCase, all fields). Then re-serialize through `canonical_json_bytes` to
        // produce a deterministic byte sequence for the hash chain.
        let row = serde_json::to_value(&record)?;
        let payload = canonical_json_bytes(&row)?;
        let current_hash = hash_row(self.prev_hash, &row)?;

        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other(format!("audit log not open: {}", self.path.display())))?;
        file.write_all(&payload)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;

        self.next_seq = self.next_seq.saturating_add(1);
        self.prev_hash = current_hash;
        Ok(seq)
    }

    fn rotate_if_needed(&mut self) -> io::Result<()> {
        let current_size = self.path.metadata().map(|meta| meta.len()).unwrap_or(0);
        if current_size <= self.max_bytes {
            return Ok(());
        }

        if let Some(file) = self.file.take() {
            file.sync_all()?;
        }

        rotate_files(&self.path, self.generations)?;
        self.prev_hash = [0; 32];
        self.file = Some(open_append(&self.path)?);
        Ok(())
    }
}

/// Replay the chain from `path` to confirm no row has been tampered. `Ok(())` on success or
/// when the file is absent; returns `TamperedAt` pointing at the first offending line on
/// detection. We return `Result` rather than annotating `#[must_use]` because Rust already
/// warns on unused `Result`s (the prior `#[must_use]` was redundant and triggered
/// clippy::double_must_use).
pub fn verify_chain(path: &Path) -> Result<(), TamperedAt> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(TamperedAt {
                line: 0,
                reason: format!("failed to open audit log: {err}"),
            });
        }
    };

    let mut expected_prev = [0_u8; 32];
    let mut last_seq = 0_u64;
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line_number = index + 1;
        let line = line.map_err(|err| TamperedAt {
            line: line_number,
            reason: format!("failed to read line: {err}"),
        })?;
        if line.trim().is_empty() {
            continue;
        }

        let row: Value = serde_json::from_str(&line).map_err(|err| TamperedAt {
            line: line_number,
            reason: format!("invalid JSON row: {err}"),
        })?;
        let seq = extract_seq(&row).ok_or_else(|| TamperedAt {
            line: line_number,
            reason: "missing seq".to_owned(),
        })?;
        if seq <= last_seq {
            return Err(TamperedAt {
                line: line_number,
                reason: format!("sequence regressed from {last_seq} to {seq}"),
            });
        }

        let prev_hash_hex = extract_prev_hash_hex(&row).ok_or_else(|| TamperedAt {
            line: line_number,
            reason: "missing prevHashHex".to_owned(),
        })?;
        let prev_hash = decode_hash_hex(prev_hash_hex).map_err(|reason| TamperedAt { line: line_number, reason })?;
        if prev_hash != expected_prev {
            return Err(TamperedAt {
                line: line_number,
                reason: format!("prevHashHex mismatch: expected {}, found {}", hex::encode(expected_prev), prev_hash_hex),
            });
        }

        expected_prev = hash_row(prev_hash, &row).map_err(|err| TamperedAt {
            line: line_number,
            reason: err.to_string(),
        })?;
        last_seq = seq;
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuditStreamKind {
    Read,
    Destructive,
}

#[derive(Debug, Clone)]
struct PageEntry {
    stream: AuditStreamKind,
    record: McpAuditRecord,
    ts: Option<OffsetDateTime>,
}

pub fn read_page(read_path: &Path, destructive_path: &Path, filter: &McpAuditFilter) -> io::Result<McpAuditPage> {
    let since = parse_filter_time(filter.since.as_deref())?;
    let until = parse_filter_time(filter.until.as_deref())?;
    let limit = filter.limit.clamp(1, 500) as usize;

    let mut entries = Vec::new();
    entries.extend(read_entries(read_path, AuditStreamKind::Read, filter, since, until)?);
    entries.extend(read_entries(destructive_path, AuditStreamKind::Destructive, filter, since, until)?);

    entries.sort_by(|left, right| {
        right
            .ts
            .cmp(&left.ts)
            .then_with(|| right.record.seq.cmp(&left.record.seq))
            .then_with(|| cursor_for(right.stream, right.record.seq).cmp(&cursor_for(left.stream, left.record.seq)))
    });

    let start_index = match filter.cursor.as_deref() {
        Some(cursor) => entries
            .iter()
            .position(|entry| cursor_for(entry.stream, entry.record.seq) == cursor)
            .unwrap_or(entries.len()),
        None => 0,
    };
    let page_records: Vec<McpAuditRecord> = entries.iter().skip(start_index).take(limit).map(|entry| entry.record.clone()).collect();
    let next_cursor = entries
        .get(start_index.saturating_add(limit))
        .map(|entry| cursor_for(entry.stream, entry.record.seq));

    Ok(McpAuditPage {
        records: page_records,
        next_cursor,
    })
}

fn read_entries(
    path: &Path,
    stream: AuditStreamKind,
    filter: &McpAuditFilter,
    since: Option<OffsetDateTime>,
    until: Option<OffsetDateTime>,
) -> io::Result<Vec<PageEntry>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };

    let mut entries = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<McpAuditRecord>(&line) else {
            continue;
        };
        if !record_matches_filter(&record, filter, since, until) {
            continue;
        }
        let ts = OffsetDateTime::parse(&record.ts, &Rfc3339).ok();
        entries.push(PageEntry { stream, record, ts });
    }
    Ok(entries)
}

fn record_matches_filter(record: &McpAuditRecord, filter: &McpAuditFilter, since: Option<OffsetDateTime>, until: Option<OffsetDateTime>) -> bool {
    if filter.session_id.as_deref().is_some_and(|session_id| record.session_id != session_id) {
        return false;
    }
    if filter.tool.as_deref().is_some_and(|tool| record.tool != tool) {
        return false;
    }
    if filter.decision.is_some_and(|decision| record.decision != decision) {
        return false;
    }
    let ts = match OffsetDateTime::parse(&record.ts, &Rfc3339) {
        Ok(ts) => ts,
        Err(_) => return false,
    };
    if since.is_some_and(|since| ts < since) {
        return false;
    }
    if until.is_some_and(|until| ts > until) {
        return false;
    }
    true
}

fn parse_filter_time(value: Option<&str>) -> io::Result<Option<OffsetDateTime>> {
    let Some(value) = value else {
        return Ok(None);
    };
    OffsetDateTime::parse(value, &Rfc3339)
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid RFC3339 timestamp '{value}': {err}")))
}

fn cursor_for(stream: AuditStreamKind, seq: u64) -> String {
    // Two append-only audit logs each maintain their own `seq`, so pagination has to encode the
    // stream alongside the seq to keep cursors stable once the viewer merges them.
    let prefix = match stream {
        AuditStreamKind::Read => "read",
        AuditStreamKind::Destructive => "destructive",
    };
    format!("{prefix}:{seq}")
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().append(true).create(true).open(path)
}

fn hash_row(prev_hash: [u8; 32], row: &Value) -> Result<[u8; 32], AuditError> {
    let payload = canonical_json_bytes(row)?;
    let mut hasher = Sha256::new();
    hasher.update(prev_hash);
    hasher.update(payload);
    Ok(hasher.finalize().into())
}

fn canonical_json_bytes(value: &Value) -> Result<Vec<u8>, serde_json::Error> {
    let mut output = Vec::new();
    write_canonical_value(value, &mut output)?;
    Ok(output)
}

fn write_canonical_value(value: &Value, output: &mut Vec<u8>) -> Result<(), serde_json::Error> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(flag) => output.extend_from_slice(if *flag { b"true" } else { b"false" }),
        Value::Number(number) => serde_json::to_writer(output, number)?,
        Value::String(text) => serde_json::to_writer(output, text)?,
        Value::Array(items) => {
            output.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical_value(item, output)?;
            }
            output.push(b']');
        }
        Value::Object(map) => {
            output.push(b'{');
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key)?;
                output.push(b':');
                write_canonical_value(value, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

fn extract_seq(row: &Value) -> Option<u64> {
    row.get("seq")?.as_u64()
}

fn extract_prev_hash_hex(row: &Value) -> Option<&str> {
    row.get("prevHashHex")?.as_str()
}

fn decode_hash_hex(value: &str) -> Result<[u8; 32], String> {
    let mut bytes = [0_u8; 32];
    hex::decode_to_slice(value, &mut bytes).map_err(|err| format!("invalid prevHashHex: {err}"))?;
    Ok(bytes)
}

#[derive(Debug, Clone, Copy)]
struct ScanState {
    next_seq: u64,
    prev_hash: [u8; 32],
}

fn scan_state_loose(path: &Path) -> io::Result<ScanState> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(ScanState {
                next_seq: 1,
                prev_hash: [0; 32],
            });
        }
        Err(err) => return Err(err),
    };

    let mut state = ScanState {
        next_seq: 1,
        prev_hash: [0; 32],
    };
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(seq) = extract_seq(&row) else {
            continue;
        };
        let Some(prev_hash_hex) = extract_prev_hash_hex(&row) else {
            continue;
        };
        let Ok(prev_hash) = decode_hash_hex(prev_hash_hex) else {
            continue;
        };
        let Ok(current_hash) = hash_row(prev_hash, &row) else {
            continue;
        };
        state.next_seq = seq.saturating_add(1);
        state.prev_hash = current_hash;
    }

    Ok(state)
}

fn rotate_files(base: &Path, generations: usize) -> io::Result<()> {
    for index in (1..=generations).rev() {
        let target = rotated_path(base, index);
        if index == generations && target.exists() {
            fs::remove_file(&target)?;
        }

        let source = if index == 1 { base.to_path_buf() } else { rotated_path(base, index - 1) };
        if source.exists() {
            fs::rename(source, target)?;
        }
    }
    Ok(())
}

fn rotated_path(base: &Path, index: usize) -> PathBuf {
    base.with_extension(format!("jsonl.{index}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample_input(label: &str) -> AuditEntryInput {
        AuditEntryInput {
            ts: format!("2025-01-01T00:00:0{label}Z"),
            session_id: "session-1".to_owned(),
            session_label: "feature-x".to_owned(),
            tool: "list_worktrees".to_owned(),
            decision: McpAuditDecision::AutoApproved,
            args_summary: format!("args {label}"),
            result: json!({"outcome": format!("outcome {label}")}),
            duration_ms: 12,
            request_id: format!("req-{label}"),
            confirmation_token_sha256: None,
            audit_id: format!("audit-{label}"),
        }
    }

    #[test]
    fn append_and_verify_chain() {
        let temp_dir = TempDir::new().expect("tempdir");
        let log = AuditLog::new(temp_dir.path().to_path_buf()).expect("log should open");

        let seq = log.append_read(sample_input("1")).expect("append should succeed");

        assert_eq!(seq, 1);
        assert!(verify_chain(&temp_dir.path().join(READ_LOG_NAME)).is_ok());
    }

    #[test]
    fn tampered_row_is_detected() {
        let temp_dir = TempDir::new().expect("tempdir");
        let log = AuditLog::new(temp_dir.path().to_path_buf()).expect("log should open");
        log.append_read(sample_input("1")).expect("first append should succeed");
        log.append_read(sample_input("2")).expect("second append should succeed");

        let path = temp_dir.path().join(READ_LOG_NAME);
        let mutated = fs::read_to_string(&path).expect("read log").replace("outcome 1", "tampered outcome");
        fs::write(&path, mutated).expect("rewrite tampered log");

        let err = verify_chain(&path).expect_err("hash chain should detect tampering");
        assert_eq!(err.line, 2);
    }

    #[test]
    fn rotation_is_triggered_before_append_when_cap_is_exceeded() {
        let temp_dir = TempDir::new().expect("tempdir");
        let path = temp_dir.path().join("audit.jsonl");
        let (mut stream, _) = AuditStream::open(path.clone(), 1, 2).expect("stream should open");

        stream.append(sample_input("1")).expect("first append should succeed");
        stream.append(sample_input("2")).expect("second append should rotate then append");

        assert!(rotated_path(&path, 1).exists());
        let current = fs::read_to_string(&path).expect("read active log");
        assert!(current.contains("outcome 2"));
    }

    #[test]
    fn seq_is_monotonic_per_log_stream() {
        let temp_dir = TempDir::new().expect("tempdir");
        let log = AuditLog::new(temp_dir.path().to_path_buf()).expect("log should open");

        let read_1 = log.append_read(sample_input("1")).expect("append should succeed");
        let read_2 = log.append_read(sample_input("2")).expect("append should succeed");
        let destructive_1 = log.append_destructive(sample_input("3")).expect("append should succeed");
        let destructive_2 = log.append_destructive(sample_input("4")).expect("append should succeed");

        assert_eq!((read_1, read_2), (1, 2));
        assert_eq!((destructive_1, destructive_2), (1, 2));
    }

    #[test]
    fn read_page_returns_reverse_chronological_records_across_both_streams() {
        let temp_dir = TempDir::new().expect("tempdir");
        let log = AuditLog::new(temp_dir.path().to_path_buf()).expect("log should open");
        log.append_read(sample_input("1")).expect("append should succeed");
        log.append_read(sample_input("2")).expect("append should succeed");
        log.append_destructive(sample_input("3")).expect("append should succeed");

        let page = log
            .read_page(&McpAuditFilter {
                limit: 10,
                ..Default::default()
            })
            .expect("page should load");

        let ts_values: Vec<_> = page.records.iter().map(|record| record.ts.as_str()).collect();
        assert_eq!(ts_values, vec!["2025-01-01T00:00:03Z", "2025-01-01T00:00:02Z", "2025-01-01T00:00:01Z"]);
        assert_eq!(page.next_cursor, None);
    }

    #[test]
    fn read_page_filters_and_paginates_with_composite_cursor() {
        let temp_dir = TempDir::new().expect("tempdir");
        let log = AuditLog::new(temp_dir.path().to_path_buf()).expect("log should open");
        let mut read_one = sample_input("1");
        read_one.tool = "workspace_status".to_owned();
        read_one.decision = McpAuditDecision::Approved;
        let mut read_two = sample_input("2");
        read_two.tool = "workspace_status".to_owned();
        read_two.decision = McpAuditDecision::Approved;
        let mut destructive = sample_input("3");
        destructive.tool = "create_worktree".to_owned();
        log.append_read(read_one).expect("append should succeed");
        log.append_read(read_two).expect("append should succeed");
        log.append_destructive(destructive).expect("append should succeed");

        let first = log
            .read_page(&McpAuditFilter {
                tool: Some("workspace_status".to_owned()),
                decision: Some(McpAuditDecision::Approved),
                limit: 1,
                ..Default::default()
            })
            .expect("first page should load");
        assert_eq!(first.records.len(), 1);
        assert_eq!(first.records[0].seq, 2);
        assert_eq!(first.next_cursor.as_deref(), Some("read:1"));

        let second = log
            .read_page(&McpAuditFilter {
                tool: Some("workspace_status".to_owned()),
                decision: Some(McpAuditDecision::Approved),
                limit: 1,
                cursor: first.next_cursor,
                ..Default::default()
            })
            .expect("second page should load");
        assert_eq!(second.records.len(), 1);
        assert_eq!(second.records[0].seq, 1);
        assert_eq!(second.next_cursor, None);
    }
}
