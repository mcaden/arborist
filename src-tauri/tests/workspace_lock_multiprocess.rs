//! Cross-process integration tests for [`arborist_lib::workspace_lock`].
//!
//! These tests exist because Unix `flock(2)` (which `fs2` uses on
//! Linux/macOS) tracks lock ownership per-process, not per-file-handle.
//! That means the in-module `#[test]` for double-acquire only proves
//! the Windows behaviour; on Unix the same-process re-acquire returns
//! success. The boot-time uniqueness guarantee that matters in
//! production is *cross-process*, so we exercise it here by spawning
//! `arborist-test-locker` as a real second process.
//!
//! The locker binary path is provided by Cargo via
//! `env!("CARGO_BIN_EXE_arborist-test-locker")` because both crates
//! live in the same workspace.

use arborist_lib::workspace_lock::{LockError, WorkspaceLockGuard};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::tempdir;

const LOCKER_PATH: &str = env!("CARGO_BIN_EXE_arborist-test-locker");
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Spawn the locker child and block until it prints `LOCKED` on stdout
/// (proving it has acquired the lock). Returns the running child; the
/// caller is responsible for cleaning it up via `release_child`.
///
/// `#[allow(clippy::zombie_processes)]` is justified because every
/// panic site inside this helper kills *and* waits for the child
/// before unwinding, and the success path hands ownership to the
/// caller (which is contractually required to call `release_child`,
/// which waits).
#[allow(clippy::zombie_processes)]
fn spawn_locker_and_wait_ready(lock_path: &std::path::Path) -> Child {
    let mut child = Command::new(LOCKER_PATH)
        .arg(lock_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn arborist-test-locker");

    let stdout = child.stdout.take().expect("child stdout pipe");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();

    let start = Instant::now();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("locker exited before producing READY sentinel");
            }
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed == "LOCKED" {
                    // Reattach stdout so the child can keep writing if it
                    // wants to (it doesn't, but we don't want to drop the
                    // pipe and SIGPIPE the child).
                    child.stdout = Some(reader.into_inner());
                    return child;
                }
                if trimmed == "CONTENDED" {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("locker reported CONTENDED on initial acquire — test bug");
                }
                // Unknown line: keep reading; useful for debug dumps.
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("read from locker stdout failed: {e}");
            }
        }
        if start.elapsed() > READY_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("timed out waiting for locker READY sentinel");
        }
    }
}

/// Cleanly shut down the locker by closing its stdin (which causes the
/// blocking `lines()` loop to terminate, dropping the guard) and
/// waiting for it to exit. Then poll the lock until acquirable so the
/// rest of the test can take it without flakiness.
fn release_child(mut child: Child, lock_path: &std::path::Path) {
    drop(child.stdin.take());
    let status = child.wait().expect("locker wait");
    assert!(
        status.success(),
        "locker exited with non-zero status: {status:?}"
    );

    let start = Instant::now();
    loop {
        match WorkspaceLockGuard::acquire(lock_path) {
            Ok(_g) => return,
            Err(LockError::Contention) if start.elapsed() < Duration::from_secs(2) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("post-release acquire failed: {e:?}"),
        }
    }
}

/// While a separate process holds the lock, our acquire MUST return
/// `Contention`. This is the production-relevant boot-time guarantee
/// that the same-process Windows-only test cannot exercise on Unix.
#[test]
fn cross_process_acquire_returns_contention() {
    let td = tempdir().expect("tempdir");
    let lock = td.path().join("contended.lock");

    let mut child = spawn_locker_and_wait_ready(&lock);

    match WorkspaceLockGuard::acquire(&lock) {
        Err(LockError::Contention) => {}
        Err(other) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("expected Contention from contending acquire, got {other:?}");
        }
        Ok(_) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("contending acquire unexpectedly succeeded");
        }
    }

    release_child(child, &lock);
}

/// After the holder process exits (and the OS releases the handle),
/// a fresh acquire MUST succeed. This proves crash-recovery: a hung
/// or killed Arborist instance does not leave a permanently-bound
/// (branch, workspace) tuple.
#[test]
fn cross_process_acquire_succeeds_after_holder_exits() {
    let td = tempdir().expect("tempdir");
    let lock = td.path().join("recovery.lock");

    let child = spawn_locker_and_wait_ready(&lock);
    release_child(child, &lock);
    // release_child already proved we can acquire it; double-acquire
    // here just sanity-checks idempotence.
    let _g = WorkspaceLockGuard::acquire(&lock).expect("post-release re-acquire");
}
