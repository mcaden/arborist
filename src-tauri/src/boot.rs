//! Boot-time workspace selection (Phase 6 of per-(branch, workspace)
//! settings isolation).
//!
//! ## Why a separate module
//!
//! The new layout (`<app_data_dir>/branches/<branch>/workspaces/<key>/...`)
//! must be activated **before** [`crate::commands::AppContext`] is built —
//! otherwise the legacy top-level `config.json`/`sessions.json` would be
//! used as a fallback, defeating the isolation guarantee. So workspace
//! resolution, OS-lock acquisition, and seed-on-first-launch all happen
//! synchronously in this module from `lib::run()`'s setup hook before
//! the `AppContext` is constructed.
//!
//! ## Resolution priority
//!
//! 1. `--workspace <path>` (or `--workspace=<path>`) CLI argument.
//! 2. The branch-specific `last-workspace.json` hint file written by
//!    a previous successful bind.
//! 3. Legacy `<app_data_dir>/config.json::workspace_root` (one-time
//!    migration breadcrumb so existing single-canonical-install users
//!    pick up the new layout on their first upgraded launch).
//! 4. Native folder picker dialog ([`rfd::FileDialog::pick_folder`]).
//!    The user can cancel — the app then exits cleanly.
//!
//! ## Lock contention
//!
//! When `bind_workspace` cannot acquire the per-(branch, workspace) OS
//! lock (because another Arborist process — same branch, same
//! workspace — already holds it), we surface a synchronous native
//! message dialog ([`rfd::MessageDialog`]) naming the branch + workspace
//! and exit with a non-zero status. The dialog is the user's signal
//! that this isn't an Arborist crash but a deliberate single-writer
//! refusal.
//!
//! ## Why `rfd` instead of `tauri-plugin-dialog`
//!
//! `tauri-plugin-dialog` requires an `AppHandle`, which by the time
//! we're inside `setup` is half-built. Routing the failure path
//! through the half-built Tauri app introduces ordering hazards (the
//! main webview window may or may not exist yet). `rfd` is a
//! standalone synchronous OS-dialog crate (already a transitive dep
//! via `tauri-plugin-dialog`), so we use it directly for boot-time
//! UX. No `AppHandle` needed; works before any Tauri lifecycle.

use std::path::{Path, PathBuf};
use std::{ffi::OsString, fs};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config_store::ConfigStore;
use crate::git::GitRunner;
use crate::seed::{initialise_workspace_dir, SeedError};
use crate::store_layout::{is_canonical_build, StoreLayout, StoreRoot};
use crate::workspace_lock::{LockError, WorkspaceLockGuard};
use crate::workspace_scope::WorkspaceScope;

const HINT_FILE_NAME: &str = "last-workspace.json";
const LEGACY_CONFIG_FILE_NAME: &str = "config.json";

