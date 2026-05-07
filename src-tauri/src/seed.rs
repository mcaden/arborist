//! Seed-on-first-launch for per-(branch, workspace) storage dirs.
//!
//! When a (branch, workspace) tuple is opened for the first time, its storage dir is empty. To make the user experience smooth across an upgrade —
//! and to avoid forcing every branch dev build to start with blank settings — we seed the new dir from the most-relevant existing source:
//!
//! 1. **Branch builds** seed `config.json` from the canonical (main / release)
//!    build's same-workspace settings, if present. They never seed sessions
//!    (each branch dev build starts with a fresh session list — see SPEC
//!    §C-04).
//! 2. **Canonical builds** seed `config.json` (and `sessions.json`) from the
//!    legacy top-level paths only when those paths' recorded `workspaceRoot`
//!    matches the workspace being seeded (or, for `config.json` first-launch,
//!    when no `workspaceRoot` is set yet — treat first-pick as adopt).
//! 3. If no seed source applies, the dir stays empty; a fresh
//!    [`AppConfig::default`] applies and `sessions.json` is absent.
//!
//! The marker for "this dir has already been seeded" is the existence of `<workspace_dir>/workspace-meta.json`, which is always written by
//! [`initialise_workspace_dir`] regardless of which sources matched. Using a dedicated marker (rather than overloading `config.json`) avoids a corner
//! case where a branch build with no canonical seed source would re-attempt the seed every launch.
//!
//! ## Concurrency
//!
//! Two Arborist processes can race to seed the same dir on first launch (e.g., the user double-clicks the icon). Correctness is preserved by:
//!
//! 1. Acquiring `<workspace_dir>/.config-seed.lock` *blocking* — the second
//!    process waits for the first to finish.
//! 2. Re-checking the marker (`workspace-meta.json`) **after** obtaining the
//!    lock — the lock-then-recheck pattern. The loser sees the marker the
//!    winner just wrote and returns [`SeedOutcome::AlreadySeeded`].
//!
//! The seed lock is released by dropping the returned guard at the end of [`initialise_workspace_dir`]; it is *not* the long-lived workspace lock
//! acquired by `WorkspaceScope` (a separate file).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tracing::{debug, warn};

use crate::store_layout::StoreLayout;
use crate::workspace_lock::{LockError, WorkspaceLockGuard};

/// What [`initialise_workspace_dir`] did (or didn't) for a given invocation. Carried back to the caller so boot-time logging can record the choice
/// without having to re-stat the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedOutcome {
    /// `workspace-meta.json` already existed — the dir was previously initialised, and no source files were copied.
    AlreadySeeded,
    /// `config.json` was copied from the canonical build's same- workspace settings (only possible for branch builds).
    SeededConfigFromCanonical,
    /// `config.json` was copied from the legacy top-level path (`<app_data_dir>/config.json`).
    SeededConfigFromLegacy,
    /// `sessions.json` was copied from the legacy top-level path (`<app_data_dir>/sessions.json`). May appear *in addition to* a `SeededConfig*`
    /// outcome — see [`SeedReport`].
    SeededSessionsFromLegacy,
    /// No seed source applied; the dir is fresh and only the `workspace-meta.json` sidecar was written.
    Fresh,
}

/// Aggregate outcome for one [`initialise_workspace_dir`] call. Multiple outcomes can co-occur (e.g. config from canonical, sessions from legacy), so
/// the report is a vector. Empty vector means no seeding happened (`AlreadySeeded` is signalled via
/// [`Self::already_seeded`]).
#[derive(Debug, Default)]
pub struct SeedReport {
    /// `true` iff the marker was already present and we returned without copying anything. Mutually exclusive with all other outcomes.
    pub already_seeded: bool,
    /// What was copied (in source-priority order). Empty for both `AlreadySeeded` and `Fresh`-only invocations.
    pub outcomes: Vec<SeedOutcome>,
}

/// Errors returned by [`initialise_workspace_dir`].
#[derive(Debug)]
pub enum SeedError {
    /// Failed to acquire the seed lock (e.g. permissions on `.config-seed.lock`'s parent dir). Distinct from a lock contention timeout —
    /// `acquire_blocking` waits indefinitely.
    Lock(LockError),
    /// I/O while reading a source file or writing a destination.
    Io(io::Error),
}

