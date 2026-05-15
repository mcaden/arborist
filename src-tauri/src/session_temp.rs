//! Hardened session temp-directory and Copilot OTel file handling.
//!
//! All active per-session temp paths live below `<os-temp>/arborist/<session-uuid>`. This module keeps creation and cleanup on the narrow, expected
//! path set: UUID-named session directories and, for Copilot, the single `otel.jsonl` file. Cleanup refuses symlinks and Windows reparse points rather
//! than following them.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

use tracing::warn;

use crate::compose;
use crate::types::{Error, SessionId};

#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;
#[cfg(windows)]
const WINDOWS_FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// The only active Copilot telemetry temp filename Arborist owns.
pub const COPILOT_OTEL_FILE_NAME: &str = "otel.jsonl";

/// The only active Claude hook-events filename Arborist owns. The [`crate::claude_hook_events`] tailer reads it; the `arborist-claude-hook` sidecar
/// helper appends to it.
pub const CLAUDE_HOOK_EVENTS_FILE_NAME: &str = "hook-events.jsonl";

/// Ensure `<os-temp>/arborist/<session-uuid>` exists and is private to the current user where the platform supports Unix-style modes.
pub fn ensure_session_temp_dir(id: &SessionId) -> Result<PathBuf, Error> {
    let root = compose::session_temp_root();
    ensure_private_dir(&root)?;

    let dir = compose::session_temp_dir(id);
    validate_session_dir_path(id, &dir)?;
    ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Reset Copilot's OTel JSONL file to an empty owner-only file under the expected session temp directory.
pub fn prepare_copilot_otel_file(id: &SessionId) -> Result<PathBuf, Error> {
    let dir = ensure_session_temp_dir(id)?;
    let path = compose::copilot_otel_path(id);
    validate_copilot_otel_path(id, &path, &dir)?;
    remove_regular_file_no_follow(&path)?;
    create_private_file_new(&path)?;
    Ok(path)
}

/// Reset Claude's hook-events JSONL file. Unlike Copilot's exporter, the `arborist-claude-hook` helper creates the file on its first append, so we
/// only need to *remove* any leftover from a prior spawn (restart paths). The dir is still ensured so [`crate::claude_hook_events::run_watcher`] can
/// open the path lazily.
pub fn prepare_claude_hook_events_file(id: &SessionId) -> Result<PathBuf, Error> {
    let dir = ensure_session_temp_dir(id)?;
    let path = crate::claude_hook_events::hook_events_path(id);
    validate_claude_hook_events_path(id, &path, &dir)?;
    remove_regular_file_no_follow(&path)?;
    Ok(path)
}

/// Remove the exact Copilot OTel JSONL file for a session, if present.
pub fn remove_copilot_otel_file(id: &SessionId) -> Result<bool, Error> {
    let root = compose::session_temp_root();
    let root_meta = match fs::symlink_metadata(&root) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(Error::Io(e)),
    };
    reject_special_or_non_dir(&root, &root_meta)?;
    set_private_dir_permissions(&root)?;

    let dir = compose::session_temp_dir(id);
    let dir_meta = match fs::symlink_metadata(&dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(Error::Io(e)),
    };
    validate_session_dir_path(id, &dir)?;
    reject_special_or_non_dir(&dir, &dir_meta)?;
    set_private_dir_permissions(&dir)?;

    let path = compose::copilot_otel_path(id);
    validate_copilot_otel_path(id, &path, &dir)?;
    remove_regular_file_no_follow(&path)
}

/// Remove a session temp directory without traversing symlinks or Windows reparse points.
pub fn remove_session_temp_dir(id: &SessionId) -> Result<bool, Error> {
    let root = compose::session_temp_root();
    let root_meta = match fs::symlink_metadata(&root) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(Error::Io(e)),
    };
    reject_special_or_non_dir(&root, &root_meta)?;
    set_private_dir_permissions(&root)?;

    let dir = compose::session_temp_dir(id);
    validate_session_dir_path(id, &dir)?;
    remove_dir_tree_no_follow(&dir)
}

/// Scan `<os-temp>/arborist/` for UUID-named orphan session temp directories older than `age_threshold`.
pub fn cleanup_orphan_session_temp_dirs(persisted_session_ids: &[SessionId], age_threshold: Duration) -> Result<usize, Error> {
    let root = compose::session_temp_root();
    let root_meta = match fs::symlink_metadata(&root) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(Error::Io(e)),
    };
    reject_special_or_non_dir(&root, &root_meta)?;
    set_private_dir_permissions(&root)?;

    let persisted: HashSet<String> = persisted_session_ids.iter().map(|id| id.0.to_string()).collect();
    let now = SystemTime::now();
    let mut deleted = 0usize;

    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(uuid) = uuid::Uuid::parse_str(name) else {
            continue;
        };
        if persisted.contains(name) {
            continue;
        }

        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "cleanup_orphans: could not inspect candidate; skipping");
                continue;
            }
        };
        if is_symlink_or_reparse_point(&meta) {
            warn!(path = %path.display(), "cleanup_orphans: refusing to remove symlink or reparse-point candidate");
            continue;
        }
        if !meta.is_dir() {
            continue;
        }

        let mtime = meta.modified().unwrap_or(now);
        let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);
        if age < age_threshold {
            continue;
        }

        match remove_session_temp_dir(&SessionId(uuid)) {
            Ok(true) => deleted += 1,
            Ok(false) => {}
            Err(e) => warn!(dir = %path.display(), error = %e, "cleanup_orphans: hardened removal failed"),
        }
    }

    Ok(deleted)
}