#[derive(Debug, thiserror::Error)]
pub enum BootError {
    #[error("CLI parse error: {0}")]
    Cli(String),
    #[error("workspace path does not exist or is not a directory: {0}")]
    InvalidWorkspace(PathBuf),
    #[error("workspace path is not a git repository root ({workspace}): {reason}")]
    NotARepository { workspace: PathBuf, reason: String },
    #[error("failed to canonicalise workspace path {path}: {source}")]
    Canonicalise {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("workspace already open in another Arborist window (branch={branch}, workspace={workspace})")]
    Contention { branch: String, workspace: PathBuf },
    #[error("failed to acquire workspace lock for {workspace}: {source}")]
    Lock {
        workspace: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("seed failed: {0}")]
    Seed(#[from] SeedError),
    #[error("failed to open config store at {dir}: {source}")]
    ConfigStore {
        dir: PathBuf,
        #[source]
        source: crate::types::Error,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Parsed CLI arguments. Today only `--workspace <path>` /
/// `--workspace=<path>`. Unknown args are ignored (forward-compat).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CliArgs {
    pub workspace: Option<PathBuf>,
}

/// Parse `--workspace <path>` / `--workspace=<path>` from an arbitrary
/// argv (the binary name at index 0 is skipped).
///
/// Errors:
/// * Missing value after `--workspace` (last arg, or next arg starts
///   with `--`).
/// * Duplicate `--workspace` (specified more than once).
/// * Empty value (e.g. `--workspace=`).
pub fn parse_cli_args<I, S>(argv: I) -> Result<CliArgs, BootError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut args = argv.into_iter().map(Into::into).peekable();
    args.next(); // skip argv[0] (binary name)

    let mut out = CliArgs::default();
    while let Some(arg) = args.next() {
        let s = match arg.to_str() {
            Some(s) => s.to_string(),
            None => continue, // non-UTF-8 args are ignored (forward-compat)
        };
        if let Some(value) = s.strip_prefix("--workspace=") {
            if value.is_empty() {
                return Err(BootError::Cli("--workspace= requires a value".into()));
            }
            if out.workspace.is_some() {
                return Err(BootError::Cli(
                    "--workspace specified more than once".into(),
                ));
            }
            out.workspace = Some(PathBuf::from(value));
        } else if s == "--workspace" {
            let next = args
                .next()
                .ok_or_else(|| BootError::Cli("--workspace requires a path argument".into()))?;
            let next_str = next
                .to_str()
                .ok_or_else(|| BootError::Cli("--workspace value is not valid UTF-8".into()))?;
            if next_str.is_empty() || next_str.starts_with("--") {
                return Err(BootError::Cli(
                    "--workspace requires a path argument (got flag-like value)".into(),
                ));
            }
            if out.workspace.is_some() {
                return Err(BootError::Cli(
                    "--workspace specified more than once".into(),
                ));
            }
            out.workspace = Some(PathBuf::from(next_str));
        }
        // unknown args ignored
    }
    Ok(out)
}

/// Where the per-branch hint file lives. Mirrors the storage layout:
/// canonical builds keep it at `<app_data_dir>/last-workspace.json`,
/// branch builds at `<app_data_dir>/branches/<branch>/last-workspace.json`.
#[must_use]
pub fn hint_file_path(app_data_dir: &Path, branch: &str) -> PathBuf {
    if is_canonical_build(branch) {
        app_data_dir.join(HINT_FILE_NAME)
    } else {
        app_data_dir
            .join("branches")
            .join(branch.trim())
            .join(HINT_FILE_NAME)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct HintFile {
    #[serde(rename = "workspaceRoot")]
    workspace_root: PathBuf,
}

/// Read the hint file, returning `None` if it doesn't exist, isn't
/// readable, or is malformed. We never propagate hint errors: stale
/// or broken hints fall through to the next resolution step.
#[must_use]
pub fn read_hint(app_data_dir: &Path, branch: &str) -> Option<PathBuf> {
    let path = hint_file_path(app_data_dir, branch);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            debug!(?path, error = %e, "no hint file");
            return None;
        }
    };
    match serde_json::from_slice::<HintFile>(&bytes) {
        Ok(h) => Some(h.workspace_root),
        Err(e) => {
            warn!(?path, error = %e, "hint file malformed; ignoring");
            None
        }
    }
}

/// Atomically write the hint file (tempfile + rename). The hint is a
/// single canonicalised absolute path the user most recently bound.
pub fn write_hint(app_data_dir: &Path, branch: &str, workspace_root: &Path) -> std::io::Result<()> {
    let path = hint_file_path(app_data_dir, branch);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let payload = HintFile {
        workspace_root: workspace_root.to_path_buf(),
    };
    let tmp = tempfile::Builder::new()
        .prefix(".last-workspace-")
        .suffix(".json")
        .tempfile_in(path.parent().unwrap_or(app_data_dir))?;
    serde_json::to_writer(tmp.as_file(), &payload).map_err(std::io::Error::other)?;
    tmp.as_file().sync_all()?;
    tmp.persist(&path)
        .map_err(|e| std::io::Error::other(format!("persist hint failed: {e}")))?;
    Ok(())
}

/// Best-effort read of `<app_data_dir>/config.json::workspace_root`
/// for the legacy migration breadcrumb. Returns `None` if the legacy
/// file doesn't exist, is unreadable, or has no `workspace_root` set.
#[must_use]
pub fn read_legacy_workspace_root(app_data_dir: &Path) -> Option<PathBuf> {
    let path = app_data_dir.join(LEGACY_CONFIG_FILE_NAME);
    let bytes = fs::read(&path).ok()?;
    #[derive(Deserialize)]
    struct Peek {
        #[serde(rename = "workspaceRoot")]
        workspace_root: Option<PathBuf>,
    }
    serde_json::from_slice::<Peek>(&bytes).ok()?.workspace_root
}

/// Resolve the workspace to bind, following the priority chain
/// (CLI → hint → legacy → `None`). Returns `None` to mean "fall back
/// to the picker dialog". The CLI arg is hard-failed (returns
/// `Err`) if it points at a non-directory **or a non-git-repo
/// path**; hint/legacy mismatches (missing dir, *or* not a git
/// repository) silently fall through to the next step.
pub fn resolve_boot_workspace(
    args: &CliArgs,
    app_data_dir: &Path,
    branch: &str,
    git_runner: &dyn GitRunner,
) -> Result<Option<PathBuf>, BootError> {
    if let Some(p) = &args.workspace {
        let canon = canonicalise_existing(p)?;
        validate_repo_root(&canon, git_runner)?;
        return Ok(Some(canon));
    }
    if let Some(p) = read_hint(app_data_dir, branch) {
        match canonicalise_existing(&p) {
            Ok(canon) => match validate_repo_root(&canon, git_runner) {
                Ok(()) => return Ok(Some(canon)),
                Err(e) => warn!(
                    path = ?canon, error = %e,
                    "hint workspace is no longer a git repository; ignoring"
                ),
            },
            Err(_) => warn!(path = ?p, "hint workspace no longer exists; ignoring"),
        }
    }
    if let Some(p) = read_legacy_workspace_root(app_data_dir) {
        match canonicalise_existing(&p) {
            Ok(canon) => match validate_repo_root(&canon, git_runner) {
                Ok(()) => return Ok(Some(canon)),
                Err(e) => warn!(
                    path = ?canon, error = %e,
                    "legacy workspace_root is no longer a git repository; ignoring"
                ),
            },
            Err(_) => warn!(path = ?p, "legacy workspace_root no longer exists; ignoring"),
        }
    }
    Ok(None)
}

fn canonicalise_existing(p: &Path) -> Result<PathBuf, BootError> {
    let canon = dunce::canonicalize(p).map_err(|source| BootError::Canonicalise {
        path: p.to_path_buf(),
        source,
    })?;
    if !canon.is_dir() {
        return Err(BootError::InvalidWorkspace(canon));
    }
    Ok(canon)
}

/// Verify that `canon` is the root of a git repository (i.e. running
/// `git rev-parse --show-toplevel` from inside it returns `canon`
/// itself). Mirrors the check that
/// [`crate::commands::workspace_validate_impl`] applies to user-picked
/// paths in the in-app picker, so the boot resolution chain (CLI arg,
/// last-workspace hint, legacy migration breadcrumb, native picker)
/// can't bind a non-repo directory and leave the user staring at
/// confusing downstream worktree/session failures.
///
/// Errors:
/// * [`BootError::NotARepository`] if `git_toplevel` returns `None`
///   (path is not inside any git working tree, or git is unavailable),
///   or if the discovered toplevel differs from `canon` (the path is
///   *inside* a repo but not the repo root — e.g. the user picked a
///   subdirectory).
/// * [`BootError::NotARepository`] (with the underlying error in
///   `reason`) if `git_toplevel` itself errors.
fn validate_repo_root(canon: &Path, git_runner: &dyn GitRunner) -> Result<(), BootError> {
    match git_runner.git_toplevel(canon) {
        Ok(Some(toplevel)) if toplevel == *canon => Ok(()),
        Ok(Some(toplevel)) => Err(BootError::NotARepository {
            workspace: canon.to_path_buf(),
            reason: format!(
                "path is inside a git working tree but not the repository root \
                 (root is {})",
                toplevel.display()
            ),
        }),
        Ok(None) => Err(BootError::NotARepository {
            workspace: canon.to_path_buf(),
            reason: "directory is not a git repository (no .git found)".to_string(),
        }),
        Err(e) => Err(BootError::NotARepository {
            workspace: canon.to_path_buf(),
            reason: format!("git probe failed: {e}"),
        }),
    }
}

/// The successful result of binding a workspace at boot. The caller
/// builds an `AppContext::with_workspace` from this.
#[derive(Debug)]
pub struct WorkspaceBinding {
    pub workspace_root: PathBuf,
    pub layout: StoreLayout,
    pub store: ConfigStore,
    pub lock: WorkspaceLockGuard,
}

/// Acquire the OS lock for `(branch, workspace)`, run seed-on-first-
/// launch, and open a `ConfigStore` rooted at the workspace dir.
/// Returns [`BootError::Contention`] if another process holds the
/// lock; the caller is expected to surface a native dialog and exit.
///
/// `git_runner` is used to verify the path is a git repository root
/// before any locking or seeding side-effects — boot must reject
/// non-repo paths up front (matching the in-app `workspace_validate`
/// command), otherwise downstream worktree/session flows fail in
/// confusing ways.
pub fn bind_workspace(
    workspace_root: &Path,
    app_data_dir: &Path,
    branch: &str,
    git_runner: &dyn GitRunner,
) -> Result<WorkspaceBinding, BootError> {
    let canon = canonicalise_existing(workspace_root)?;
    validate_repo_root(&canon, git_runner)?;
    let root = StoreRoot::new(app_data_dir, branch);
    let layout = root.for_workspace(canon.clone());

    fs::create_dir_all(layout.workspace_dir())?;

    let lock = match WorkspaceLockGuard::acquire(layout.lock_path()) {
        Ok(g) => g,
        Err(LockError::Contention) => {
            return Err(BootError::Contention {
                branch: branch.to_string(),
                workspace: canon,
            });
        }
        Err(LockError::Io(source)) => {
            return Err(BootError::Lock {
                workspace: canon,
                source,
            });
        }
    };

    initialise_workspace_dir(&layout)?;

    let store =
        ConfigStore::from_layout(layout.clone()).map_err(|source| BootError::ConfigStore {
            dir: layout.workspace_dir().to_path_buf(),
            source,
        })?;

    Ok(WorkspaceBinding {
        workspace_root: canon,
        layout,
        store,
        lock,
    })
}

/// Convert a successful binding into a [`WorkspaceScope`] suitable for
/// `AppContext::with_workspace(...)`.
#[must_use]
pub fn into_scope(binding: WorkspaceBinding) -> WorkspaceScope {
    WorkspaceScope::new(Some(binding.workspace_root), binding.store, binding.lock)
}

/// Persist `workspace_root` into the bound store's `config.json` if it
/// is not already present (or differs from the canonical path).
///
/// Used by both the boot orchestrator and the in-app
/// `workspace_switch` command (Phase 7) — without this, a freshly-
/// seeded or brand-new workspace would have `workspace_root: None`
/// and the React frontend's picker UI would fire on top of an
/// already-bound workspace. Failures are logged but non-fatal: a
/// transient IO error here just means the user re-picks on next
/// launch.
pub fn ensure_workspace_root_in_config(store: &ConfigStore, canonical: &Path) {
    let cfg = store.load_config();
    if cfg.workspace_root.as_deref() == Some(canonical) {
        return;
    }
    let patch = crate::types::PartialAppConfig {
        workspace_root: Some(Some(canonical.to_path_buf())),
        ..Default::default()
    };
    if let Err(e) = store.save_config(patch) {
        warn!(error = ?e, "failed to persist bound workspace_root into config.json; non-fatal");
    }
}

/// Native folder-picker dialog. Returns the user's chosen path, or
/// `None` if they cancelled. Synchronous — blocks the calling thread.
#[must_use]
pub fn prompt_for_workspace_native(branch: &str) -> Option<PathBuf> {
    let title = if is_canonical_build(branch) {
        "Pick the Arborist workspace folder".to_string()
    } else {
        format!("Pick the Arborist workspace folder (branch: {branch})")
    };
    let dialog = rfd::FileDialog::new().set_title(&title);
    dialog.pick_folder()
}

/// Native message-dialog informing the user that the requested
/// workspace is already open in another Arborist window.
pub fn show_lock_contention_dialog(branch: &str, workspace: &Path) {
    let body = format!(
        "Arborist cannot open this workspace because another Arborist window is already using it for the same branch.\n\nBranch: {}\nWorkspace: {}\n\nClose the other window and try again.",
        if branch.trim().is_empty() { "main" } else { branch },
        workspace.display(),
    );
    let _ = rfd::MessageDialog::new()
        .set_title("Workspace already open")
        .set_description(&body)
        .set_level(rfd::MessageLevel::Error)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
    info!(branch = branch, ?workspace, "boot refused: lock contention");
}

/// Native message-dialog informing the user that the requested path
/// isn't a git repository root and so cannot be bound as a workspace.
/// Used when the source of the path was the native picker (the user
/// explicitly chose it) — for `--workspace` / hint / legacy sources
/// the boot orchestrator surfaces the error via stderr / log instead.
pub fn show_not_a_repo_dialog(workspace: &Path, reason: &str) {
    let body = format!(
        "Arborist could not open this folder as a workspace because it is not a git repository root.\n\nWorkspace: {}\n\nReason: {reason}\n\nPick a folder that contains a `.git` directory at its top level and try again.",
        workspace.display(),
    );
    let _ = rfd::MessageDialog::new()
        .set_title("Not a git repository")
        .set_description(&body)
        .set_level(rfd::MessageLevel::Error)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
    info!(?workspace, reason, "boot refused: not a git repository");
}

/// Boot-time workspace resolution + binding orchestration. This is the
/// single entry point `lib::run()`'s setup hook calls.
///
/// Behaviour:
/// * If a workspace can be resolved (CLI/hint/legacy/picker) and bound,
///   returns `Ok(Some(WorkspaceBinding))`. Caller writes the hint and
///   builds the AppContext.
/// * If the user cancels the picker, returns `Ok(None)`. Caller exits
///   cleanly (no error).
/// * On lock contention or hard error, returns `Err(BootError)`.
///   Caller surfaces a native dialog (for [`BootError::Contention`])
///   and exits non-zero.
pub fn boot_select_workspace(
    args: &CliArgs,
    app_data_dir: &Path,
    branch: &str,
    git_runner: &dyn GitRunner,
) -> Result<Option<WorkspaceBinding>, BootError> {
    let resolved = match resolve_boot_workspace(args, app_data_dir, branch, git_runner)? {
        Some(p) => Some(p),
        None => prompt_for_workspace_native(branch),
    };
    let Some(workspace_root) = resolved else {
        return Ok(None); // user cancelled
    };
    let binding = bind_workspace(&workspace_root, app_data_dir, branch, git_runner)?;

    // Ensure the bound workspace's config.json reflects the canonical
    // workspace_root (single source of truth — see helper docs).
    ensure_workspace_root_in_config(&binding.store, &binding.workspace_root);

    if let Err(e) = write_hint(app_data_dir, branch, &binding.workspace_root) {
        warn!(error = %e, "failed to persist last-workspace hint; non-fatal");
    }
    Ok(Some(binding))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GitRunner;
    use crate::types::{Error, WorktreeInfo};
    use tempfile::TempDir;

    /// Test fixture: pretends every queried path *is* a git repository
    /// root (returns the canonicalised path itself as `git_toplevel`).
    /// Used everywhere boot tests don't care about the repo-root check.
    #[derive(Default)]
    struct YesRunner;

    impl GitRunner for YesRunner {
        fn list_worktrees(&self, _: &Path) -> Result<Vec<WorktreeInfo>, Error> {
            Ok(vec![])
        }
        fn git_toplevel(&self, path: &Path) -> Result<Option<PathBuf>, Error> {
            Ok(Some(
                dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
            ))
        }
        fn create_worktree(&self, _: &Path, _: &Path, _: &str) -> Result<PathBuf, Error> {
            unimplemented!("not used in boot tests")
        }
        fn remove_worktree(&self, _: &Path, _: &Path) -> Result<(), Error> {
            unimplemented!("not used in boot tests")
        }
    }

    /// Test fixture: pretends nothing is a git repository. Used for
    /// the new "non-repo path" boot rejection tests.
    #[derive(Default)]
    struct NoRunner;

    impl GitRunner for NoRunner {
        fn list_worktrees(&self, _: &Path) -> Result<Vec<WorktreeInfo>, Error> {
            Ok(vec![])
        }
        fn git_toplevel(&self, _: &Path) -> Result<Option<PathBuf>, Error> {
            Ok(None)
        }
        fn create_worktree(&self, _: &Path, _: &Path, _: &str) -> Result<PathBuf, Error> {
            unimplemented!("not used in boot tests")
        }
        fn remove_worktree(&self, _: &Path, _: &Path) -> Result<(), Error> {
            unimplemented!("not used in boot tests")
        }
    }

    // ----- parse_cli_args ---------------------------------------------

    #[test]
    fn parse_cli_args_no_args_returns_default() {
        let args = parse_cli_args(["arborist"]).unwrap();
        assert_eq!(args, CliArgs::default());
    }

    #[test]
    fn parse_cli_args_space_separated() {
        let args = parse_cli_args(["arborist", "--workspace", "/path/to/ws"]).unwrap();
        assert_eq!(args.workspace.as_deref(), Some(Path::new("/path/to/ws")));
    }

    #[test]
    fn parse_cli_args_equals_form() {
        let args = parse_cli_args(["arborist", "--workspace=/path/to/ws"]).unwrap();
        assert_eq!(args.workspace.as_deref(), Some(Path::new("/path/to/ws")));
    }

    #[test]
    fn parse_cli_args_missing_value_errors() {
        let err = parse_cli_args(["arborist", "--workspace"]).unwrap_err();
        match err {
            BootError::Cli(_) => {}
            _ => panic!("expected Cli error"),
        }
    }

    #[test]
    fn parse_cli_args_flag_like_value_errors() {
        let err = parse_cli_args(["arborist", "--workspace", "--other-flag"]).unwrap_err();
        match err {
            BootError::Cli(_) => {}
            _ => panic!("expected Cli error"),
        }
    }

    #[test]
    fn parse_cli_args_empty_equals_value_errors() {
        let err = parse_cli_args(["arborist", "--workspace="]).unwrap_err();
        match err {
            BootError::Cli(_) => {}
            _ => panic!("expected Cli error"),
        }
    }

    #[test]
    fn parse_cli_args_duplicate_errors() {
        let err = parse_cli_args(["arborist", "--workspace", "/a", "--workspace=/b"]).unwrap_err();
        match err {
            BootError::Cli(_) => {}
            _ => panic!("expected Cli error"),
        }
    }

    #[test]
    fn parse_cli_args_ignores_unknown_flags() {
        let args = parse_cli_args(["arborist", "--unknown", "value", "--workspace=/ws"]).unwrap();
        assert_eq!(args.workspace.as_deref(), Some(Path::new("/ws")));
    }

    // ----- hint_file_path ----------------------------------------------

    #[test]
    fn hint_file_path_canonical_is_top_level() {
        let p = hint_file_path(Path::new("/data"), "");
        assert_eq!(p, Path::new("/data/last-workspace.json"));
        let p = hint_file_path(Path::new("/data"), "main");
        assert_eq!(p, Path::new("/data/last-workspace.json"));
    }

    #[test]
    fn hint_file_path_branch_is_nested() {
        let p = hint_file_path(Path::new("/data"), "feature/x");
        assert_eq!(p, Path::new("/data/branches/feature/x/last-workspace.json"));
    }

    // ----- write_hint / read_hint -------------------------------------

    #[test]
    fn write_then_read_hint_roundtrip() {
        let td = TempDir::new().unwrap();
        let ws = td.path().join("workspace-a");
        write_hint(td.path(), "main", &ws).unwrap();
        let read = read_hint(td.path(), "main");
        assert_eq!(read.as_deref(), Some(ws.as_path()));
    }

    #[test]
    fn write_hint_creates_branch_subdir() {
        let td = TempDir::new().unwrap();
        let ws = td.path().join("ws");
        write_hint(td.path(), "feature/y", &ws).unwrap();
        assert!(td
            .path()
            .join("branches")
            .join("feature/y")
            .join("last-workspace.json")
            .exists());
    }

    #[test]
    fn read_hint_missing_returns_none() {
        let td = TempDir::new().unwrap();
        assert!(read_hint(td.path(), "main").is_none());
    }

    #[test]
    fn read_hint_malformed_returns_none() {
        let td = TempDir::new().unwrap();
        std::fs::write(td.path().join("last-workspace.json"), b"###bogus###").unwrap();
        assert!(read_hint(td.path(), "main").is_none());
    }

    // ----- read_legacy_workspace_root ---------------------------------

    #[test]
    fn read_legacy_returns_none_when_missing() {
        let td = TempDir::new().unwrap();
        assert!(read_legacy_workspace_root(td.path()).is_none());
    }

    #[test]
    fn read_legacy_extracts_workspace_root() {
        let td = TempDir::new().unwrap();
        std::fs::write(
            td.path().join("config.json"),
            r#"{"workspaceRoot":"/some/path"}"#,
        )
        .unwrap();
        assert_eq!(
            read_legacy_workspace_root(td.path()).as_deref(),
            Some(Path::new("/some/path"))
        );
    }

    #[test]
    fn read_legacy_returns_none_when_absent_field() {
        let td = TempDir::new().unwrap();
        std::fs::write(td.path().join("config.json"), r#"{"otherKey":42}"#).unwrap();
        assert!(read_legacy_workspace_root(td.path()).is_none());
    }

    // ----- resolve_boot_workspace -------------------------------------

    #[test]
    fn resolve_prefers_cli_over_hint_and_legacy() {
        let td = TempDir::new().unwrap();
        let ws_cli = td.path().join("ws-cli");
        let ws_hint = td.path().join("ws-hint");
        let ws_legacy = td.path().join("ws-legacy");
        for w in [&ws_cli, &ws_hint, &ws_legacy] {
            std::fs::create_dir_all(w).unwrap();
        }
        write_hint(td.path(), "main", &ws_hint).unwrap();
        std::fs::write(
            td.path().join("config.json"),
            format!(
                r#"{{"workspaceRoot":{:?}}}"#,
                ws_legacy.to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();
        let args = CliArgs {
            workspace: Some(ws_cli.clone()),
        };
        let resolved = resolve_boot_workspace(&args, td.path(), "main", &YesRunner).unwrap();
        assert_eq!(resolved, Some(dunce::canonicalize(&ws_cli).unwrap()));
    }

    #[test]
    fn resolve_falls_through_to_hint_when_no_cli() {
        let td = TempDir::new().unwrap();
        let ws = td.path().join("ws-hint");
        std::fs::create_dir_all(&ws).unwrap();
        write_hint(td.path(), "main", &ws).unwrap();
        let resolved =
            resolve_boot_workspace(&CliArgs::default(), td.path(), "main", &YesRunner).unwrap();
        assert_eq!(resolved, Some(dunce::canonicalize(&ws).unwrap()));
    }

    #[test]
    fn resolve_falls_through_to_legacy_when_no_hint() {
        let td = TempDir::new().unwrap();
        let ws = td.path().join("ws-legacy");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            td.path().join("config.json"),
            format!(
                r#"{{"workspaceRoot":{:?}}}"#,
                ws.to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();
        let resolved =
            resolve_boot_workspace(&CliArgs::default(), td.path(), "main", &YesRunner).unwrap();
        assert_eq!(resolved, Some(dunce::canonicalize(&ws).unwrap()));
    }

    #[test]
    fn resolve_returns_none_when_nothing_resolves() {
        let td = TempDir::new().unwrap();
        let resolved =
            resolve_boot_workspace(&CliArgs::default(), td.path(), "main", &YesRunner).unwrap();
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolve_skips_hint_when_target_missing() {
        let td = TempDir::new().unwrap();
        write_hint(td.path(), "main", &td.path().join("missing-ws")).unwrap();
        let resolved =
            resolve_boot_workspace(&CliArgs::default(), td.path(), "main", &YesRunner).unwrap();
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolve_cli_with_missing_path_errors() {
        let td = TempDir::new().unwrap();
        let args = CliArgs {
            workspace: Some(td.path().join("does-not-exist")),
        };
        let err = resolve_boot_workspace(&args, td.path(), "main", &YesRunner).unwrap_err();
        match err {
            BootError::Canonicalise { .. } | BootError::InvalidWorkspace(_) => {}
            other => panic!("expected Canonicalise/Invalid, got {other:?}"),
        }
    }

    #[test]
    fn resolve_cli_with_non_repo_path_errors() {
        // CLI is hard-fail: a non-repo `--workspace` value must surface
        // up to the caller (lib.rs exits with a clear message),
        // matching the existing missing-dir behaviour.
        let td = TempDir::new().unwrap();
        let ws = td.path().join("not-a-repo");
        std::fs::create_dir_all(&ws).unwrap();
        let args = CliArgs {
            workspace: Some(ws.clone()),
        };
        let err = resolve_boot_workspace(&args, td.path(), "main", &NoRunner).unwrap_err();
        match err {
            BootError::NotARepository { workspace, .. } => {
                assert_eq!(workspace, dunce::canonicalize(&ws).unwrap());
            }
            other => panic!("expected NotARepository, got {other:?}"),
        }
    }

    #[test]
    fn resolve_falls_through_when_hint_not_a_repo() {
        // Hint pointing at a still-existing-but-no-longer-a-repo folder
        // must silently fall through to the next step (legacy → None →
        // picker), matching the existing missing-dir fall-through. It
        // must NOT hard-fail boot.
        let td = TempDir::new().unwrap();
        let ws = td.path().join("hint-but-no-repo");
        std::fs::create_dir_all(&ws).unwrap();
        write_hint(td.path(), "main", &ws).unwrap();

        let resolved =
            resolve_boot_workspace(&CliArgs::default(), td.path(), "main", &NoRunner).unwrap();
        assert_eq!(
            resolved, None,
            "non-repo hint must silently fall through, not hard-fail"
        );
    }

    #[test]
    fn resolve_falls_through_when_legacy_not_a_repo() {
        let td = TempDir::new().unwrap();
        let ws = td.path().join("legacy-but-no-repo");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            td.path().join("config.json"),
            format!(
                r#"{{"workspaceRoot":{:?}}}"#,
                ws.to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();

        let resolved =
            resolve_boot_workspace(&CliArgs::default(), td.path(), "main", &NoRunner).unwrap();
        assert_eq!(
            resolved, None,
            "non-repo legacy workspace_root must silently fall through, not hard-fail"
        );
    }

    // ----- bind_workspace ---------------------------------------------

    #[test]
    fn bind_workspace_happy_path_locks_and_seeds() {
        let td = TempDir::new().unwrap();
        let app_data = td.path().join("app-data");
        std::fs::create_dir_all(&app_data).unwrap();
        let ws = td.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();

        let binding = bind_workspace(&ws, &app_data, "main", &YesRunner).unwrap();
        assert_eq!(binding.workspace_root, dunce::canonicalize(&ws).unwrap());
        // Seeded marker should exist.
        assert!(binding.layout.workspace_meta_path().exists());
        // Lock file should exist.
        assert!(binding.layout.lock_path().exists());
    }

    #[test]
    fn bind_workspace_second_attempt_contends() {
        let td = TempDir::new().unwrap();
        let app_data = td.path().join("app-data");
        std::fs::create_dir_all(&app_data).unwrap();
        let ws = td.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();

        let _b1 = bind_workspace(&ws, &app_data, "main", &YesRunner).unwrap();

        // Same-process re-acquire is only a reliable contention signal
        // on Windows (per phase 2 findings); cross-process contention is
        // exercised by tests/workspace_lock_multiprocess.rs. Gate the
        // assertion to Windows.
        #[cfg(target_os = "windows")]
        {
            let err = bind_workspace(&ws, &app_data, "main", &YesRunner).unwrap_err();
            match err {
                BootError::Contention { .. } => {}
                other => panic!("expected Contention, got {other:?}"),
            }
        }
    }

    #[test]
    fn bind_workspace_rejects_nonexistent_path() {
        let td = TempDir::new().unwrap();
        let app_data = td.path().join("app-data");
        std::fs::create_dir_all(&app_data).unwrap();
        let err =
            bind_workspace(&td.path().join("missing"), &app_data, "main", &YesRunner).unwrap_err();
        match err {
            BootError::Canonicalise { .. } => {}
            other => panic!("expected Canonicalise, got {other:?}"),
        }
    }

    #[test]
    fn bind_workspace_rejects_non_repo_path() {
        // Regression: previously bind_workspace only checked is_dir(),
        // so any folder selected via --workspace, hint, legacy, or the
        // native picker would happily bind even if it wasn't a git
        // repo, leaving the user staring at confusing downstream
        // worktree/session failures. The fix routes through
        // validate_repo_root before locking/seeding.
        let td = TempDir::new().unwrap();
        let app_data = td.path().join("app-data");
        std::fs::create_dir_all(&app_data).unwrap();
        let ws = td.path().join("not-a-repo");
        std::fs::create_dir_all(&ws).unwrap();

        let err = bind_workspace(&ws, &app_data, "main", &NoRunner).unwrap_err();
        match err {
            BootError::NotARepository { workspace, .. } => {
                assert_eq!(workspace, dunce::canonicalize(&ws).unwrap());
            }
            other => panic!("expected NotARepository, got {other:?}"),
        }

        // No side-effects: lock/seed must NOT have run for a rejected
        // path. (Otherwise we'd leave a stray .lock under app_data_dir
        // for a workspace that was never actually bound.)
        let layout =
            StoreRoot::new(&app_data, "main").for_workspace(dunce::canonicalize(&ws).unwrap());
        assert!(
            !layout.lock_path().exists(),
            "lock file must not be created when bind_workspace rejects the path"
        );
    }

    // ----- boot_select_workspace --------------------------------------

    #[test]
    fn boot_select_via_cli_writes_hint_and_workspace_root() {
        let td = TempDir::new().unwrap();
        let app_data = td.path().join("app-data");
        std::fs::create_dir_all(&app_data).unwrap();
        let ws = td.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();

        let args = CliArgs {
            workspace: Some(ws.clone()),
        };
        let binding = boot_select_workspace(&args, &app_data, "main", &YesRunner)
            .unwrap()
            .expect("should bind, not cancel");

        // Hint file written.
        let hint = read_hint(&app_data, "main").expect("hint persisted");
        assert_eq!(hint, dunce::canonicalize(&ws).unwrap());

        // workspace_root populated in the workspace's own config.json.
        let cfg = binding.store.load_config();
        assert_eq!(
            cfg.workspace_root.as_deref(),
            Some(dunce::canonicalize(&ws).unwrap().as_path())
        );
    }
}
