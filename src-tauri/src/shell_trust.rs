//! Trust records for repo-provided executable settings.
//!
//! Repo-owned `.arborist/settings.json` can contribute shell snippets. Those
//! snippets must be explicitly trusted before execution, and persisted sessions
//! created from them are revalidated before restart/restore.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::compose;
use crate::types::{
    AppConfig, AppError, CommandProvenance, RepoCommandTrustRecord, Session, ShellCommandKind, ShellCommandPreview, ShellCommandPreviewItem,
    ShellCommandSource,
};

const TRUST_FINGERPRINT_VERSION: &str = "arborist.repo-shell-command.v1";
const SESSION_TEMP_PLACEHOLDER: &str = "<session-temp>";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoCommandCandidate {
    pub kind: ShellCommandKind,
    pub command: String,
    pub scope: Option<String>,
    pub workspace_root: PathBuf,
    pub source_path: PathBuf,
    pub target_worktree_path: PathBuf,
}

#[must_use]
pub fn normalize_session_command(session: &Session) -> String {
    normalize_command_for_session(session.id, &session.composed_command)
}

#[must_use]
pub fn normalize_command_for_session(session_id: crate::types::SessionId, command: &str) -> String {
    let raw_temp_dir = compose::session_temp_dir(&session_id).to_string_lossy().into_owned();
    let slash_temp_dir = raw_temp_dir.replace('\\', "/");
    command
        .replace(&raw_temp_dir, SESSION_TEMP_PLACEHOLDER)
        .replace(&slash_temp_dir, SESSION_TEMP_PLACEHOLDER)
}

#[must_use]
pub fn fingerprint(candidate: &RepoCommandCandidate) -> String {
    let shell = compose::platform_shell();
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, "version", TRUST_FINGERPRINT_VERSION);
    hash_field(&mut hasher, "os", std::env::consts::OS);
    hash_field(&mut hasher, "shell_program", &shell.program);
    hash_field(&mut hasher, "shell_flag", shell.flag);
    hash_field(&mut hasher, "workspace_root", &path_key(&candidate.workspace_root));
    hash_field(&mut hasher, "source_path", &path_key(&candidate.source_path));
    hash_field(&mut hasher, "kind", kind_key(candidate.kind));
    hash_field(&mut hasher, "scope", candidate.scope.as_deref().unwrap_or(""));
    hash_field(&mut hasher, "command", &candidate.command);
    // `sha2 = "0.11"` returns a `hybrid_array::Array<u8, _>` from `finalize()`, which (unlike
    // generic-array in 0.10) does NOT implement `LowerHex` — so `format!("{:x}", …)` no longer
    // compiles. Hand-roll the lowercase hex encoding the same way `store_layout::path_hash`
    // does to avoid pulling in the `hex` crate as a direct dependency.
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in &digest {
        // Infallible: writing to a String never errors.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn hash_field(hasher: &mut Sha256, name: &str, value: &str) {
    hasher.update(name.as_bytes());
    hasher.update(b"\0");
    hasher.update(value.len().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(value.as_bytes());
    hasher.update(b"\0");
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn kind_key(kind: ShellCommandKind) -> &'static str {
    match kind {
        ShellCommandKind::AiLaunch => "aiLaunch",
        ShellCommandKind::WorktreePrep => "worktreePrep",
    }
}

fn kind_label(kind: ShellCommandKind) -> &'static str {
    match kind {
        ShellCommandKind::AiLaunch => "AI launch",
        ShellCommandKind::WorktreePrep => "worktree prep",
    }
}

#[must_use]
pub fn trust_record(candidate: &RepoCommandCandidate, trusted_at: i64) -> RepoCommandTrustRecord {
    RepoCommandTrustRecord {
        fingerprint: fingerprint(candidate),
        workspace_root: candidate.workspace_root.clone(),
        source_path: candidate.source_path.clone(),
        kind: candidate.kind,
        scope: candidate.scope.clone(),
        command: candidate.command.clone(),
        trusted_at,
    }
}

#[must_use]
pub fn provenance(candidate: &RepoCommandCandidate) -> CommandProvenance {
    CommandProvenance {
        kind: candidate.kind,
        source: ShellCommandSource::RepoSettings,
        command: candidate.command.clone(),
        scope: candidate.scope.clone(),
        fingerprint: Some(fingerprint(candidate)),
        workspace_root: Some(candidate.workspace_root.clone()),
        source_path: Some(candidate.source_path.clone()),
    }
}

#[must_use]
pub fn is_trusted(cfg: &AppConfig, candidate: &RepoCommandCandidate) -> bool {
    let fp = fingerprint(candidate);
    cfg.repo_command_trust
        .records
        .get(&fp)
        .is_some_and(|record| record_matches_candidate(record, candidate, &fp))
}