fn ensure_private_dir(path: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            reject_special_or_non_dir(path, &meta)?;
            set_private_dir_permissions(path)?;
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            create_private_dir(path)?;
            let meta = fs::symlink_metadata(path)?;
            reject_special_or_non_dir(path, &meta)?;
            set_private_dir_permissions(path)?;
            Ok(())
        }
        Err(e) => Err(Error::Io(e)),
    }
}

fn create_private_dir(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    let result = {
        let mut builder = fs::DirBuilder::new();
        builder.mode(PRIVATE_DIR_MODE);
        builder.create(path)
    };
    #[cfg(not(unix))]
    let result = fs::DirBuilder::new().create(path);

    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(map_temp_permission_error(path, "create private session temp directory", e)),
    }
}

fn set_private_dir_permissions(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIR_MODE))
            .map_err(|e| map_temp_permission_error(path, "set owner-only permissions on session temp directory", e))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_FILE_MODE))
            .map_err(|e| map_temp_permission_error(path, "set owner-only permissions on Copilot OTel file", e))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn map_temp_permission_error(path: &Path, action: &str, e: io::Error) -> Error {
    if e.kind() == io::ErrorKind::PermissionDenied {
        Error::PermissionDenied(format!(
            "could not {action} {}; ensure the temp path is owned by the current user or remove it: {e}",
            path.display()
        ))
    } else {
        Error::Io(e)
    }
}

fn create_private_file_new(path: &Path) -> Result<(), Error> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(PRIVATE_FILE_MODE);
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }

    let file = options.open(path)?;
    drop(file);
    set_private_file_permissions(path)?;
    Ok(())
}

fn remove_regular_file_no_follow(path: &Path) -> Result<bool, Error> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(Error::Io(e)),
    };
    if is_symlink_or_reparse_point(&meta) {
        return Err(Error::InvalidPath(format!(
            "refusing to remove symlink or reparse point {}",
            path.display()
        )));
    }
    if !meta.is_file() {
        return Err(Error::InvalidPath(format!("refusing to remove non-file {}", path.display())));
    }
    fs::remove_file(path)?;
    Ok(true)
}

fn remove_dir_tree_no_follow(path: &Path) -> Result<bool, Error> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(Error::Io(e)),
    };
    reject_special_or_non_dir(path, &meta)?;

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        let child_meta = fs::symlink_metadata(&child)?;
        if is_symlink_or_reparse_point(&child_meta) {
            return Err(Error::InvalidPath(format!(
                "refusing to remove symlink or reparse point inside session temp dir: {}",
                child.display()
            )));
        }
        if child_meta.is_dir() {
            remove_dir_tree_no_follow(&child)?;
        } else {
            fs::remove_file(&child)?;
        }
    }

    fs::remove_dir(path)?;
    Ok(true)
}

fn validate_session_dir_path(id: &SessionId, dir: &Path) -> Result<(), Error> {
    let expected = compose::session_temp_root().join(id.0.to_string());
    if dir != expected {
        return Err(Error::InvalidPath(format!(
            "session temp dir {} did not match expected {}",
            dir.display(),
            expected.display()
        )));
    }
    Ok(())
}

fn validate_copilot_otel_path(id: &SessionId, path: &Path, dir: &Path) -> Result<(), Error> {
    validate_session_dir_path(id, dir)?;
    let expected = dir.join(COPILOT_OTEL_FILE_NAME);
    if path != expected {
        return Err(Error::InvalidPath(format!(
            "Copilot OTel path {} did not match expected {}",
            path.display(),
            expected.display()
        )));
    }
    Ok(())
}

fn validate_claude_hook_events_path(id: &SessionId, path: &Path, dir: &Path) -> Result<(), Error> {
    validate_session_dir_path(id, dir)?;
    let expected = dir.join(CLAUDE_HOOK_EVENTS_FILE_NAME);
    if path != expected {
        return Err(Error::InvalidPath(format!(
            "Claude hook-events path {} did not match expected {}",
            path.display(),
            expected.display()
        )));
    }
    Ok(())
}

fn reject_special_or_non_dir(path: &Path, meta: &fs::Metadata) -> Result<(), Error> {
    if is_symlink_or_reparse_point(meta) {
        return Err(Error::InvalidPath(format!("refusing to use symlink or reparse point {}", path.display())));
    }
    if !meta.is_dir() {
        return Err(Error::InvalidPath(format!("{} is not a directory", path.display())));
    }
    Ok(())
}

fn is_symlink_or_reparse_point(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink() || is_windows_reparse_point(meta)
}

#[cfg(windows)]
fn is_windows_reparse_point(meta: &fs::Metadata) -> bool {
    meta.file_attributes() & WINDOWS_FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_windows_reparse_point(_meta: &fs::Metadata) -> bool {
    false
}
