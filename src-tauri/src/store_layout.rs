//! Storage path resolution for per-(branch, workspace) isolation.
//!
//! Arborist used to write `config.json` and `sessions.json` directly under
//! the OS `app_data_dir`. Because the Tauri bundle identifier is a single
//! value, every build (release host + every `tauri:dev` worktree) resolves
//! the same `app_data_dir`, so concurrent processes silently clobber each
//! other's edits (`ConfigStore`'s in-process `Mutex` only covers a single
//! process — see `config_store.rs:82-94`).
//!
//! This module is the path layer of the fix. It introduces two scoping
//! axes — the *build* (keyed off `BUILD_BRANCH`) and the *workspace* (keyed
//! off the canonicalised workspace root path) — and resolves them to
//! distinct on-disk locations:
//!
//! ```text
//! <app_data_dir>/
//!   config.json                              # legacy (read-fallback only)
//!   sessions.json                            # legacy (read-fallback only)
//!   workspaces/<key>/                        # canonical (main / production builds)
//!     config.json
//!     sessions.json
//!     workspace-meta.json
//!     .lock
//!     .config-seed.lock
//!   branches/<branch>/
//!     last-workspace.json                    # picker default for next launch
//!     workspaces/<key>/
//!       config.json
//!       sessions.json
//!       workspace-meta.json
//!       .lock
//!       .config-seed.lock
//! ```
//!
//! The "canonical" build (empty `BUILD_BRANCH` or the literal `"main"`)
//! collapses the `branches/<branch>/` segment so existing installs see no
//! path change at the branch axis. The same collapse rule is already used
//! by the title bar — see [`crate::window_title_for_branch`].
//!
//! All functions in this module are pure and deterministic: they do not
//! touch the filesystem. Disk I/O (lock acquisition, seeding, atomic
//! writes) lives in the modules that consume these paths.

use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Returns `true` if `branch` represents the canonical (top-level) build.
///
/// Both an empty string (no git info / detached HEAD / shallow clone) and
/// the literal `"main"` collapse to the canonical layout. This mirrors
/// [`crate::window_title_for_branch`] so the title-bar story and the
/// storage-key story stay aligned.
#[must_use]
pub fn is_canonical_build(branch: &str) -> bool {
    let trimmed = branch.trim();
    trimmed.is_empty() || trimmed == "main"
}