impl std::fmt::Display for SeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lock(e) => write!(f, "seed lock error: {e}"),
            Self::Io(e) => write!(f, "seed I/O error: {e}"),
        }
    }
}

impl std::error::Error for SeedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Lock(e) => Some(e),
            Self::Io(e) => Some(e),
        }
    }
}

impl From<io::Error> for SeedError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Sidecar written to `<workspace_dir>/workspace-meta.json` on first launch. Two purposes:
///
/// 1. Serves as the "this dir was initialised" marker for future launches
///    (presence-only check; field contents are diagnostic).
/// 2. Records the original canonicalised workspace path so a human inspecting
///    the on-disk store dir can tell which workspace this opaque-keyed
///    directory belongs to ([`crate::store_layout::workspace_key`] is one-way).
///
/// The schema is intentionally minimal and forward-compatible (extra fields are ignored on read). Versioning is unnecessary because the file is
/// purely informational — the marker test is `path.exists()`, not the parsed contents.
#[derive(Debug, Serialize, Deserialize)]
struct WorkspaceMeta {
    workspace: PathBuf,
    branch: String,
    initialised_at: u64,
}

/// Subset of `AppConfig` we need to read from a candidate seed source to verify its `workspaceRoot` matches the dir being seeded. Defined inline (not
/// reusing `AppConfig`) so an unrelated schema change in the source file can't break seed compatibility.
#[derive(Debug, Deserialize)]
struct LegacyConfigPeek {
    #[serde(default, rename = "workspaceRoot")]
    workspace_root: Option<PathBuf>,
}

/// Initialise (idempotently) the storage dir for `layout`, performing the seed-on-first-launch logic described at the module level. Safe to call
/// concurrently from multiple processes; only one will win the seed and the others return [`SeedReport::already_seeded`].
///
/// Always creates `layout.workspace_dir()` if missing, and always writes `workspace-meta.json` on the winning path so subsequent launches
/// short-circuit.
pub fn initialise_workspace_dir(layout: &StoreLayout) -> Result<SeedReport, SeedError> {
    let workspace_dir = layout.workspace_dir();
    fs::create_dir_all(&workspace_dir)?;

    // Block on the seed lock so concurrent first-launchers serialise through the marker re-check below.
    let _seed_lock = WorkspaceLockGuard::acquire_blocking(layout.seed_lock_path()).map_err(SeedError::Lock)?;

    // Lock-then-recheck: the predecessor may already have written the marker while we were waiting on the lock.
    let meta_path = layout.workspace_meta_path();
    if meta_path.exists() {
        return Ok(SeedReport {
            already_seeded: true,
            outcomes: Vec::new(),
        });
    }

    let mut outcomes = Vec::new();

    // ---- config.json ----
    //
    // Branch builds strip `lastOpenSessions` / `tabOrder` / `activeSessionId` from the seeded config because they don't also seed `sessions.json`
    // (per SPEC §C-04). Without the strip, the seeded config carries IDs that point at sessions which never existed in this storage dir — phantom IDs
    // that `restore_all_sessions`'s per-session worktree-missing trim never visits (it iterates over actual records, not config refs).
    // `restore_all_sessions` also has an upfront orphan-trim step as defense in depth, but the cleaner fix is to not produce the inconsistency in the
    // first place.
    let dest_config = layout.settings_path();
    let strip_session_fields = !layout.root().is_canonical();
    if !dest_config.exists() {
        if let Some(canonical_src) = layout.root().canonical_workspace_settings_path(layout.workspace()) {
            if canonical_src.exists() {
                copy_config_atomic(&canonical_src, &dest_config, &workspace_dir, strip_session_fields)?;
                outcomes.push(SeedOutcome::SeededConfigFromCanonical);
            }
        }
        if !dest_config.exists() {
            let legacy = layout.root().legacy_config_path();
            if should_seed_from_legacy_config(&legacy, layout) {
                copy_config_atomic(&legacy, &dest_config, &workspace_dir, strip_session_fields)?;
                outcomes.push(SeedOutcome::SeededConfigFromLegacy);
            }
        }
    }

    // ---- sessions.json ---- Only canonical builds seed sessions. Branch dev builds start with an empty session list per SPEC §C-04 so
    // feature-branch experiments don't entangle with the user's "real" session set.
    let dest_sessions = layout.sessions_path();
    if layout.root().is_canonical() && !dest_sessions.exists() {
        let legacy = layout.root().legacy_sessions_path();
        if legacy.exists() && legacy_workspace_root_matches(&layout.root().legacy_config_path(), layout) {
            copy_atomic(&legacy, &dest_sessions, &workspace_dir)?;
            outcomes.push(SeedOutcome::SeededSessionsFromLegacy);
        }
    }

    // ---- marker ---- Use `OpenOptions::create_new` to write the marker. Combined with the seed lock above, this gives belt-and-suspenders
    // atomicity: even if a buggy caller skipped the lock, two processes cannot both successfully write the marker. The loser sees
    // `ErrorKind::AlreadyExists` and reports `AlreadySeeded` instead of double-seeding.
    match write_marker_create_new(layout, &meta_path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Ok(SeedReport {
                already_seeded: true,
                outcomes: Vec::new(),
            });
        }
        Err(e) => return Err(SeedError::Io(e)),
    }

    if outcomes.is_empty() {
        outcomes.push(SeedOutcome::Fresh);
    }
    debug!(
        workspace_dir = %workspace_dir.display(),
        ?outcomes,
        "workspace dir initialised",
    );
    Ok(SeedReport {
        already_seeded: false,
        outcomes,
    })
}