fn untrusted_candidates<'a>(cfg: &AppConfig, candidates: &'a [RepoCommandCandidate]) -> Vec<(String, &'a RepoCommandCandidate)> {
    let mut seen = HashSet::new();
    candidates
        .iter()
        .filter_map(|candidate| {
            if is_trusted(cfg, candidate) {
                return None;
            }
            let fingerprint = fingerprint(candidate);
            if seen.insert(fingerprint.clone()) {
                Some((fingerprint, candidate))
            } else {
                None
            }
        })
        .collect()
}

fn record_matches_candidate(record: &RepoCommandTrustRecord, candidate: &RepoCommandCandidate, fingerprint: &str) -> bool {
    record.fingerprint == fingerprint
        && record.workspace_root == candidate.workspace_root
        && record.source_path == candidate.source_path
        && record.kind == candidate.kind
        && record.scope == candidate.scope
        && record.command == candidate.command
}

pub fn ensure_trusted(cfg: &AppConfig, candidates: &[RepoCommandCandidate]) -> Result<(), AppError> {
    if let Some((_, candidate)) = untrusted_candidates(cfg, candidates).first() {
        return Err(AppError::new(
            "TrustRequired",
            format!(
                "Repository-provided {} command from {} must be trusted before it can run.",
                kind_label(candidate.kind),
                candidate.source_path.display(),
            ),
        ));
    }
    Ok(())
}

pub fn allow_once(approvals: &Mutex<HashMap<String, usize>>, records: &[RepoCommandTrustRecord]) -> Result<(), AppError> {
    let mut approvals = approvals
        .lock()
        .map_err(|_| AppError::new("Internal", "repo command one-shot trust mutex poisoned"))?;
    for record in records {
        *approvals.entry(record.fingerprint.clone()).or_insert(0) += 1;
    }
    Ok(())
}

pub fn ensure_trusted_or_consume_once(
    cfg: &AppConfig,
    candidates: &[RepoCommandCandidate],
    approvals: &Mutex<HashMap<String, usize>>,
) -> Result<(), AppError> {
    let untrusted = untrusted_candidates(cfg, candidates);
    if untrusted.is_empty() {
        return Ok(());
    }

    let mut approvals = approvals
        .lock()
        .map_err(|_| AppError::new("Internal", "repo command one-shot trust mutex poisoned"))?;
    if let Some((_, candidate)) = untrusted
        .iter()
        .find(|(fingerprint, _)| approvals.get(fingerprint).copied().unwrap_or_default() == 0)
    {
        return Err(AppError::new(
            "TrustRequired",
            format!(
                "Repository-provided {} command from {} must be trusted before it can run.",
                kind_label(candidate.kind),
                candidate.source_path.display(),
            ),
        ));
    }

    for (fingerprint, _) in untrusted {
        if let Some(count) = approvals.get_mut(&fingerprint) {
            *count -= 1;
            if *count == 0 {
                approvals.remove(&fingerprint);
            }
        }
    }
    Ok(())
}

#[must_use]
pub fn preview(target_worktree_path: PathBuf, cfg: &AppConfig, candidates: Vec<RepoCommandCandidate>) -> ShellCommandPreview {
    let mut commands = Vec::with_capacity(candidates.len());
    let mut trust_records = Vec::new();

    for candidate in candidates {
        let trusted = is_trusted(cfg, &candidate);
        commands.push(ShellCommandPreviewItem {
            kind: candidate.kind,
            source: ShellCommandSource::RepoSettings,
            command: candidate.command.clone(),
            target_worktree_path: candidate.target_worktree_path.clone(),
            scope: candidate.scope.clone(),
            source_path: Some(candidate.source_path.clone()),
            trusted,
        });
        if !trusted {
            trust_records.push(trust_record(&candidate, 0));
        }
    }

    ShellCommandPreview {
        target_worktree_path,
        trust_required: !trust_records.is_empty(),
        commands,
        trust_records,
    }
}

pub fn session_candidates(session: &Session) -> Result<Vec<RepoCommandCandidate>, AppError> {
    let command = normalize_session_command(session);
    let mut candidates = Vec::new();
    for provenance in &session.command_provenance {
        if provenance.source != ShellCommandSource::RepoSettings {
            continue;
        }
        let workspace_root = provenance.workspace_root.clone().ok_or_else(|| {
            AppError::new(
                "TrustRequired",
                format!("session {} has repo command provenance without a workspace root", session.id),
            )
        })?;
        let source_path = provenance.source_path.clone().ok_or_else(|| {
            AppError::new(
                "TrustRequired",
                format!("session {} has repo command provenance without a source path", session.id),
            )
        })?;
        candidates.push(RepoCommandCandidate {
            kind: provenance.kind,
            command: command.clone(),
            scope: provenance.scope.clone(),
            workspace_root,
            source_path,
            target_worktree_path: session.worktree_path.clone(),
        });
    }
    Ok(candidates)
}

pub fn ensure_session_trusted(cfg: &AppConfig, session: &Session) -> Result<(), AppError> {
    let candidates = session_candidates(session)?;
    ensure_trusted(cfg, &candidates)
}

#[must_use]
pub fn now_unix_seconds() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