/// First 16 hex chars of `SHA-256(canonical_path_bytes)` — the storage
/// key for a workspace. 16 hex chars = 64 bits = collision-free for any
/// realistic number of workspaces.
///
/// **The caller MUST pass an already-canonicalised absolute path.** The
/// hash is computed over the path's platform-native byte representation,
/// so two equivalent paths that aren't byte-identical (different case
/// on Windows, with vs without a trailing separator, symlink vs target,
/// etc.) would otherwise produce different keys and accidentally split
/// a single workspace's state across two storage dirs.
///
/// On Unix we hash the raw `OsStr` bytes (paths are opaque byte
/// sequences and may not be valid UTF-8). On Windows we hash the UTF-16
/// LE code units. We deliberately do **not** route through
/// `to_string_lossy`, which would replace invalid UTF-8 with `U+FFFD`
/// and could collide two genuinely distinct non-UTF-8 paths to the
/// same key.
#[must_use]
pub fn workspace_key(canonical_path: &Path) -> String {
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        hasher.update(canonical_path.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        for wide in canonical_path.as_os_str().encode_wide() {
            hasher.update(wide.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        // Fallback for exotic platforms: lossy string. None of our
        // supported targets reach this branch.
        hasher.update(canonical_path.to_string_lossy().as_bytes());
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for byte in &digest[..8] {
        // Infallible: writing to a String never errors.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Per-build storage root.
///
/// Holds the inputs needed to resolve any path that does **not** depend
/// on a chosen workspace — most importantly the per-branch
/// `last-workspace.json` hint that drives picker defaults at startup,
/// and the legacy top-level paths used as seed sources for upgrading
/// users.
///
/// For paths that depend on a specific workspace, call
/// [`StoreRoot::for_workspace`] to get a [`StoreLayout`].
#[derive(Debug, Clone)]
pub struct StoreRoot {
    app_data_dir: PathBuf,
    branch: String,
}

impl StoreRoot {
    /// Construct a root from the OS `app_data_dir` and the `BUILD_BRANCH`
    /// constant baked in at compile time (see `build.rs`).
    ///
    /// `branch` is normalised by trimming surrounding ASCII whitespace
    /// so every downstream method ([`Self::branch_dir`],
    /// [`Self::last_workspace_hint_path`], etc.) sees the same value
    /// that [`is_canonical_build`] uses for its canonical-vs-branch
    /// decision. Without this, a whitespace-padded branch like
    /// `"  feat  "` would route storage under
    /// `<app_data_dir>/branches/  feat  /` while
    /// [`crate::boot::hint_file_path`] (which trims independently)
    /// would write the picker hint under `<app_data_dir>/branches/feat/`
    /// — silently splitting the binding's state across two trees.
    /// Production `BUILD_BRANCH` values are already space-stripped by
    /// `build.rs::sanitize_branch`, so this trim is defensive in depth
    /// against direct callers (tests, examples, future tooling).
    #[must_use]
    pub fn new(app_data_dir: impl Into<PathBuf>, branch: impl Into<String>) -> Self {
        let mut branch = branch.into();
        let trimmed_len = branch.trim().len();
        if trimmed_len != branch.len() {
            // Avoid an allocation in the common case where no trim is
            // needed; otherwise rebuild from the trimmed slice.
            branch = branch.trim().to_owned();
        }
        Self {
            app_data_dir: app_data_dir.into(),
            branch,
        }
    }

    /// The OS `app_data_dir` this root was built from. Useful for
    /// callers that need to write next to (rather than under) the
    /// branch-scoped subtree.
    #[must_use]
    pub fn app_data_dir(&self) -> &Path {
        &self.app_data_dir
    }

    /// The build branch this root was built from. Empty string means
    /// detached HEAD / no git info — see [`is_canonical_build`].
    #[must_use]
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// `true` for canonical (main / empty-branch) builds.
    #[must_use]
    pub fn is_canonical(&self) -> bool {
        is_canonical_build(&self.branch)
    }

    /// `<app_data_dir>` for canonical builds, otherwise
    /// `<app_data_dir>/branches/<branch>`.
    #[must_use]
    pub fn branch_dir(&self) -> PathBuf {
        if self.is_canonical() {
            self.app_data_dir.clone()
        } else {
            self.app_data_dir.join("branches").join(&self.branch)
        }
    }

    /// Hint file recording the workspace last successfully opened by
    /// this build. Lives at `<branch_dir>/last-workspace.json`. Used as
    /// the picker's default on the next launch when no `--workspace`
    /// CLI arg is provided.
    #[must_use]
    pub fn last_workspace_hint_path(&self) -> PathBuf {
        self.branch_dir().join("last-workspace.json")
    }

    /// Legacy top-level `config.json` location. Used by new builds
    /// **only** as a read-only seed source for upgrading users; new
    /// builds never write here.
    #[must_use]
    pub fn legacy_config_path(&self) -> PathBuf {
        self.app_data_dir.join("config.json")
    }

    /// Legacy top-level `sessions.json` location. Same read-only-seed
    /// rule as [`Self::legacy_config_path`].
    #[must_use]
    pub fn legacy_sessions_path(&self) -> PathBuf {
        self.app_data_dir.join("sessions.json")
    }

    /// Path that a *canonical* (main-build) `ConfigStore` would write
    /// its settings to for `canonical_workspace`. Branch builds use
    /// this during seed-on-first-launch to inherit canonical settings
    /// for the same workspace.
    ///
    /// Returns `None` when this root **is** the canonical root — a
    /// canonical build seeding from itself makes no sense.
    #[must_use]
    pub fn canonical_workspace_settings_path(&self, canonical_workspace: &Path) -> Option<PathBuf> {
        if self.is_canonical() {
            None
        } else {
            Some(
                self.app_data_dir
                    .join("workspaces")
                    .join(workspace_key(canonical_workspace))
                    .join("config.json"),
            )
        }
    }

    /// Promote this root to a full per-workspace [`StoreLayout`].
    /// `canonical_workspace` MUST already be canonicalised — see
    /// [`workspace_key`] for the stability requirement.
    #[must_use]
    pub fn for_workspace(&self, canonical_workspace: impl Into<PathBuf>) -> StoreLayout {
        StoreLayout {
            root: self.clone(),
            workspace: canonical_workspace.into(),
        }
    }
}

/// Per-(branch, workspace) storage paths.
///
/// Constructed via [`StoreRoot::for_workspace`]. Every path returned by
/// this struct is rooted at `<branch_dir>/workspaces/<key>`, so the
/// `ConfigStore` for one (branch, workspace) tuple cannot accidentally
/// touch another's files.
#[derive(Debug, Clone)]
pub struct StoreLayout {
    root: StoreRoot,
    workspace: PathBuf,
}

impl StoreLayout {
    /// The build root this layout was derived from.
    #[must_use]
    pub fn root(&self) -> &StoreRoot {
        &self.root
    }

    /// The canonicalised workspace path this layout is keyed on.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// `<branch_dir>/workspaces/<key>` — the directory that holds every
    /// piece of state belonging to this (branch, workspace) tuple.
    #[must_use]
    pub fn workspace_dir(&self) -> PathBuf {
        self.root
            .branch_dir()
            .join("workspaces")
            .join(workspace_key(&self.workspace))
    }

    /// `<workspace_dir>/config.json` — per-(branch, workspace) settings.
    #[must_use]
    pub fn settings_path(&self) -> PathBuf {
        self.workspace_dir().join("config.json")
    }

    /// `<workspace_dir>/sessions.json` — per-(branch, workspace) sessions.
    #[must_use]
    pub fn sessions_path(&self) -> PathBuf {
        self.workspace_dir().join("sessions.json")
    }

    /// `<workspace_dir>/workspace-meta.json` — sidecar recording the
    /// original (canonicalised) workspace path for diagnostics, since
    /// [`workspace_key`] is one-way.
    #[must_use]
    pub fn workspace_meta_path(&self) -> PathBuf {
        self.workspace_dir().join("workspace-meta.json")
    }

    /// `<workspace_dir>/.lock` — uniqueness lock held for the lifetime
    /// of the running Arborist instance bound to this (branch,
    /// workspace) tuple.
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.workspace_dir().join(".lock")
    }

    /// `<workspace_dir>/.config-seed.lock` — short-lived lock held only
    /// during the seed-on-first-launch step so two same-(branch,
    /// workspace) starts cannot both seed.
    #[must_use]
    pub fn seed_lock_path(&self) -> PathBuf {
        self.workspace_dir().join(".config-seed.lock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root(app_data: &str, branch: &str) -> StoreRoot {
        StoreRoot::new(PathBuf::from(app_data), branch.to_string())
    }

    // ----- is_canonical_build -------------------------------------------

    #[test]
    fn is_canonical_build_collapses_empty_and_main() {
        assert!(is_canonical_build(""));
        assert!(is_canonical_build("main"));
        assert!(is_canonical_build("  "));
        assert!(is_canonical_build("  main  "));
    }

    #[test]
    fn is_canonical_build_rejects_branches() {
        assert!(!is_canonical_build("settings-flush"));
        assert!(!is_canonical_build("feature/x"));
        assert!(!is_canonical_build("Main")); // case-sensitive on purpose
        assert!(!is_canonical_build("main2"));
        assert!(!is_canonical_build("dev"));
    }

    // ----- workspace_key ------------------------------------------------

    #[test]
    fn workspace_key_is_deterministic() {
        let p = PathBuf::from("/repos/arborist");
        assert_eq!(workspace_key(&p), workspace_key(&p));
    }

    #[test]
    fn workspace_key_differs_for_different_paths() {
        let a = workspace_key(&PathBuf::from("/repos/arborist"));
        let b = workspace_key(&PathBuf::from("/repos/other"));
        assert_ne!(a, b);
    }

    #[test]
    fn workspace_key_is_16_lowercase_hex() {
        let key = workspace_key(&PathBuf::from("/some/workspace/path"));
        assert_eq!(key.len(), 16, "key must be exactly 16 hex chars");
        assert!(
            key.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "key must be lowercase hex only: {key}"
        );
    }

    /// On Unix, paths are opaque byte sequences and may not be valid
    /// UTF-8. Two genuinely distinct non-UTF-8 paths must hash to
    /// distinct keys — otherwise both would share storage. Routing
    /// through `to_string_lossy` would collide them via `U+FFFD`
    /// substitution.
    #[cfg(unix)]
    #[test]
    fn workspace_key_does_not_collide_non_utf8_unix_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let a = PathBuf::from(OsStr::from_bytes(b"/tmp/test\xFF"));
        let b = PathBuf::from(OsStr::from_bytes(b"/tmp/test\xFE"));
        assert_ne!(
            workspace_key(&a),
            workspace_key(&b),
            "non-UTF-8 paths that differ in raw bytes must hash distinctly",
        );
    }

    // ----- StoreRoot::branch_dir ----------------------------------------

    #[test]
    fn branch_dir_collapses_for_canonical_builds() {
        assert_eq!(root("/data", "").branch_dir(), PathBuf::from("/data"));
        assert_eq!(root("/data", "main").branch_dir(), PathBuf::from("/data"));
    }

    #[test]
    fn branch_dir_includes_branch_segment_for_branch_builds() {
        assert_eq!(
            root("/data", "settings-flush").branch_dir(),
            PathBuf::from("/data/branches/settings-flush"),
        );
    }

    /// Regression for the trim-on-construction normalisation: a branch
    /// passed with surrounding whitespace MUST resolve to the same
    /// directory tree that `boot::hint_file_path` (which trims
    /// independently) would produce. Without this, the storage layout
    /// and the picker hint would silently split across two trees for
    /// the same logical (branch, workspace) tuple.
    #[test]
    fn branch_dir_trims_whitespace_around_branch() {
        let r = root("/data", "  settings-flush  ");
        assert_eq!(r.branch(), "settings-flush");
        assert_eq!(
            r.branch_dir(),
            PathBuf::from("/data/branches/settings-flush"),
        );
    }

    #[test]
    fn workspace_dir_for_whitespace_branch_matches_trimmed_layout() {
        let ws = PathBuf::from("/repos/x");
        let r1 = root("/data", "feat");
        let r2 = root("/data", "  feat  ");
        assert_eq!(
            r1.for_workspace(ws.clone()).workspace_dir(),
            r2.for_workspace(ws).workspace_dir(),
            "whitespace-padded branch must hash to the same workspace dir",
        );
    }

    // ----- StoreRoot::last_workspace_hint_path --------------------------

    #[test]
    fn last_workspace_hint_path_is_under_branch_dir() {
        assert_eq!(
            root("/data", "main").last_workspace_hint_path(),
            PathBuf::from("/data/last-workspace.json"),
        );
        assert_eq!(
            root("/data", "feat").last_workspace_hint_path(),
            PathBuf::from("/data/branches/feat/last-workspace.json"),
        );
    }

    // ----- Legacy paths -------------------------------------------------

    #[test]
    fn legacy_paths_are_always_at_app_data_dir_root() {
        // Even for branch builds, legacy paths point at the top-level
        // app_data_dir — that's where pre-isolation Arborist wrote.
        let r = root("/data", "feat");
        assert_eq!(r.legacy_config_path(), PathBuf::from("/data/config.json"));
        assert_eq!(
            r.legacy_sessions_path(),
            PathBuf::from("/data/sessions.json")
        );
    }

    // ----- canonical_workspace_settings_path ----------------------------

    #[test]
    fn canonical_workspace_settings_path_is_none_for_canonical_root() {
        let r = root("/data", "main");
        assert_eq!(
            r.canonical_workspace_settings_path(&PathBuf::from("/repos/x")),
            None,
            "canonical build seeding from itself makes no sense",
        );
    }

    #[test]
    fn canonical_workspace_settings_path_points_at_canonical_layout_for_branch() {
        let r = root("/data", "feat");
        let ws = PathBuf::from("/repos/x");
        let key = workspace_key(&ws);
        assert_eq!(
            r.canonical_workspace_settings_path(&ws),
            Some(PathBuf::from(format!("/data/workspaces/{key}/config.json"))),
        );
    }

    // ----- StoreLayout::workspace_dir + leaf paths ----------------------

    #[test]
    fn workspace_dir_for_canonical_omits_branch_segment() {
        let layout = root("/data", "main").for_workspace(PathBuf::from("/repos/x"));
        let key = workspace_key(&PathBuf::from("/repos/x"));
        assert_eq!(
            layout.workspace_dir(),
            PathBuf::from(format!("/data/workspaces/{key}")),
        );
    }

    #[test]
    fn workspace_dir_for_branch_includes_branch_segment() {
        let layout = root("/data", "feat").for_workspace(PathBuf::from("/repos/x"));
        let key = workspace_key(&PathBuf::from("/repos/x"));
        assert_eq!(
            layout.workspace_dir(),
            PathBuf::from(format!("/data/branches/feat/workspaces/{key}")),
        );
    }

    #[test]
    fn leaf_paths_live_under_workspace_dir() {
        let layout = root("/data", "feat").for_workspace(PathBuf::from("/repos/x"));
        let dir = layout.workspace_dir();
        assert_eq!(layout.settings_path(), dir.join("config.json"));
        assert_eq!(layout.sessions_path(), dir.join("sessions.json"));
        assert_eq!(
            layout.workspace_meta_path(),
            dir.join("workspace-meta.json")
        );
        assert_eq!(layout.lock_path(), dir.join(".lock"));
        assert_eq!(layout.seed_lock_path(), dir.join(".config-seed.lock"));
    }

    #[test]
    fn two_layouts_for_different_workspaces_do_not_collide() {
        let r = root("/data", "feat");
        let a = r.for_workspace(PathBuf::from("/repos/x"));
        let b = r.for_workspace(PathBuf::from("/repos/y"));
        assert_ne!(a.workspace_dir(), b.workspace_dir());
        assert_ne!(a.settings_path(), b.settings_path());
        assert_ne!(a.lock_path(), b.lock_path());
    }

    #[test]
    fn two_layouts_for_different_branches_same_workspace_do_not_collide() {
        let ws = PathBuf::from("/repos/x");
        let a = root("/data", "main").for_workspace(ws.clone());
        let b = root("/data", "feat").for_workspace(ws);
        assert_ne!(a.workspace_dir(), b.workspace_dir());
        assert_ne!(a.settings_path(), b.settings_path());
        assert_ne!(a.lock_path(), b.lock_path());
    }
}
