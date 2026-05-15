//! Boot-time workspace selection (Phase 6 of per-(branch, workspace) settings isolation).
//!
//! ## Why a separate module
//!
//! The new layout (`<app_data_dir>/branches/<branch>/workspaces/<key>/...`) must be activated **before** [`crate::commands::AppContext`] is built —
//! otherwise the legacy top-level `config.json`/`sessions.json` would be used as a fallback, defeating the isolation guarantee. So workspace
//! resolution, OS-lock acquisition, and seed-on-first-launch all happen synchronously in this module from `lib::run()`'s setup hook before the
//! `AppContext` is constructed.
//!
//! ## Resolution priority
//!
//! 1. `--workspace <path>` (or `--workspace=<path>`) CLI argument.
//! 2. The branch-specific `last-workspace.json` hint file written by a previous
//!    successful bind.
//! 3. Legacy `<app_data_dir>/config.json::workspace_root` (one-time migration
//!    breadcrumb so existing single-canonical-install users pick up the new
//!    layout on their first upgraded launch).
//! 4. Native folder picker dialog ([`rfd::FileDialog::pick_folder`]). The user
//!    can cancel — the app then exits cleanly.
//!
//! ## Lock contention
//!
//! When `bind_workspace` cannot acquire the per-(branch, workspace) OS lock (because another Arborist process — same branch, same workspace — already
//! holds it), we surface a synchronous native message dialog ([`rfd::MessageDialog`]) naming the branch + workspace and exit with a non-zero status.
//! The dialog is the user's signal that this isn't an Arborist crash but a deliberate single-writer refusal.
//!
//! ## Why `rfd` for boot-time dialogs
//!
//! Boot-time workspace selection runs before the full Tauri runtime is ready, so
//! we need a standalone synchronous OS-dialog path that does not depend on any
//! WebView/plugin lifecycle. `rfd` gives us that for the first-launch picker and
//! lock/contention error dialogs.

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
    NotARepository {
        workspace: PathBuf,
        reason: String,
        /// Where the path came from in the boot resolution chain. Drives the lib.rs presentation: `Picker` errors trigger a native dialog (the user
        /// *clicked* a folder and deserves visible feedback); `Cli` / `Hint` / `Legacy` errors log to stderr only so non-interactive launches don't
        /// pop a GUI prompt and so `--workspace` failures match the documented stderr-reporting behavior. (`Hint` and `Legacy` are internally demoted
        /// to warnings inside
        /// [`resolve_boot_workspace`] today and so never propagate
        /// out as `NotARepository`, but the variants are kept for completeness in case a future change starts surfacing them.)
        origin: BootSource,
    },
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
    /// `bind_workspace` succeeded (lock + store open) but the follow-on `ensure_workspace_root_in_config` write failed. We must abort boot on this —
    /// see the doc on
    /// [`ensure_workspace_root_in_config`] for the why (the frontend
    /// would otherwise see `workspaceRoot: null` and fall back to the first-boot picker on top of an already-bound backend, with no way to repair the
    /// misalignment).
    #[error("failed to persist bound workspace_root into config.json at {dir}: {source}")]
    WorkspaceRootPersist {
        dir: PathBuf,
        #[source]
        source: crate::types::Error,
    },
}

/// Where in the boot resolution chain a workspace path came from. Threaded through [`validate_repo_root`] / [`bind_workspace`] so
/// [`BootError::NotARepository`] carries enough context for the
/// caller in `lib::run()` to decide between popping a native dialog (picker) and logging to stderr (CLI / hint / legacy / non-interactive launch).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BootSource {
    /// `--workspace <path>` (or `--workspace=<path>`) CLI argument.
    Cli,
    /// The branch-specific `last-workspace.json` hint file.
    Hint,
    /// Legacy `<app_data_dir>/config.json::workspace_root` (one-time migration breadcrumb).
    Legacy,
    /// Native folder-picker dialog ([`prompt_for_workspace_native`]) or any other interactive user-driven path (e.g. the in-app switch command, which
    /// functionally mirrors the picker).
    Picker,
}

/// Parsed CLI arguments. Today: `--workspace <path>` / `--workspace=<path>`, and the test-seam launch overrides
/// `--ai-launch-claude=<cmd>` / `--ai-launch-copilot=<cmd>` (both also accept the space-separated form). Unknown args are ignored (forward-compat).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CliArgs {
    pub workspace: Option<PathBuf>,
    /// Override for the `claude` program token. Seeds `AppConfig.ai_launch_commands.claude` at boot. Used by Linux e2e to point at
    /// `arborist-test-child` without environment variables.
    pub ai_launch_claude: Option<String>,
    /// Sibling of [`Self::ai_launch_claude`] for the `copilot` program token.
    pub ai_launch_copilot: Option<String>,
}

