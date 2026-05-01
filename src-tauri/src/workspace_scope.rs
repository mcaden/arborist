//! `WorkspaceScope` — the per-(branch, workspace) binding owned by a
//! running Arborist instance.
//!
//! At boot time (phase 6 — not yet wired) the app resolves the user's
//! chosen workspace, builds a [`StoreLayout`](crate::store_layout) for
//! it, acquires the matching [`WorkspaceLockGuard`](crate::workspace_lock),
//! opens a [`ConfigStore`](crate::config_store) at the layout's
//! workspace dir, and bundles all three into a `WorkspaceScope`. The
//! scope is then stored inside [`AppContext`](crate::commands::AppContext)
//! behind an `Arc<RwLock<…>>` so that the in-app workspace switch (phase
//! 7) can transactionally swap the entire scope — releasing the old
//! lock and adopting the new one — under a write lock without any
//! caller seeing a torn intermediate state.
//!
//! ## Locking model
//!
//! * The OS-level lock guard ([`WorkspaceLockGuard`]) lives on the
//!   scope; dropping the scope releases the OS lock. The lock is held
//!   for the lifetime of the binding, not just per-write — that's
//!   what gives single-writer semantics across processes.
//! * The in-process `RwLock<WorkspaceScope>` is for read/write
//!   coordination *within* the running process: callers acquire a
//!   read lock to clone the [`ConfigStore`] (cheap, returns instantly),
//!   the workspace switch acquires a write lock to perform the
//!   transactional swap.
//!
//! ## Snapshot pattern
//!
//! [`AppContext::store`] returns an owned `ConfigStore` clone after
//! grabbing a brief read lock. This means callers never hold the
//! `RwLock` across an `.await` or any long operation — they take a
//! snapshot, drop the lock, then operate on the snapshot. After a
//! workspace switch the snapshot becomes stale (it points at the
//! prior workspace's dir), but every command handler resolves it
//! fresh, so the staleness window is per-call and harmless.

use std::path::PathBuf;

use crate::config_store::ConfigStore;
use crate::workspace_lock::WorkspaceLockGuard;

/// One running instance's binding to a single (branch, workspace) tuple.
///
/// Holds the open [`ConfigStore`] for that workspace plus the OS-level
/// uniqueness lock for it. The lock guard is `Option`-wrapped so tests
/// can construct a scope without taking a real OS lock; production
/// builds always set it via [`Self::new`].
#[derive(Debug)]
pub struct WorkspaceScope {
    /// Canonicalised workspace root this scope is bound to. `None`
    /// during the brief window between app boot and workspace
    /// selection (phase 6 will eliminate this `None` path for
    /// production code; tests may legitimately leave it `None` when
    /// they only exercise commands that don't read it).
    pub workspace_root: Option<PathBuf>,
    /// Cheap-to-clone [`ConfigStore`] handle for this workspace.
    pub store: ConfigStore,
    /// OS-level advisory lock proving this process is the sole
    /// writer for this (branch, workspace) tuple. Held by `_file`
    /// inside the guard for its lifetime; released on drop.
    /// `None` only in test contexts that opt out via
    /// [`Self::for_test`].
    _lock: Option<WorkspaceLockGuard>,
}

impl WorkspaceScope {
    /// Production constructor: bind a store + workspace path to a
    /// concretely-acquired OS lock.
    #[must_use]
    pub fn new(
        workspace_root: Option<PathBuf>,
        store: ConfigStore,
        lock: WorkspaceLockGuard,
    ) -> Self {
        Self {
            workspace_root,
            store,
            _lock: Some(lock),
        }
    }

    /// Test-only constructor that omits the OS lock. Suitable for
    /// integration tests that don't exercise cross-process
    /// uniqueness; production code must use [`Self::new`].
    #[doc(hidden)]
    #[must_use]
    pub fn for_test(store: ConfigStore, workspace_root: Option<PathBuf>) -> Self {
        Self {
            workspace_root,
            store,
            _lock: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn for_test_omits_lock() {
        let td = TempDir::new().unwrap();
        let store = ConfigStore::open(td.path()).unwrap();
        let scope = WorkspaceScope::for_test(store, Some(td.path().to_path_buf()));
        assert!(scope._lock.is_none());
        assert_eq!(scope.workspace_root.as_deref(), Some(td.path()));
    }

    /// Verifies that the OS lock held by a [`WorkspaceScope::new`] is
    /// released exactly when the scope is dropped, by attempting to
    /// re-acquire from the same process.
    ///
    /// **Windows-only** because Unix `flock(2)` is per-process, not
    /// per-fd: a same-process re-acquire on Linux/macOS may succeed
    /// even while another fd in the same process holds the lock. The
    /// cross-process equivalent of this test lives in
    /// `tests/workspace_lock_multiprocess.rs` (phase 2) and exercises
    /// the same drop-releases-lock behaviour against the lower-level
    /// [`WorkspaceLockGuard`]. The same `Drop` impl is in play here, so
    /// the multi-process test is sufficient cross-platform coverage.
    #[cfg(target_os = "windows")]
    #[test]
    fn new_holds_lock_for_lifetime_windows() {
        let td = TempDir::new().unwrap();
        let store = ConfigStore::open(td.path()).unwrap();
        let lock = WorkspaceLockGuard::acquire(td.path().join(".lock")).unwrap();
        let lock_path = lock.path().to_path_buf();
        let scope = WorkspaceScope::new(None, store, lock);

        // While the scope is alive, a non-blocking acquire on the same
        // path must contend.
        let err = WorkspaceLockGuard::acquire(&lock_path).unwrap_err();
        match err {
            crate::workspace_lock::LockError::Contention => {}
            other => panic!("expected Contention while scope alive, got {other:?}"),
        }

        drop(scope);

        // Once the scope is dropped, the next acquire succeeds.
        let _g = WorkspaceLockGuard::acquire(&lock_path).expect("acquire after drop");
    }

    /// Cross-platform smoke test: a freshly-constructed scope holds an
    /// `Option<WorkspaceLockGuard>` populated (the production guarantee
    /// that drives single-writer semantics). The actual contention
    /// behaviour is covered by the multi-process integration test for
    /// `WorkspaceLockGuard` itself.
    #[test]
    fn new_populates_lock_field() {
        let td = TempDir::new().unwrap();
        let store = ConfigStore::open(td.path()).unwrap();
        let lock = WorkspaceLockGuard::acquire(td.path().join(".lock")).unwrap();
        let scope = WorkspaceScope::new(Some(td.path().to_path_buf()), store, lock);
        assert!(scope._lock.is_some());
        assert_eq!(scope.workspace_root.as_deref(), Some(td.path()));
    }
}