/// Branch builds: `canonical_workspace_settings_path` may return `Some(...)` even if that file doesn't exist. Caller must check `exists()` before
/// copying. (Module-internal helper, kept private.)
///
/// Decide whether the legacy top-level config is a valid seed source:
/// * It must exist on disk.
/// * If it has a `workspaceRoot` field, it must match the workspace we're
///   seeding (i.e., the legacy install was last opened against the same repo).
/// * If the field is absent/null AND we're a canonical build, the user has not
///   yet adopted a workspace — treat first-pick as adopt and seed from legacy.
/// * Branch builds never seed from a `workspaceRoot`-less legacy config;
///   without the match check there's no reason to believe the legacy settings
///   belong to this workspace.
fn should_seed_from_legacy_config(legacy_path: &Path, layout: &StoreLayout) -> bool {
    if !legacy_path.exists() {
        return false;
    }
    match read_legacy_workspace_root(legacy_path) {
        Some(Some(legacy_root)) => paths_equal(&legacy_root, layout.workspace().as_path()),
        Some(None) => layout.root().is_canonical(),
        None => {
            // Parse failure on the candidate seed source: skip with a warning rather than propagating the error. The store will simply start fresh;
            // the user can re-import later.
            warn!(
                legacy = %legacy_path.display(),
                "legacy config unparseable; skipping as seed source",
            );
            false
        }
    }
}

/// For sessions seeding, we need `workspaceRoot` to match — the same rule applies but without the canonical-build "adopt" relaxation (sessions are
/// tied to a specific workspace's worktrees). Reads the *config* to check, since `sessions.json` itself doesn't carry `workspaceRoot`.
fn legacy_workspace_root_matches(legacy_config_path: &Path, layout: &StoreLayout) -> bool {
    if !legacy_config_path.exists() {
        return false;
    }
    matches!(
        read_legacy_workspace_root(legacy_config_path),
        Some(Some(root)) if paths_equal(&root, layout.workspace().as_path()),
    )
}

/// `Some(Some(path))` → field present and set. `Some(None)` → field present and explicitly null/absent (default). `None` → couldn't read or parse the
/// file.
fn read_legacy_workspace_root(path: &Path) -> Option<Option<PathBuf>> {
    let bytes = fs::read(path).ok()?;
    let peek: LegacyConfigPeek = serde_json::from_slice(&bytes).ok()?;
    Some(peek.workspace_root)
}

/// Compare two paths for "are these the same workspace?" semantics. Both should be canonicalised by the caller; we still apply `dunce::canonicalize`
/// defensively for the legacy path which was written by an older binary that may not have canonicalised.
fn paths_equal(a: &Path, b: &Path) -> bool {
    let a_canon = dunce::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let b_canon = dunce::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    a_canon == b_canon
}

/// Copy `src` → `dest` via a same-directory `NamedTempFile::persist`, matching `config_store::write_atomic`'s durability story (rename is atomic on
/// the same filesystem). `dir` is the parent of `dest` and must already exist.
fn copy_atomic(src: &Path, dest: &Path, dir: &Path) -> io::Result<()> {
    let bytes = fs::read(src)?;
    let mut tmp = NamedTempFile::new_in(dir)?;
    use std::io::Write as _;
    tmp.write_all(&bytes)?;
    tmp.flush()?;
    tmp.persist(dest).map_err(|e: tempfile::PersistError| e.error)?;
    Ok(())
}