/// Parse `--workspace <path>` / `--workspace=<path>` and `--ai-launch-claude=<cmd>` / `--ai-launch-copilot=<cmd>` from an arbitrary argv (the binary
/// name at index 0 is skipped).
///
/// Errors:
/// * Missing value after `--workspace` (last arg, or next arg starts with
///   `--`).
/// * Duplicate `--workspace` (specified more than once).
/// * Empty value (e.g. `--workspace=`).
/// * Same conditions for the `--ai-launch-*` variants.
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
            None => {
                // Symmetry with the space-separated form below: if the arg *looks* like one of the recognised flags but the value isn't valid UTF-8,
                // surface a clean error rather than silently dropping the flag and falling back to the picker. We use `to_string_lossy` for the
                // prefix probe — `\u{FFFD}` can't appear inside a literal flag name, so the lossy form is safe to inspect.
                let lossy = arg.to_string_lossy();
                for flag in ["--workspace", "--ai-launch-claude", "--ai-launch-copilot"] {
                    if lossy == flag || lossy.starts_with(&format!("{flag}=")) {
                        return Err(BootError::Cli(format!("{flag} value is not valid UTF-8")));
                    }
                }
                continue;
            }
        };
        if let Some(value) = s.strip_prefix("--workspace=") {
            if value.is_empty() {
                return Err(BootError::Cli("--workspace= requires a value".into()));
            }
            if out.workspace.is_some() {
                return Err(BootError::Cli("--workspace specified more than once".into()));
            }
            out.workspace = Some(PathBuf::from(value));
        } else if s == "--workspace" {
            let next = args.next().ok_or_else(|| BootError::Cli("--workspace requires a path argument".into()))?;
            let next_str = next
                .to_str()
                .ok_or_else(|| BootError::Cli("--workspace value is not valid UTF-8".into()))?;
            if next_str.is_empty() || next_str.starts_with("--") {
                return Err(BootError::Cli("--workspace requires a path argument (got flag-like value)".into()));
            }
            if out.workspace.is_some() {
                return Err(BootError::Cli("--workspace specified more than once".into()));
            }
            out.workspace = Some(PathBuf::from(next_str));
        } else if let Some(value) = s.strip_prefix("--ai-launch-claude=") {
            assign_ai_launch(&mut out.ai_launch_claude, "--ai-launch-claude", value)?;
        } else if s == "--ai-launch-claude" {
            let next = args.next().ok_or_else(|| BootError::Cli("--ai-launch-claude requires a value".into()))?;
            let next_str = next
                .to_str()
                .ok_or_else(|| BootError::Cli("--ai-launch-claude value is not valid UTF-8".into()))?;
            if next_str.is_empty() || next_str.starts_with("--") {
                return Err(BootError::Cli("--ai-launch-claude requires a value (got flag-like value)".into()));
            }
            assign_ai_launch(&mut out.ai_launch_claude, "--ai-launch-claude", next_str)?;
        } else if let Some(value) = s.strip_prefix("--ai-launch-copilot=") {
            assign_ai_launch(&mut out.ai_launch_copilot, "--ai-launch-copilot", value)?;
        } else if s == "--ai-launch-copilot" {
            let next = args.next().ok_or_else(|| BootError::Cli("--ai-launch-copilot requires a value".into()))?;
            let next_str = next
                .to_str()
                .ok_or_else(|| BootError::Cli("--ai-launch-copilot value is not valid UTF-8".into()))?;
            if next_str.is_empty() || next_str.starts_with("--") {
                return Err(BootError::Cli("--ai-launch-copilot requires a value (got flag-like value)".into()));
            }
            assign_ai_launch(&mut out.ai_launch_copilot, "--ai-launch-copilot", next_str)?;
        }
        // unknown args ignored
    }
    Ok(out)
}

fn assign_ai_launch(slot: &mut Option<String>, flag: &str, value: &str) -> Result<(), BootError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(BootError::Cli(format!("{flag} requires a non-empty value")));
    }
    if slot.is_some() {
        return Err(BootError::Cli(format!("{flag} specified more than once")));
    }
    *slot = Some(trimmed.to_owned());
    Ok(())
}

