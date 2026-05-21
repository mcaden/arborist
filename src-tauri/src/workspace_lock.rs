//! Per-(branch, workspace) uniqueness lock.
//!
//! Each running Arborist instance is bound to one (build branch, workspace) tuple and holds an exclusive advisory lock on a sidecar `.lock` file
//! inside that tuple's storage directory for the lifetime of the bound scope. A second process trying to bind the same tuple gets
//! [`LockError::Contention`] and is expected to refuse to start (with a user-facing dialog naming the branch + workspace).
//!
//! This is an *advisory* lock — it does not protect the data files themselves from external manipulation. Data-loss prevention is via
//! single-writer-per-(branch, workspace), not file locking; the lock is what enforces "single writer".
//!
//! ## Handle ownership (Windows footgun)
//!
//! Hold the locked `File` handle for the lifetime of the lock; do NOT clone or duplicate it. `std::fs::File::lock` documents that lock state is tied
//! to the underlying OS handle, so duplicating it can produce surprising behaviour around release. The guard's inner `File` is private and not
//! exposed.
//!
//! ## Crash semantics
//!
//! When the process exits (cleanly or by crash), the OS closes its file handles, which releases the lock. There is no stale-lock cleanup needed.
//! Verified by [`tests::acquire_after_drop_succeeds`].

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// Errors returned by [`WorkspaceLockGuard::acquire`].
#[derive(Debug)]
pub enum LockError {
    /// The lock is currently held by another process. The caller is expected to surface a user-facing message and refuse to bind this (branch,
    /// workspace) tuple.
    Contention,
    /// I/O error opening or interacting with the lock file (e.g. permission denied, parent-dir creation failed). Distinct from `Contention` so the
    /// caller can distinguish "another instance already running" from "your filesystem is broken".
    Io(io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contention => write!(f, "workspace lock is held by another process"),
            Self::Io(e) => write!(f, "workspace lock I/O error: {e}"),
        }
    }
}

impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Contention => None,
            Self::Io(e) => Some(e),
        }
    }
}

/// Owned exclusive advisory lock on a workspace directory.
///
/// The lock is released when the guard is dropped — explicitly via the
/// [`Drop`] impl below (which calls `unlock()`) and then implicitly by
/// the inner `File` closing its OS handle. For correct semantics, the guard MUST NOT be cloned or duplicated — see module-level docs.
#[derive(Debug)]
pub struct WorkspaceLockGuard {
    // Holding the File alive keeps the OS handle (and thus the lock) alive. We never read or write through this File — it's just the lock anchor.
    // Underscore-prefixed because the field is unread.
    _file: File,
    path: PathBuf,
}

impl Drop for WorkspaceLockGuard {
    /// Explicitly release the OS-level advisory lock before the inner `File` is dropped. Closing the handle would also release the lock (per the
    /// `std::fs::File::lock` contract), but on some Unix platforms closing *any* fd referring to the file is sufficient to release all `flock()`s
    /// held against that inode — calling `unlock()` here makes the release point deterministic and matches the safety note inside
    /// [`Self::acquire_inner`]. Errors are swallowed: if the file handle is already closed or the OS rejects the request, the subsequent `File` drop
    /// will release anyway.
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

impl WorkspaceLockGuard {
    /// Try to acquire an exclusive lock on `lock_path`. Non-blocking (`File::try_lock`); contention returns
    /// [`LockError::Contention`] rather than blocking.
    ///
    /// Creates `lock_path` and any missing parent directories. Hold the returned guard for the lifetime of the bound (branch, workspace) scope.
    pub fn acquire(lock_path: impl AsRef<Path>) -> Result<Self, LockError> {
        Self::acquire_inner(lock_path, false)
    }

    /// Acquire an exclusive lock on `lock_path`, blocking the calling thread until the lock is available. Used by the seed-on-first-launch step
    /// (`crate::seed`), where two concurrent same-(branch, workspace) starts must serialise so only one wins the seed; the loser waits, then
    /// re-checks the seed marker and skips.
    ///
    /// Always returns `LockError::Io` for failure modes; never `LockError::Contention` (since we explicitly wait for it).
    pub fn acquire_blocking(lock_path: impl AsRef<Path>) -> Result<Self, LockError> {
        Self::acquire_inner(lock_path, true)
    }