/// Like [`copy_atomic`] but, when `strip_session_fields` is true, removes `lastOpenSessions`, `tabOrder`, and `activeSessionId` from the JSON before
/// writing. Used by branch builds seeding `config.json` without a paired `sessions.json` (those IDs would be phantoms).
///
/// If the source isn't a JSON object (corrupted file, unexpected schema), falls back to a verbatim copy — better to surface "garbage in, garbage out"
/// to the config-store loader (which quarantines bad files at load time) than to silently lose every other setting in the source.
fn copy_config_atomic(src: &Path, dest: &Path, dir: &Path, strip_session_fields: bool) -> io::Result<()> {
    let bytes = fs::read(src)?;
    let payload = if strip_session_fields {
        match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(serde_json::Value::Object(mut map)) => {
                map.remove("lastOpenSessions");
                map.remove("tabOrder");
                map.remove("activeSessionId");
                serde_json::to_vec_pretty(&serde_json::Value::Object(map)).map_err(io::Error::other)?
            }
            _ => bytes,
        }
    } else {
        bytes
    };
    let mut tmp = NamedTempFile::new_in(dir)?;
    use std::io::Write as _;
    tmp.write_all(&payload)?;
    tmp.flush()?;
    tmp.persist(dest).map_err(|e: tempfile::PersistError| e.error)?;
    Ok(())
}