/// Where the per-branch hint file lives. Mirrors the storage layout: canonical builds keep it at `<app_data_dir>/last-workspace.json`, branch builds
/// at `<app_data_dir>/branches/<branch>/last-workspace.json`.
#[must_use]
pub fn hint_file_path(app_data_dir: &Path, branch: &str) -> PathBuf {
    if is_canonical_build(branch) {
        app_data_dir.join(HINT_FILE_NAME)
    } else {
        app_data_dir.join("branches").join(branch.trim()).join(HINT_FILE_NAME)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct HintFile {
    #[serde(rename = "workspaceRoot")]
    workspace_root: PathBuf,
}

/// Read the hint file, returning `None` if it doesn't exist, isn't readable, or is malformed. We never propagate hint errors: stale or broken hints
/// fall through to the next resolution step.
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

/// Atomically write the hint file (tempfile + rename). The hint is a single canonicalised absolute path the user most recently bound.
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

/// Best-effort read of `<app_data_dir>/config.json::workspace_root` for the legacy migration breadcrumb. Returns `None` if the legacy file doesn't
/// exist, is unreadable, or has no `workspace_root` set.
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

/// Resolve the workspace to bind, following the priority chain (CLI → hint → legacy → `None`). Returns `None` to mean "fall back to the picker
/// dialog". The CLI arg is hard-failed (returns `Err`) if it points at a non-directory **or a non-git-repo path**; hint/legacy mismatches (missing
/// dir, *or* not a git repository) silently fall through to the next step.
pub fn resolve_boot_workspace(
    args: &CliArgs,
    app_data_dir: &Path,
    branch: &str,
    git_runner: &dyn GitRunner,
) -> Result<Option<(PathBuf, BootSource)>, BootError> {
    if let Some(p) = &args.workspace {
        let canon = canonicalise_existing(p)?;
        validate_repo_root(canon.as_path(), git_runner, BootSource::Cli)?;
        return Ok(Some((canon.into_inner(), BootSource::Cli)));
    }
    if let Some(p) = read_hint(app_data_dir, branch) {
        match canonicalise_existing(&p) {
            Ok(canon) => match validate_repo_root(canon.as_path(), git_runner, BootSource::Hint) {
                Ok(()) => return Ok(Some((canon.into_inner(), BootSource::Hint))),
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
            Ok(canon) => match validate_repo_root(canon.as_path(), git_runner, BootSource::Legacy) {
                Ok(()) => return Ok(Some((canon.into_inner(), BootSource::Legacy))),
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

fn canonicalise_existing(p: &Path) -> Result<crate::store_layout::CanonicalPath, BootError> {
    let canon = crate::store_layout::CanonicalPath::canonicalise(p).map_err(|source| BootError::Canonicalise {
        path: p.to_path_buf(),
        source,
    })?;
    if !canon.as_path().is_dir() {
        return Err(BootError::InvalidWorkspace(canon.into_inner()));
    }
    Ok(canon)
}

/// Verify that `canon` is the root of a git repository (i.e. running `git rev-parse --show-toplevel` from inside it returns `canon` itself) AND that
/// it's a *primary* repo root, not a linked worktree. Mirrors the check that
/// [`crate::commands::workspace_validate_impl`] applies to user-picked
/// paths in the in-app picker, so the boot resolution chain (CLI arg, last-workspace hint, legacy migration breadcrumb, native picker) can't bind a
/// non-repo directory and leave the user staring at confusing downstream worktree/session failures.
///
/// Linked worktrees are explicitly rejected because Arborist's whole model is "spawn child worktrees from a primary repo root" — you cannot create a
/// worktree from inside another worktree (`git worktree add` against a linked worktree's `.git` *file* fails with "not a working tree"). Allowing a
/// worktree root as the workspace would make every session-creation flow break.
///
/// The primary-vs-worktree distinction is encoded on disk: a primary repo has `<root>/.git` as a *directory*; a linked worktree has it as a *file*
/// containing `gitdir: <path-into-primary>`. We check `<canon>/.git.is_dir()` after the toplevel check so both signals must agree (defense in depth —
/// if a future git change ever made `git rev-parse --show-toplevel` behave differently from the primary-only contract we want here, the on-disk check
/// still rejects worktrees).
///
/// Errors:
/// * [`BootError::NotARepository`] if `git_toplevel` returns `None` (path is
///   not inside any git working tree, or git is unavailable), or if the
///   discovered toplevel differs from `canon` (the path is *inside* a repo but
///   not the repo root — e.g. the user picked a subdirectory).
/// * [`BootError::NotARepository`] if `<canon>/.git` is not a directory (the
///   path is a linked worktree root, a submodule working tree, or otherwise
///   non-primary).
/// * [`BootError::NotARepository`] (with the underlying error in `reason`) if
///   `git_toplevel` itself errors.
fn validate_repo_root(canon: &Path, git_runner: &dyn GitRunner, origin: BootSource) -> Result<(), BootError> {
    match git_runner.git_toplevel(canon) {
        Ok(Some(toplevel)) if toplevel == *canon => {
            // Reject linked worktrees / submodule working trees: their `.git` is a file (containing `gitdir: ...`), not a dir. A primary repo has
            // `.git` as a directory at the root.
            let dot_git = canon.join(".git");
            if dot_git.is_dir() {
                Ok(())
            } else {
                Err(BootError::NotARepository {
                    workspace: canon.to_path_buf(),
                    reason: "path is a linked git worktree, not a primary repository root \
                         (Arborist cannot spawn worktrees from inside another worktree; \
                         pick the primary clone instead)"
                        .to_string(),
                    origin,
                })
            }
        }
        Ok(Some(toplevel)) => Err(BootError::NotARepository {
            workspace: canon.to_path_buf(),
            reason: format!(
                "path is inside a git working tree but not the repository root \
                 (root is {})",
                toplevel.display()
            ),
            origin,
        }),
        Ok(None) => Err(BootError::NotARepository {
            workspace: canon.to_path_buf(),
            reason: "directory is not a git repository (no .git found)".to_string(),
            origin,
        }),
        Err(e) => Err(BootError::NotARepository {
            workspace: canon.to_path_buf(),
            reason: format!("git probe failed: {e}"),
            origin,
        }),
    }
}

/// The successful result of binding a workspace at boot. The caller builds an `AppContext::with_workspace` from this.
#[derive(Debug)]
pub struct WorkspaceBinding {
    pub workspace_root: PathBuf,
    pub layout: StoreLayout,
    pub store: ConfigStore,
    pub lock: WorkspaceLockGuard,
}

/// Acquire the OS lock for `(branch, workspace)`, run seed-on-first- launch, and open a `ConfigStore` rooted at the workspace dir. Returns
/// [`BootError::Contention`] if another process holds the lock; the caller is expected to surface a native dialog and exit.
///
/// `git_runner` is used to verify the path is a git repository root before any locking or seeding side-effects — boot must reject non-repo paths up
/// front (matching the in-app `workspace_validate` command), otherwise downstream worktree/session flows fail in confusing ways.
pub fn bind_workspace(
    workspace_root: &Path,
    app_data_dir: &Path,
    branch: &str,
    git_runner: &dyn GitRunner,
    origin: BootSource,
) -> Result<WorkspaceBinding, BootError> {
    let canon = canonicalise_existing(workspace_root)?;
    validate_repo_root(canon.as_path(), git_runner, origin)?;
    let root = StoreRoot::new(app_data_dir, branch);
    let layout = root.for_workspace(&canon);

    fs::create_dir_all(layout.workspace_dir())?;

    let lock = match WorkspaceLockGuard::acquire(layout.lock_path()) {
        Ok(g) => g,
        Err(LockError::Contention) => {
            return Err(BootError::Contention {
                branch: branch.to_string(),
                workspace: canon.into_inner(),
            });
        }
        Err(LockError::Io(source)) => {
            return Err(BootError::Lock {
                workspace: canon.into_inner(),
                source,
            });
        }
    };

    initialise_workspace_dir(&layout)?;

    let store = ConfigStore::from_layout(layout.clone()).map_err(|source| BootError::ConfigStore {
        dir: layout.workspace_dir().to_path_buf(),
        source,
    })?;

    Ok(WorkspaceBinding {
        workspace_root: canon.into_inner(),
        layout,
        store,
        lock,
    })
}

/// Convert a successful binding into a [`WorkspaceScope`] suitable for `AppContext::with_workspace(...)`.
#[must_use]
pub fn into_scope(binding: WorkspaceBinding) -> WorkspaceScope {
    WorkspaceScope::new(Some(binding.workspace_root), binding.store, binding.lock)
}

/// Persist `workspace_root` into the bound store's `config.json` if it is not already present (or differs from the canonical path).
///
/// Used by both the boot orchestrator and the in-app `workspace_switch` command (Phase 7) — without this, a freshly- seeded or brand-new workspace
/// would have `workspace_root: None` and the React frontend's picker UI would fire on top of an already-bound workspace.
///
/// Ensure `store`'s `config.json` records `canonical` as its `workspace_root` (the single source of truth the frontend reads during rehydrate). No-op
/// if the value already matches.
///
/// Returns the underlying [`crate::types::Error`] if the save fails. **Both callers must propagate the error** — neither boot nor the switch command
/// can leave the system in a state where the backend is bound to a workspace but the frontend's `workspaceRoot` is `None`:
///
/// * **Boot path** ([`boot_select_workspace`]) aborts with
///   [`BootError::WorkspaceRootPersist`]. The lock + store binding are dropped
///   on the way out, the user gets a launch failure, and the next launch starts
///   from a clean slate. Tolerating the failure (the previous behaviour) was
///   unsafe: the frontend would read `workspaceRoot: null`, show the first-boot
///   [`crate::commands::session::workspace_validate_impl`] picker, and the
///   picker's `onConfirm` only calls `config_set` on the already-bound store —
///   so the user would believe they picked a new workspace while the backend
///   continued to hold the original one's lock and the new path was written
///   into the wrong store.
/// * **Switch path**
///   ([`crate::commands::session::workspace_switch_impl_inner`]) MUST also
///   propagate. The switch must call this BEFORE the `WorkspaceScope` swap so a
///   failure can abort cleanly with the old workspace still bound (drop the new
///   binding → release the new OS lock).
pub fn ensure_workspace_root_in_config(store: &ConfigStore, canonical: &Path) -> Result<(), crate::types::Error> {
    let cfg = store.load_config();
    if cfg.workspace_root.as_deref() == Some(canonical) {
        return Ok(());
    }
    let patch = crate::types::PartialAppConfig {
        workspace_root: Some(Some(canonical.to_path_buf())),
        ..Default::default()
    };
    store.save_config(patch)?;
    Ok(())
}

/// Native folder-picker dialog. Returns the user's chosen path, or `None` if they cancelled. Synchronous — blocks the calling thread.
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

/// Native message-dialog informing the user that the requested workspace is already open in another Arborist window.
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

/// Variant of [`show_lock_contention_dialog`] used when the boot flow will fall back to the native picker after the user dismisses the dialog.
/// Informs the user that the resolved workspace is locked and they should pick a different one.
fn show_lock_contention_picker_dialog(branch: &str, workspace: &Path) {
    let body = format!(
        "This workspace is already open in another Arborist window for the same branch.\n\nBranch: {}\nWorkspace: {}\n\nPick a different workspace folder to continue.",
        if branch.trim().is_empty() { "main" } else { branch },
        workspace.display(),
    );
    let _ = rfd::MessageDialog::new()
        .set_title("Workspace already open")
        .set_description(&body)
        .set_level(rfd::MessageLevel::Warning)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
    info!(
        branch = branch,
        ?workspace,
        "lock contention on auto-resolved workspace; falling back to picker"
    );
}

/// Native message-dialog informing the user that the requested path isn't a git repository root and so cannot be bound as a workspace. Used when the
/// source of the path was the native picker (the user explicitly chose it) — for `--workspace` / hint / legacy sources the boot orchestrator surfaces
/// the error via stderr / log instead.
pub fn show_not_a_repo_dialog(workspace: &Path, reason: &str) {
    // `validate_repo_root` accepts ONLY primary repository roots (where `.git` is a *directory*). Linked worktrees (where `.git` is a *file*
    // containing `gitdir: ...`) are rejected because Arborist's whole model is "spawn child worktrees from a primary repo root" — a linked worktree
    // cannot host its own worktrees. Steer the user toward the primary clone, and explicitly call out the worktree case so they don't pick a sibling
    // worktree root and wonder why it was rejected.
    let body = format!(
        "Arborist could not open this folder as a workspace because it is not a primary git repository root.\n\nWorkspace: {}\n\nReason: {reason}\n\nPick a folder that contains a `.git` directory at its top level (the primary clone) and try again. Linked git worktrees are not supported as workspaces — Arborist creates per-session worktrees from the primary clone.",
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

/// Native message-dialog informing the user that the chosen workspace was bound but the canonical `workspace_root` couldn't be persisted into the
/// workspace's `config.json`. Boot aborts in that case (see
/// [`ensure_workspace_root_in_config`] doc) — a partial bind would
/// leave the frontend showing the first-boot picker on top of a locked backend with no way to repair the misalignment.
pub fn show_workspace_root_persist_dialog(workspace_dir: &Path, reason: &str) {
    let body = format!(
        "Arborist opened the workspace but could not persist its location into the workspace's config.json. Boot was aborted to avoid a state where the app holds the workspace lock but its UI shows the first-boot picker.\n\nWorkspace config: {}\n\nReason: {reason}\n\nCheck filesystem permissions on the workspace folder and try again.",
        workspace_dir.display(),
    );
    let _ = rfd::MessageDialog::new()
        .set_title("Failed to save workspace location")
        .set_description(&body)
        .set_level(rfd::MessageLevel::Error)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
    info!(?workspace_dir, reason, "boot refused: failed to persist workspace_root into config.json");
}

/// Boot-time workspace resolution + binding orchestration. This is the single entry point `lib::run()`'s setup hook calls.
///
/// Behaviour:
/// * If a workspace can be resolved (CLI/hint/legacy/picker) and bound, returns
///   `Ok(Some(WorkspaceBinding))`. Caller writes the hint and builds the
///   AppContext.
/// * If the user cancels the picker, returns `Ok(None)`. Caller exits cleanly
///   (no error).
/// * On lock contention from a non-CLI source, surfaces a native dialog and
///   falls back to the native picker so the user can choose a different
///   workspace. If the user cancels the picker, returns `Ok(None)`.
/// * On lock contention from a `--workspace` CLI arg, returns
///   `Err(BootError::Contention)`. Caller surfaces a dialog and exits non-zero.
/// * On other hard errors, returns `Err(BootError)`. Caller exits non-zero.
pub fn boot_select_workspace(
    args: &CliArgs,
    app_data_dir: &Path,
    branch: &str,
    git_runner: &dyn GitRunner,
) -> Result<Option<WorkspaceBinding>, BootError> {
    let resolved = match resolve_boot_workspace(args, app_data_dir, branch, git_runner)? {
        Some(pair) => Some(pair),
        None => prompt_for_workspace_native(branch).map(|p| (p, BootSource::Picker)),
    };
    let Some((workspace_root, source)) = resolved else {
        return Ok(None); // user cancelled
    };

    let binding = match bind_workspace(&workspace_root, app_data_dir, branch, git_runner, source) {
        Ok(b) => b,
        Err(BootError::Contention { ref branch, ref workspace }) if source != BootSource::Cli => {
            // The auto-resolved workspace (hint/legacy) or picker selection is locked by another instance. Inform the user and let them pick a
            // different workspace rather than hard-exiting.
            show_lock_contention_picker_dialog(branch, workspace);
            return boot_select_workspace_from_picker(app_data_dir, branch, git_runner);
        }
        Err(e) => return Err(e),
    };

    // Ensure the bound workspace's config.json reflects the canonical workspace_root (single source of truth — see helper docs). Boot must propagate
    // a save failure here: if we let the bind stand with workspace_root=None on disk, the frontend would rehydrate, see `workspaceRoot: null`, fall
    // back to the first- boot picker, and the picker's confirm path (`config_set`) would write to the already-bound store while the backend remained
    // locked on the original workspace. Aborting drops `binding`, which releases the OS lock — caller (`lib::run`) surfaces a dialog and exits
    // non-zero, and the next launch starts clean.
    if let Err(source) = ensure_workspace_root_in_config(&binding.store, &binding.workspace_root) {
        return Err(BootError::WorkspaceRootPersist {
            dir: binding.layout.workspace_dir().to_path_buf(),
            source,
        });
    }

    if let Err(e) = write_hint(app_data_dir, branch, &binding.workspace_root) {
        warn!(error = %e, "failed to persist last-workspace hint; non-fatal");
    }
    Ok(Some(binding))
}

/// Picker-only workspace selection loop. Called when the initial resolution hit lock contention and we need the user to pick a different workspace.
/// Loops on contention (the user might pick the same locked workspace again) and on invalid-repo picks, showing a dialog each time. Returns
/// `Ok(None)` if the user cancels the picker at any point.
fn boot_select_workspace_from_picker(app_data_dir: &Path, branch: &str, git_runner: &dyn GitRunner) -> Result<Option<WorkspaceBinding>, BootError> {
    loop {
        let Some(workspace_root) = prompt_for_workspace_native(branch) else {
            return Ok(None); // user cancelled
        };

        match bind_workspace(&workspace_root, app_data_dir, branch, git_runner, BootSource::Picker) {
            Ok(binding) => {
                if let Err(source) = ensure_workspace_root_in_config(&binding.store, &binding.workspace_root) {
                    return Err(BootError::WorkspaceRootPersist {
                        dir: binding.layout.workspace_dir().to_path_buf(),
                        source,
                    });
                }
                if let Err(e) = write_hint(app_data_dir, branch, &binding.workspace_root) {
                    warn!(error = %e, "failed to persist last-workspace hint; non-fatal");
                }
                return Ok(Some(binding));
            }
            Err(BootError::Contention {
                branch: ref b,
                workspace: ref w,
            }) => {
                show_lock_contention_picker_dialog(b, w);
                // loop again — let user pick another
            }
            Err(BootError::NotARepository {
                ref workspace, ref reason, ..
            }) => {
                show_not_a_repo_dialog(workspace, reason);
                // loop again — let user pick another
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GitRunner;
    use crate::types::{Error, WorktreeInfo};
    use tempfile::TempDir;

    /// Test fixture: pretends every queried path *is* a git repository root (returns the canonicalised path itself as `git_toplevel`). Used
    /// everywhere boot tests don't care about the repo-root check.
    #[derive(Default)]
    struct YesRunner;

    impl GitRunner for YesRunner {
        fn list_worktrees(&self, _: &Path) -> Result<Vec<WorktreeInfo>, Error> {
            Ok(vec![])
        }
        fn git_toplevel(&self, path: &Path) -> Result<Option<PathBuf>, Error> {
            Ok(Some(dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())))
        }
        fn create_worktree(&self, _: &Path, _: &Path, _: &str) -> Result<PathBuf, Error> {
            unimplemented!("not used in boot tests")
        }
        fn remove_worktree(&self, _: &Path, _: &Path) -> Result<(), Error> {
            unimplemented!("not used in boot tests")
        }
        fn git_status(&self, _: &Path) -> Result<crate::types::WorktreeGitStatus, Error> {
            Ok(crate::types::WorktreeGitStatus::default())
        }
    }

    /// Test fixture: pretends nothing is a git repository. Used for the new "non-repo path" boot rejection tests.
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
        fn git_status(&self, _: &Path) -> Result<crate::types::WorktreeGitStatus, Error> {
            Ok(crate::types::WorktreeGitStatus::default())
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

    #[cfg(unix)]
    #[test]
    fn parse_cli_args_non_utf8_workspace_equals_value_errors() {
        // Symmetric with the space-separated form: an invalid-UTF-8 value after `--workspace=` must surface a clean CLI error rather than being
        // silently dropped (which previously caused the boot to fall through to the picker without telling the user their flag was rejected).
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let mut bytes: Vec<u8> = b"--workspace=".to_vec();
        bytes.extend_from_slice(&[0xFF, 0xFE, 0xFD]); // invalid UTF-8
        let arg = OsString::from_vec(bytes);
        let err = parse_cli_args::<_, OsString>([OsString::from("arborist"), arg]).unwrap_err();
        match err {
            BootError::Cli(msg) => assert!(msg.contains("--workspace value is not valid UTF-8")),
            other => panic!("expected BootError::Cli, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn parse_cli_args_non_utf8_bare_workspace_value_errors() {
        // The space-separated form was already strict; this test pins the existing behaviour so a future refactor doesn't regress both forms
        // together.
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let value = OsString::from_vec(vec![0xFF, 0xFE, 0xFD]);
        let err = parse_cli_args::<_, OsString>([OsString::from("arborist"), OsString::from("--workspace"), value]).unwrap_err();
        match err {
            BootError::Cli(msg) => assert!(msg.contains("--workspace value is not valid UTF-8")),
            other => panic!("expected BootError::Cli, got {other:?}"),
        }
    }

    // ----- --ai-launch-claude / --ai-launch-copilot --------------------

    #[test]
    fn parse_cli_args_ai_launch_claude_equals_form() {
        let args = parse_cli_args(["arborist", "--ai-launch-claude=/usr/local/bin/test-child"]).unwrap();
        assert_eq!(args.ai_launch_claude.as_deref(), Some("/usr/local/bin/test-child"));
        assert!(args.ai_launch_copilot.is_none());
    }

    #[test]
    fn parse_cli_args_ai_launch_claude_space_separated() {
        let args = parse_cli_args(["arborist", "--ai-launch-claude", "/usr/local/bin/test-child"]).unwrap();
        assert_eq!(args.ai_launch_claude.as_deref(), Some("/usr/local/bin/test-child"));
    }

    #[test]
    fn parse_cli_args_ai_launch_copilot_equals_form() {
        let args = parse_cli_args(["arborist", "--ai-launch-copilot=/usr/local/bin/test-child"]).unwrap();
        assert_eq!(args.ai_launch_copilot.as_deref(), Some("/usr/local/bin/test-child"));
        assert!(args.ai_launch_claude.is_none());
    }

    #[test]
    fn parse_cli_args_ai_launch_copilot_space_separated() {
        let args = parse_cli_args(["arborist", "--ai-launch-copilot", "/usr/local/bin/test-child"]).unwrap();
        assert_eq!(args.ai_launch_copilot.as_deref(), Some("/usr/local/bin/test-child"));
    }

    #[test]
    fn parse_cli_args_ai_launch_claude_missing_value_errors() {
        let err = parse_cli_args(["arborist", "--ai-launch-claude"]).unwrap_err();
        match err {
            BootError::Cli(msg) => assert!(msg.contains("--ai-launch-claude requires a value")),
            other => panic!("expected BootError::Cli, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_ai_launch_copilot_missing_value_errors() {
        let err = parse_cli_args(["arborist", "--ai-launch-copilot"]).unwrap_err();
        match err {
            BootError::Cli(msg) => assert!(msg.contains("--ai-launch-copilot requires a value")),
            other => panic!("expected BootError::Cli, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_ai_launch_claude_flag_like_value_errors() {
        // Regression: bare `--ai-launch-claude --workspace /ws` must NOT silently
        // assign `--workspace` as the launch command and then fail to parse the
        // workspace path. Mirror the strict behaviour we apply to `--workspace`.
        let err = parse_cli_args(["arborist", "--ai-launch-claude", "--workspace", "/ws"]).unwrap_err();
        match err {
            BootError::Cli(msg) => assert!(msg.contains("--ai-launch-claude") && msg.contains("flag-like")),
            other => panic!("expected BootError::Cli, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_ai_launch_copilot_flag_like_value_errors() {
        let err = parse_cli_args(["arborist", "--ai-launch-copilot", "--workspace", "/ws"]).unwrap_err();
        match err {
            BootError::Cli(msg) => assert!(msg.contains("--ai-launch-copilot") && msg.contains("flag-like")),
            other => panic!("expected BootError::Cli, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_ai_launch_claude_empty_equals_value_errors() {
        let err = parse_cli_args(["arborist", "--ai-launch-claude="]).unwrap_err();
        match err {
            BootError::Cli(msg) => assert!(msg.contains("--ai-launch-claude") && msg.contains("non-empty")),
            other => panic!("expected BootError::Cli, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_ai_launch_copilot_empty_equals_value_errors() {
        let err = parse_cli_args(["arborist", "--ai-launch-copilot="]).unwrap_err();
        match err {
            BootError::Cli(msg) => assert!(msg.contains("--ai-launch-copilot") && msg.contains("non-empty")),
            other => panic!("expected BootError::Cli, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_ai_launch_claude_whitespace_only_value_errors() {
        // `--ai-launch-claude="   "` (whitespace-only, non-empty string) must
        // be rejected — otherwise `compose::cli_program_for_tool` would later
        // trim the override down to empty and silently fall back to the bare
        // `claude` token, making the CLI flag look like it worked.
        let err = parse_cli_args(["arborist", "--ai-launch-claude=   "]).unwrap_err();
        match err {
            BootError::Cli(msg) => assert!(msg.contains("--ai-launch-claude") && msg.contains("non-empty")),
            other => panic!("expected BootError::Cli, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_ai_launch_copilot_whitespace_only_value_errors() {
        let err = parse_cli_args(["arborist", "--ai-launch-copilot=\t \t"]).unwrap_err();
        match err {
            BootError::Cli(msg) => assert!(msg.contains("--ai-launch-copilot") && msg.contains("non-empty")),
            other => panic!("expected BootError::Cli, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_ai_launch_claude_trims_value() {
        // A value with surrounding whitespace is accepted but stored trimmed,
        // so it matches what `compose::cli_program_for_tool` will splice into
        // the composed shell command.
        let args = parse_cli_args(["arborist", "--ai-launch-claude=  /usr/local/bin/test-child  "]).unwrap();
        assert_eq!(args.ai_launch_claude.as_deref(), Some("/usr/local/bin/test-child"));
    }

    #[test]
    fn parse_cli_args_ai_launch_claude_duplicate_errors() {
        let err = parse_cli_args(["arborist", "--ai-launch-claude=/a", "--ai-launch-claude", "/b"]).unwrap_err();
        match err {
            BootError::Cli(msg) => assert!(msg.contains("--ai-launch-claude specified more than once")),
            other => panic!("expected BootError::Cli, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_ai_launch_copilot_duplicate_errors() {
        let err = parse_cli_args(["arborist", "--ai-launch-copilot=/a", "--ai-launch-copilot=/b"]).unwrap_err();
        match err {
            BootError::Cli(msg) => assert!(msg.contains("--ai-launch-copilot specified more than once")),
            other => panic!("expected BootError::Cli, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_ai_launch_both_with_workspace() {
        // Realistic Linux e2e invocation: --workspace alongside both AI launch overrides.
        let args = parse_cli_args([
            "arborist",
            "--workspace",
            "/tmp/ws",
            "--ai-launch-claude=/usr/local/bin/test-child",
            "--ai-launch-copilot=/usr/local/bin/test-child",
        ])
        .unwrap();
        assert_eq!(args.workspace.as_deref(), Some(Path::new("/tmp/ws")));
        assert_eq!(args.ai_launch_claude.as_deref(), Some("/usr/local/bin/test-child"));
        assert_eq!(args.ai_launch_copilot.as_deref(), Some("/usr/local/bin/test-child"));
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
        assert!(td.path().join("branches").join("feature/y").join("last-workspace.json").exists());
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
        std::fs::write(td.path().join("config.json"), r#"{"workspaceRoot":"/some/path"}"#).unwrap();
        assert_eq!(read_legacy_workspace_root(td.path()).as_deref(), Some(Path::new("/some/path")));
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
            // YesRunner says "this is a repo root", but validate_repo_root *also* requires `.git` to be a real directory (linked-worktree rejection).
            // Simulate a primary clone by creating an empty `.git/` next to the workspace.
            std::fs::create_dir_all(w.join(".git")).unwrap();
        }
        write_hint(td.path(), "main", &ws_hint).unwrap();
        std::fs::write(
            td.path().join("config.json"),
            format!(r#"{{"workspaceRoot":{:?}}}"#, ws_legacy.to_string_lossy().replace('\\', "/")),
        )
        .unwrap();
        let args = CliArgs {
            workspace: Some(ws_cli.clone()),
            ..Default::default()
        };
        let resolved = resolve_boot_workspace(&args, td.path(), "main", &YesRunner).unwrap();
        assert_eq!(resolved, Some((dunce::canonicalize(&ws_cli).unwrap(), BootSource::Cli)));
    }

    #[test]
    fn resolve_falls_through_to_hint_when_no_cli() {
        let td = TempDir::new().unwrap();
        let ws = td.path().join("ws-hint");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        write_hint(td.path(), "main", &ws).unwrap();
        let resolved = resolve_boot_workspace(&CliArgs::default(), td.path(), "main", &YesRunner).unwrap();
        assert_eq!(resolved, Some((dunce::canonicalize(&ws).unwrap(), BootSource::Hint)));
    }

    #[test]
    fn resolve_falls_through_to_legacy_when_no_hint() {
        let td = TempDir::new().unwrap();
        let ws = td.path().join("ws-legacy");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::write(
            td.path().join("config.json"),
            format!(r#"{{"workspaceRoot":{:?}}}"#, ws.to_string_lossy().replace('\\', "/")),
        )
        .unwrap();
        let resolved = resolve_boot_workspace(&CliArgs::default(), td.path(), "main", &YesRunner).unwrap();
        assert_eq!(resolved, Some((dunce::canonicalize(&ws).unwrap(), BootSource::Legacy)));
    }

    #[test]
    fn resolve_returns_none_when_nothing_resolves() {
        let td = TempDir::new().unwrap();
        let resolved = resolve_boot_workspace(&CliArgs::default(), td.path(), "main", &YesRunner).unwrap();
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolve_skips_hint_when_target_missing() {
        let td = TempDir::new().unwrap();
        write_hint(td.path(), "main", &td.path().join("missing-ws")).unwrap();
        let resolved = resolve_boot_workspace(&CliArgs::default(), td.path(), "main", &YesRunner).unwrap();
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolve_cli_with_missing_path_errors() {
        let td = TempDir::new().unwrap();
        let args = CliArgs {
            workspace: Some(td.path().join("does-not-exist")),
            ..Default::default()
        };
        let err = resolve_boot_workspace(&args, td.path(), "main", &YesRunner).unwrap_err();
        match err {
            BootError::Canonicalise { .. } | BootError::InvalidWorkspace(_) => {}
            other => panic!("expected Canonicalise/Invalid, got {other:?}"),
        }
    }

    #[test]
    fn resolve_cli_with_non_repo_path_errors() {
        // CLI is hard-fail: a non-repo `--workspace` value must surface up to the caller (lib.rs exits with a clear message), matching the existing
        // missing-dir behaviour.
        let td = TempDir::new().unwrap();
        let ws = td.path().join("not-a-repo");
        std::fs::create_dir_all(&ws).unwrap();
        let args = CliArgs {
            workspace: Some(ws.clone()),
            ..Default::default()
        };
        let err = resolve_boot_workspace(&args, td.path(), "main", &NoRunner).unwrap_err();
        match err {
            BootError::NotARepository { workspace, origin, .. } => {
                assert_eq!(workspace, dunce::canonicalize(&ws).unwrap());
                // CLI-sourced rejections must be tagged so lib.rs can route to stderr/log instead of a native dialog (see boot::BootSource docs).
                assert_eq!(origin, BootSource::Cli);
            }
            other => panic!("expected NotARepository, got {other:?}"),
        }
    }

    #[test]
    fn bind_picker_with_non_repo_path_carries_picker_origin() {
        // Picker-sourced rejections must be tagged BootSource::Picker so the lib.rs caller pops a native dialog (the user is sitting in front of
        // one). This mirrors the CLI test above for the opposite arm of the lib.rs dispatch.
        let td = TempDir::new().unwrap();
        let ws = td.path().join("not-a-repo");
        std::fs::create_dir_all(&ws).unwrap();
        let app_data = td.path().join("app-data");
        let err = bind_workspace(&ws, &app_data, "main", &NoRunner, BootSource::Picker).unwrap_err();
        match err {
            BootError::NotARepository { workspace, origin, .. } => {
                assert_eq!(workspace, dunce::canonicalize(&ws).unwrap());
                assert_eq!(origin, BootSource::Picker);
            }
            other => panic!("expected NotARepository, got {other:?}"),
        }
    }

    #[test]
    fn resolve_falls_through_when_hint_not_a_repo() {
        // Hint pointing at a still-existing-but-no-longer-a-repo folder must silently fall through to the next step (legacy → None → picker),
        // matching the existing missing-dir fall-through. It must NOT hard-fail boot.
        let td = TempDir::new().unwrap();
        let ws = td.path().join("hint-but-no-repo");
        std::fs::create_dir_all(&ws).unwrap();
        write_hint(td.path(), "main", &ws).unwrap();

        let resolved = resolve_boot_workspace(&CliArgs::default(), td.path(), "main", &NoRunner).unwrap();
        assert_eq!(resolved, None, "non-repo hint must silently fall through, not hard-fail");
    }

    #[test]
    fn resolve_falls_through_when_legacy_not_a_repo() {
        let td = TempDir::new().unwrap();
        let ws = td.path().join("legacy-but-no-repo");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            td.path().join("config.json"),
            format!(r#"{{"workspaceRoot":{:?}}}"#, ws.to_string_lossy().replace('\\', "/")),
        )
        .unwrap();

        let resolved = resolve_boot_workspace(&CliArgs::default(), td.path(), "main", &NoRunner).unwrap();
        assert_eq!(resolved, None, "non-repo legacy workspace_root must silently fall through, not hard-fail");
    }

    // ----- bind_workspace ---------------------------------------------

    #[test]
    fn bind_workspace_happy_path_locks_and_seeds() {
        let td = TempDir::new().unwrap();
        let app_data = td.path().join("app-data");
        std::fs::create_dir_all(&app_data).unwrap();
        let ws = td.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(ws.join(".git")).unwrap();

        let binding = bind_workspace(&ws, &app_data, "main", &YesRunner, BootSource::Picker).unwrap();
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
        std::fs::create_dir_all(ws.join(".git")).unwrap();

        let _b1 = bind_workspace(&ws, &app_data, "main", &YesRunner, BootSource::Picker).unwrap();

        // Same-process re-acquire is only a reliable contention signal on Windows (per phase 2 findings); cross-process contention is exercised by
        // tests/workspace_lock_multiprocess.rs. Gate the assertion to Windows.
        #[cfg(target_os = "windows")]
        {
            let err = bind_workspace(&ws, &app_data, "main", &YesRunner, BootSource::Picker).unwrap_err();
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
        let err = bind_workspace(&td.path().join("missing"), &app_data, "main", &YesRunner, BootSource::Picker).unwrap_err();
        match err {
            BootError::Canonicalise { .. } => {}
            other => panic!("expected Canonicalise, got {other:?}"),
        }
    }

    #[test]
    fn bind_workspace_rejects_non_repo_path() {
        // Regression: previously bind_workspace only checked is_dir(), so any folder selected via --workspace, hint, legacy, or the native picker
        // would happily bind even if it wasn't a git repo, leaving the user staring at confusing downstream worktree/session failures. The fix routes
        // through validate_repo_root before locking/seeding.
        let td = TempDir::new().unwrap();
        let app_data = td.path().join("app-data");
        std::fs::create_dir_all(&app_data).unwrap();
        let ws = td.path().join("not-a-repo");
        std::fs::create_dir_all(&ws).unwrap();

        let err = bind_workspace(&ws, &app_data, "main", &NoRunner, BootSource::Picker).unwrap_err();
        match err {
            BootError::NotARepository { workspace, .. } => {
                assert_eq!(workspace, dunce::canonicalize(&ws).unwrap());
            }
            other => panic!("expected NotARepository, got {other:?}"),
        }

        // No side-effects: lock/seed must NOT have run for a rejected path. (Otherwise we'd leave a stray .lock under app_data_dir for a workspace
        // that was never actually bound.)
        let layout = StoreRoot::new(&app_data, "main").for_workspace(&crate::store_layout::CanonicalPath::canonicalise(&ws).unwrap());
        assert!(
            !layout.lock_path().exists(),
            "lock file must not be created when bind_workspace rejects the path"
        );
    }

    #[test]
    fn bind_workspace_rejects_linked_worktree() {
        // Regression: `git rev-parse --show-toplevel` returns the path itself for BOTH primary clones and linked worktrees, so the earlier `toplevel
        // == canon` check accepted worktree roots. But Arborist's session model requires a primary repo (you cannot `git worktree add` from inside
        // another worktree), so `validate_repo_root` now also requires `<canon>/.git` to be a *directory*. A linked worktree has `.git` as a *file*
        // containing `gitdir: <path-into-primary>`. Simulate that here.
        let td = TempDir::new().unwrap();
        let app_data = td.path().join("app-data");
        std::fs::create_dir_all(&app_data).unwrap();
        let ws = td.path().join("linked-worktree");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join(".git"), "gitdir: /some/primary/repo/.git/worktrees/branch\n").unwrap();

        // YesRunner pretends the path IS the toplevel — same signal a real `git rev-parse` would emit inside a linked worktree.
        let err = bind_workspace(&ws, &app_data, "main", &YesRunner, BootSource::Picker).unwrap_err();
        match err {
            BootError::NotARepository { workspace, reason, .. } => {
                assert_eq!(workspace, dunce::canonicalize(&ws).unwrap());
                assert!(
                    reason.contains("linked git worktree"),
                    "reason must explain the worktree-vs-primary distinction; got {reason}"
                );
            }
            other => panic!("expected NotARepository, got {other:?}"),
        }

        // No side-effects: a rejected worktree path must not leave a lock or any seeded state under app_data_dir.
        let layout = StoreRoot::new(&app_data, "main").for_workspace(&crate::store_layout::CanonicalPath::canonicalise(&ws).unwrap());
        assert!(
            !layout.lock_path().exists(),
            "lock file must not be created when bind_workspace rejects a linked worktree"
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
        std::fs::create_dir_all(ws.join(".git")).unwrap();

        let args = CliArgs {
            workspace: Some(ws.clone()),
            ..Default::default()
        };
        let binding = boot_select_workspace(&args, &app_data, "main", &YesRunner)
            .unwrap()
            .expect("should bind, not cancel");

        // Hint file written.
        let hint = read_hint(&app_data, "main").expect("hint persisted");
        assert_eq!(hint, dunce::canonicalize(&ws).unwrap());

        // workspace_root populated in the workspace's own config.json.
        let cfg = binding.store.load_config();
        assert_eq!(cfg.workspace_root.as_deref(), Some(dunce::canonicalize(&ws).unwrap().as_path()));
    }

    /// Regression for round-9 review feedback (PR #32): if `ensure_workspace_root_in_config` fails after `bind_workspace` succeeds, boot must abort
    /// with [`BootError::WorkspaceRootPersist`] instead of warning and continuing. A continued boot would leave the backend bound (lock held, store
    /// open) while the frontend rehydrates, sees `workspaceRoot: null`, and falls back to the first-boot picker on top of an already-bound workspace
    /// — self-contradictory state with no recovery path (the picker's confirm only calls `config_set`, not `workspace_switch`).
    ///
    /// We engineer the save failure by pre-creating the eventual `<workspace_dir>/config.json` path *as a directory*. Seed
    /// (`initialise_workspace_dir`) skips the seeded-config branch because `dest_config.exists()` returns true for directories, `load_config` yields
    /// defaults (read fails non-fatally), and the `save_config` write fails when `tempfile::persist` tries to rename over a directory.
    #[test]
    fn boot_aborts_when_workspace_root_persist_fails() {
        let td = TempDir::new().unwrap();
        let app_data = td.path().join("app-data");
        std::fs::create_dir_all(&app_data).unwrap();
        let ws = td.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(ws.join(".git")).unwrap();

        // Pre-create the storage layout's config.json as a directory so the post-bind save fails.
        let canon_ws = crate::store_layout::CanonicalPath::canonicalise(&ws).unwrap();
        let layout = StoreRoot::new(&app_data, "main").for_workspace(&canon_ws);
        std::fs::create_dir_all(layout.workspace_dir()).unwrap();
        std::fs::create_dir_all(layout.settings_path()).unwrap();

        let args = CliArgs {
            workspace: Some(ws.clone()),
            ..Default::default()
        };
        let err = boot_select_workspace(&args, &app_data, "main", &YesRunner).expect_err("boot must abort on persist failure");

        match err {
            BootError::WorkspaceRootPersist { dir, source: _ } => {
                assert_eq!(dir, layout.workspace_dir());
            }
            other => panic!("expected WorkspaceRootPersist, got {other:?}"),
        }

        // Hint file must NOT have been written — boot aborted before `write_hint`. Otherwise the next launch would silently re-use a workspace whose
        // canonical location was never persisted.
        assert!(
            read_hint(&app_data, "main").is_none(),
            "hint must not be written when boot aborts on persist failure",
        );
    }
}