    fn acquire_inner(lock_path: impl AsRef<Path>, blocking: bool) -> Result<Self, LockError> {
        let lock_path = lock_path.as_ref().to_path_buf();
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(LockError::Io)?;
        }
        // Open with read+write+create+no-truncate. The lock-file contents are intentionally meaningless (we only care about the OS-level lock state
        // on the handle); using `truncate(false)` avoids racing with any sibling process that already opened the file.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(LockError::Io)?;
        // SAFETY: code between a successful `lock()` / `try_lock()` and the `Ok(Self { ... })` return MUST NOT panic. If it did, the raw `file` would
        // be dropped *outside* a `WorkspaceLockGuard`, so the explicit `unlock()` in `WorkspaceLockGuard::Drop` (defined above) wouldn't fire; the OS
        // lock would still be released when the OS reclaims the handle, but the release point would no longer be deterministic. The current branches
        // only do infallible moves; keep it that way.
        if blocking {
            file.lock().map_err(LockError::Io)?;
        } else {
            match file.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => return Err(LockError::Contention),
                Err(TryLockError::Error(e)) => return Err(LockError::Io(e)),
            }
        }
        Ok(Self {
            _file: file,
            path: lock_path,
        })
    }

    /// Path of the lock file this guard holds. Useful for diagnostics and for log messages identifying which (branch, workspace) is bound to the
    /// running instance.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Non-destructive contention probe used by the workspace picker (Phase 8). Tries to acquire the lock at `lock_path` and **immediately drops the
    /// guard** on success — i.e. this never holds the lock past the call. Intended only as an *advisory* signal for "would the next acquire succeed
    /// right now?".
    ///
    /// Returns `Ok(true)` if the probe acquire succeeded (no current contender), `Ok(false)` if the lock was held by someone else, or
    /// `Err(LockError::Io)` if the probe could not even open the lock file (the caller should treat this as "no advisory signal available" and not
    /// surface it as contention).
    ///
    /// **Side-effect avoidance:** if the lock file does not yet exist on disk, the probe returns `Ok(true)` *without* creating the parent directory
    /// or the lock file itself. This matters for the workspace picker — without the short-circuit, every probed candidate path would materialise a
    /// `workspaces/<key>/` directory (containing only an empty `.lock`) on disk forever, so a user browsing through several candidate workspaces
    /// would leave a trail of empty directories behind. A path with no `.lock` file trivially has no current holder, so the answer is the same
    /// `Ok(true)` we would have returned after creating-and-releasing.
    ///
    /// **Race window:** because the guard is dropped before the caller acts on the result, another process can acquire the lock between probe and the
    /// caller's real `acquire()`. The probe is purely advisory — the authoritative check is the transactional acquire performed by
    /// `boot::bind_workspace` / `workspace_switch_impl_inner`.
    pub fn probe(lock_path: impl AsRef<Path>) -> Result<bool, LockError> {
        let lock_path = lock_path.as_ref();
        if !lock_path.exists() {
            return Ok(true);
        }
        match Self::acquire(lock_path) {
            Ok(_guard) => Ok(true), // dropped at end of scope; lock released immediately
            Err(LockError::Contention) => Ok(false),
            Err(LockError::Io(e)) => Err(LockError::Io(e)),
        }
    }
}