fn write_marker_create_new(layout: &StoreLayout, dest: &Path) -> io::Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let meta = WorkspaceMeta {
        workspace: layout.workspace().as_path().to_path_buf(),
        branch: layout.root().branch().to_owned(),
        initialised_at: now,
    };
    let bytes = serde_json::to_vec_pretty(&meta).map_err(io::Error::other)?;
    // `create_new(true)` returns `ErrorKind::AlreadyExists` if the marker is present, which the caller treats as the loser-path outcome. We
    // deliberately don't go through a temp-file rename here because we *want* the loser to fail; an atomic rename (which `tempfile::persist` does)
    // would silently overwrite.
    let mut f = fs::OpenOptions::new().write(true).create_new(true).open(dest)?;
    use std::io::Write as _;
    f.write_all(&bytes)?;
    f.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_layout::StoreRoot;
    use std::sync::{Arc, Barrier};
    use tempfile::TempDir;

    fn touch_json(path: &Path, body: &serde_json::Value) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, serde_json::to_vec_pretty(body).unwrap()).unwrap();
    }

    fn canon(p: &Path) -> crate::store_layout::CanonicalPath {
        crate::store_layout::CanonicalPath::canonicalise(p).unwrap()
    }

    /// First-launch with no seed source: marker is written, no other files are created. Re-running is idempotent.
    #[test]
    fn fresh_first_launch_writes_only_marker() {
        let app_data = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();

        let root = StoreRoot::new(app_data.path().to_path_buf(), "feature-x".to_owned());
        let layout = root.for_workspace(&canon(workspace.path()));

        let report = initialise_workspace_dir(&layout).unwrap();
        assert!(!report.already_seeded);
        assert_eq!(report.outcomes, vec![SeedOutcome::Fresh]);
        assert!(layout.workspace_meta_path().exists());
        assert!(!layout.settings_path().exists());
        assert!(!layout.sessions_path().exists());

        let again = initialise_workspace_dir(&layout).unwrap();
        assert!(again.already_seeded);
        assert!(again.outcomes.is_empty());
    }

    /// Branch build with a matching canonical workspace settings file seeds `config.json` from canonical and never touches sessions. Per SPEC §C-04
    /// (branch builds start with a fresh session list), the seeded copy must have `lastOpenSessions` / `tabOrder` / `activeSessionId` stripped —
    /// otherwise it would carry IDs that point at sessions which never existed in this branch's `sessions.json`.
    #[test]
    fn branch_build_seeds_config_from_canonical_only() {
        let app_data = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let workspace_canon = canon(workspace.path());

        let canonical_root = StoreRoot::new(app_data.path().to_path_buf(), "main".to_owned());
        let canonical_layout = canonical_root.for_workspace(&workspace_canon);
        // Pre-seed the canonical workspace settings, including session-list fields the strip must remove.
        touch_json(
            &canonical_layout.settings_path(),
            &serde_json::json!({
                "configVersion": 4,
                "instructionSetsDir": "/x",
                "lastOpenSessions": ["550e8400-e29b-41d4-a716-446655440000"],
                "tabOrder": ["550e8400-e29b-41d4-a716-446655440000"],
                "activeSessionId": "550e8400-e29b-41d4-a716-446655440000",
            }),
        );
        // Pre-create a canonical sessions file too — branch build must IGNORE it.
        fs::create_dir_all(canonical_layout.sessions_path().parent().unwrap()).unwrap();
        fs::write(canonical_layout.sessions_path(), b"{}").unwrap();

        let branch_root = StoreRoot::new(app_data.path().to_path_buf(), "feature-x".to_owned());
        let branch_layout = branch_root.for_workspace(&workspace_canon);
        let report = initialise_workspace_dir(&branch_layout).unwrap();

        assert!(!report.already_seeded);
        assert_eq!(report.outcomes, vec![SeedOutcome::SeededConfigFromCanonical]);
        assert!(branch_layout.settings_path().exists());
        assert!(!branch_layout.sessions_path().exists(), "branch build must never seed sessions",);

        // Strip assertion: session-list fields must be absent in the seeded copy. Other fields must survive.
        let seeded: serde_json::Value = serde_json::from_slice(&fs::read(branch_layout.settings_path()).unwrap()).unwrap();
        let obj = seeded.as_object().expect("seeded config is an object");
        assert!(
            !obj.contains_key("lastOpenSessions"),
            "lastOpenSessions must be stripped from branch-seeded config"
        );
        assert!(!obj.contains_key("tabOrder"), "tabOrder must be stripped from branch-seeded config");
        assert!(
            !obj.contains_key("activeSessionId"),
            "activeSessionId must be stripped from branch-seeded config"
        );
        assert_eq!(
            obj.get("instructionSetsDir").and_then(|v| v.as_str()),
            Some("/x"),
            "non-session fields must survive the strip"
        );
        assert_eq!(
            obj.get("configVersion").and_then(|v| v.as_u64()),
            Some(4),
            "configVersion must survive the strip"
        );
    }

    /// Branch build seeded from legacy `config.json` (the upgrade path that produced the user-reported phantom-IDs bug). Same strip rule as the
    /// canonical-source variant above.
    #[test]
    fn branch_build_seeds_config_from_legacy_strips_session_fields() {
        let app_data = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let workspace_canon = canon(workspace.path());

        let root = StoreRoot::new(app_data.path().to_path_buf(), "feature-x".to_owned());
        let layout = root.for_workspace(&workspace_canon);

        // Legacy config from an old canonical install, with a matching workspaceRoot so the seed is allowed.
        touch_json(
            &root.legacy_config_path(),
            &serde_json::json!({
                "configVersion": 4,
                "workspaceRoot": workspace_canon.as_path().to_string_lossy(),
                "instructionSetsDir": "/y",
                "lastOpenSessions": ["550e8400-e29b-41d4-a716-446655440001"],
                "tabOrder": ["550e8400-e29b-41d4-a716-446655440001"],
                "activeSessionId": "550e8400-e29b-41d4-a716-446655440001",
            }),
        );

        let report = initialise_workspace_dir(&layout).unwrap();
        assert!(report.outcomes.contains(&SeedOutcome::SeededConfigFromLegacy));
        assert!(!layout.sessions_path().exists(), "branch build must never seed sessions");

        let seeded: serde_json::Value = serde_json::from_slice(&fs::read(layout.settings_path()).unwrap()).unwrap();
        let obj = seeded.as_object().expect("seeded config is an object");
        assert!(!obj.contains_key("lastOpenSessions"));
        assert!(!obj.contains_key("tabOrder"));
        assert!(!obj.contains_key("activeSessionId"));
        assert_eq!(obj.get("instructionSetsDir").and_then(|v| v.as_str()), Some("/y"));
        assert_eq!(
            obj.get("workspaceRoot").and_then(|v| v.as_str()),
            Some(workspace_canon.as_path().to_string_lossy().as_ref()),
            "workspaceRoot must survive the strip"
        );
    }

    /// Canonical build with a matching legacy `workspaceRoot` seeds both `config.json` and `sessions.json` from the legacy paths. Unlike branch
    /// builds, canonical builds do NOT strip session-list fields — the paired `sessions.json` keeps the IDs valid.
    #[test]
    fn canonical_build_seeds_config_and_sessions_from_matching_legacy() {
        let app_data = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let workspace_canon = canon(workspace.path());

        let root = StoreRoot::new(app_data.path().to_path_buf(), "main".to_owned());
        let layout = root.for_workspace(&workspace_canon);

        touch_json(
            &root.legacy_config_path(),
            &serde_json::json!({
                "configVersion": 4,
                "workspaceRoot": workspace_canon.as_path().to_string_lossy(),
                "lastOpenSessions": ["550e8400-e29b-41d4-a716-446655440000"],
                "tabOrder": ["550e8400-e29b-41d4-a716-446655440000"],
                "activeSessionId": "550e8400-e29b-41d4-a716-446655440000",
            }),
        );
        fs::write(root.legacy_sessions_path(), b"{}").unwrap();

        let report = initialise_workspace_dir(&layout).unwrap();
        assert!(!report.already_seeded);
        assert!(report.outcomes.contains(&SeedOutcome::SeededConfigFromLegacy));
        assert!(report.outcomes.contains(&SeedOutcome::SeededSessionsFromLegacy));
        assert!(layout.settings_path().exists());
        assert!(layout.sessions_path().exists());

        // Canonical build must preserve session-list fields verbatim because it also seeded the paired sessions.json.
        let seeded: serde_json::Value = serde_json::from_slice(&fs::read(layout.settings_path()).unwrap()).unwrap();
        let obj = seeded.as_object().unwrap();
        assert!(obj.contains_key("lastOpenSessions"), "canonical seed must NOT strip lastOpenSessions");
        assert!(obj.contains_key("tabOrder"));
        assert!(obj.contains_key("activeSessionId"));
    }

    /// Legacy `workspaceRoot` set but pointing at a different workspace: don't seed (would mix unrelated state).
    #[test]
    fn legacy_with_mismatched_workspace_root_is_skipped() {
        let app_data = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let workspace_canon = canon(workspace.path());

        let root = StoreRoot::new(app_data.path().to_path_buf(), "main".to_owned());
        let layout = root.for_workspace(&workspace_canon);

        touch_json(
            &root.legacy_config_path(),
            &serde_json::json!({
                "configVersion": 4,
                "workspaceRoot": canon(other.path()).as_path().to_string_lossy(),
            }),
        );
        fs::write(root.legacy_sessions_path(), b"{}").unwrap();

        let report = initialise_workspace_dir(&layout).unwrap();
        assert_eq!(report.outcomes, vec![SeedOutcome::Fresh]);
        assert!(!layout.settings_path().exists());
        assert!(!layout.sessions_path().exists());
    }

    /// Canonical build, legacy config exists with no `workspaceRoot`: adopt-on-first-pick. Sessions still need the explicit match, so they are NOT
    /// seeded here.
    #[test]
    fn canonical_with_unset_legacy_workspace_root_adopts_config_only() {
        let app_data = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let workspace_canon = canon(workspace.path());

        let root = StoreRoot::new(app_data.path().to_path_buf(), "main".to_owned());
        let layout = root.for_workspace(&workspace_canon);

        touch_json(&root.legacy_config_path(), &serde_json::json!({"configVersion": 4}));
        fs::write(root.legacy_sessions_path(), b"{}").unwrap();

        let report = initialise_workspace_dir(&layout).unwrap();
        assert!(report.outcomes.contains(&SeedOutcome::SeededConfigFromLegacy));
        assert!(!layout.sessions_path().exists());
    }

    /// Branch build with no canonical settings AND legacy with unset `workspaceRoot`: do NOT adopt — adopt only applies to canonical builds. Result
    /// is Fresh.
    #[test]
    fn branch_build_does_not_adopt_unset_legacy_workspace_root() {
        let app_data = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();

        let root = StoreRoot::new(app_data.path().to_path_buf(), "feature-x".to_owned());
        let layout = root.for_workspace(&canon(workspace.path()));

        touch_json(&root.legacy_config_path(), &serde_json::json!({"configVersion": 4}));

        let report = initialise_workspace_dir(&layout).unwrap();
        assert_eq!(report.outcomes, vec![SeedOutcome::Fresh]);
        assert!(!layout.settings_path().exists());
    }

    /// Two threads racing to seed the same dir: exactly one performs the copy, the other returns `already_seeded`. Proves the blocking lock +
    /// lock-then-recheck pattern.
    #[test]
    fn concurrent_first_launch_serialises_one_winner() {
        let app_data = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let workspace_canon = canon(workspace.path());

        let root = StoreRoot::new(app_data.path().to_path_buf(), "main".to_owned());
        let layout = root.for_workspace(&workspace_canon);
        // Seed source so the winner has work to do (makes the test more sensitive to races than a pure-Fresh case).
        touch_json(
            &root.legacy_config_path(),
            &serde_json::json!({
                "configVersion": 4,
                "workspaceRoot": workspace_canon.as_path().to_string_lossy(),
            }),
        );

        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let layout = layout.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    initialise_workspace_dir(&layout).unwrap()
                })
            })
            .collect();

        let reports: Vec<SeedReport> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners = reports.iter().filter(|r| !r.already_seeded).count();
        let losers = reports.iter().filter(|r| r.already_seeded).count();
        assert_eq!(winners, 1, "exactly one thread must win the seed");
        assert_eq!(losers, 1, "the other thread must observe AlreadySeeded");
        assert!(layout.workspace_meta_path().exists());
        assert!(layout.settings_path().exists());
    }

    /// Marker presence short-circuits everything, even if a juicier seed source appears later. Proves the marker is the source of truth, not the
    /// contents of `config.json`.
    #[test]
    fn marker_short_circuits_subsequent_seeds() {
        let app_data = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let workspace_canon = canon(workspace.path());

        let root = StoreRoot::new(app_data.path().to_path_buf(), "main".to_owned());
        let layout = root.for_workspace(&workspace_canon);

        // First call: nothing to seed, marker written.
        let first = initialise_workspace_dir(&layout).unwrap();
        assert_eq!(first.outcomes, vec![SeedOutcome::Fresh]);

        // Now create a legacy seed source. A subsequent init MUST NOT re-seed because the marker is already there.
        touch_json(
            &root.legacy_config_path(),
            &serde_json::json!({
                "configVersion": 4,
                "workspaceRoot": workspace_canon.as_path().to_string_lossy(),
            }),
        );
        fs::write(root.legacy_sessions_path(), b"{}").unwrap();

        let second = initialise_workspace_dir(&layout).unwrap();
        assert!(second.already_seeded);
        assert!(!layout.settings_path().exists());
        assert!(!layout.sessions_path().exists());
    }

    /// Malformed legacy config must not propagate as an error — the dir is initialised fresh and a warning is logged.
    #[test]
    fn malformed_legacy_config_falls_through_to_fresh() {
        let app_data = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();

        let root = StoreRoot::new(app_data.path().to_path_buf(), "main".to_owned());
        let layout = root.for_workspace(&canon(workspace.path()));

        fs::create_dir_all(root.legacy_config_path().parent().unwrap()).unwrap();
        fs::write(root.legacy_config_path(), b"not json {{{").unwrap();

        let report = initialise_workspace_dir(&layout).unwrap();
        assert_eq!(report.outcomes, vec![SeedOutcome::Fresh]);
    }

    /// Belt-and-suspenders defence: even if the seed lock weren't effective, a pre-existing marker must not be overwritten by
    /// `write_marker_create_new`. We pre-write a marker without taking the lock and then call `initialise_workspace_dir`; it must report
    /// `AlreadySeeded` because the `create_new(true)` on the marker write fails with `AlreadyExists`. Proves the second-line-of-defence the reviewer
    /// asked for.
    #[test]
    fn pre_existing_marker_is_never_overwritten() {
        let app_data = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let root = StoreRoot::new(app_data.path().to_path_buf(), "main".to_owned());
        let layout = root.for_workspace(&canon(workspace.path()));

        // Hand-write a marker (NOT via initialise) — simulates a sibling process that won the seed first.
        fs::create_dir_all(layout.workspace_dir()).unwrap();
        fs::write(layout.workspace_meta_path(), br#"{"sentinel":true}"#).unwrap();

        let report = initialise_workspace_dir(&layout).unwrap();
        assert!(report.already_seeded);
        assert!(report.outcomes.is_empty());

        // Marker contents must be preserved (not overwritten).
        let bytes = fs::read(layout.workspace_meta_path()).unwrap();
        assert!(bytes.windows(8).any(|w| w == b"sentinel"));
    }
}