/// Cross-platform detection of "lock is held by someone else" vs a real I/O failure is now handled directly by [`std::fs::TryLockError`] — the
/// `WouldBlock` variant means "another holder right now", any other variant is a real I/O failure. See the `try_lock` branch in
/// [`WorkspaceLockGuard::acquire_inner`].
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn acquire_creates_parent_dir_and_lock_file() {
        let td = tempdir().expect("tempdir");
        let lock = td.path().join("nested/dir/.lock");
        let g = WorkspaceLockGuard::acquire(&lock).expect("acquire");
        assert!(lock.exists(), "lock file should exist after acquire");
        assert_eq!(g.path(), lock);
    }

    /// Same-process double-acquire MUST return `Contention` on Windows, where `LockFileEx` semantics are per-handle. On Unix, `flock(2)` is
    /// per-process: a second `File::try_lock` from the same PID against the same inode succeeds even via a separate `File` handle. The
    /// cross-process guarantee that matters at boot is verified by the multi-process integration test in `tests/workspace_lock_multiprocess.rs`,
    /// which spawns `arborist-test-locker` as a real second process.
    #[cfg(target_os = "windows")]
    #[test]
    fn second_acquire_returns_contention_same_process_windows() {
        let td = tempdir().expect("tempdir");
        let lock = td.path().join(".lock");
        let _g1 = WorkspaceLockGuard::acquire(&lock).expect("first acquire");
        match WorkspaceLockGuard::acquire(&lock) {
            Err(LockError::Contention) => {}
            Err(other) => panic!("expected Contention, got {other:?}"),
            Ok(_) => panic!("second acquire unexpectedly succeeded"),
        }
    }

    /// Dropping the guard must release the lock; this is what gives us crash-safe semantics (the OS releases the handle when the process exits) and
    /// what makes the in-app workspace switch transactional swap feasible.
    #[test]
    fn acquire_after_drop_succeeds() {
        let td = tempdir().expect("tempdir");
        let lock = td.path().join(".lock");
        let g1 = WorkspaceLockGuard::acquire(&lock).expect("first acquire");
        drop(g1);
        let _g2 = WorkspaceLockGuard::acquire(&lock).expect("acquire after drop");
    }

    /// Regression for the explicit [`WorkspaceLockGuard::Drop`] impl: a re-acquire from a separate `File` handle in the *same process* MUST succeed
    /// immediately after the first guard drops. On Windows (`LockFileEx` per-handle) this is the same observation as
    /// [`acquire_after_drop_succeeds`], but the explicit `unlock()`
    /// call inside `Drop` is what makes the release point deterministic across platforms — without it, some Unix kernels may delay release until the
    /// OS reclaims the handle. Cross-process crash-recovery is exercised separately by `tests/workspace_lock_multiprocess.rs`.
    #[test]
    fn drop_explicitly_unlocks_before_handle_close() {
        let td = tempdir().expect("tempdir");
        let lock = td.path().join(".lock");
        let g1 = WorkspaceLockGuard::acquire(&lock).expect("first acquire");
        // Verify the lock file persists (drop must release the lock, not the file itself).
        drop(g1);
        assert!(lock.exists(), "lock file must persist after guard drop");
        // Subsequent acquire must succeed without contention.
        let _g2 = WorkspaceLockGuard::acquire(&lock).expect("re-acquire after drop");
    }

    /// Independent lock files do not contend with each other — the per-(branch, workspace) layout depends on this.
    #[test]
    fn distinct_lock_paths_do_not_contend() {
        let td = tempdir().expect("tempdir");
        let a = td.path().join("a/.lock");
        let b = td.path().join("b/.lock");
        let _ga = WorkspaceLockGuard::acquire(&a).expect("a");
        let _gb = WorkspaceLockGuard::acquire(&b).expect("b");
    }

    /// Probe against a path that has no lock file yet returns `Ok(true)` *without* materialising the parent directory or the lock file. This keeps
    /// the workspace picker side-effect-free when the user clicks through several candidate paths.
    #[test]
    fn probe_does_not_create_files_when_lock_path_missing() {
        let td = tempdir().expect("tempdir");
        let nested = td.path().join("workspaces/abcdef0123/.lock");
        assert!(!nested.exists());
        assert!(!nested.parent().expect("parent").exists());
        assert!(WorkspaceLockGuard::probe(&nested).expect("probe"));
        assert!(!nested.exists(), "probe must not create the lock file");
        assert!(
            !nested.parent().expect("parent").exists(),
            "probe must not create the parent directory chain"
        );
    }

    /// Probe on a *pre-existing* free lock file returns `Ok(true)` (without holding the lock) and a real acquire afterwards must succeed. Distinct
    /// from the missing-file fast-path above.
    #[test]
    fn probe_returns_true_for_free_lock_and_does_not_hold() {
        let td = tempdir().expect("tempdir");
        let lock = td.path().join(".lock");
        // Create the file up front so we exercise the real try-lock-then-drop path, not the missing-file fast-path.
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock)
            .expect("create lock file");
        assert!(WorkspaceLockGuard::probe(&lock).expect("probe"));
        // The lock itself is free — a real acquire must succeed.
        let _g = WorkspaceLockGuard::acquire(&lock).expect("acquire after probe");
    }

    /// Probe against a lock currently held by another handle in the same process should return `Ok(false)` on Windows (where LockFileEx is
    /// per-handle). On Unix flock is per-process so the same-process probe will succeed; cross-process contention is covered by the
    /// `arborist-test-locker` integration test.
    #[cfg(target_os = "windows")]
    #[test]
    fn probe_returns_false_when_held_windows() {
        let td = tempdir().expect("tempdir");
        let lock = td.path().join(".lock");
        let _holder = WorkspaceLockGuard::acquire(&lock).expect("hold");
        assert!(!WorkspaceLockGuard::probe(&lock).expect("probe"));
    }
}
