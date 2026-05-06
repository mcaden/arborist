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
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tempfile::tempdir;

const LOCKER_PATH: &str = env!("CARGO_BIN_EXE_arborist-test-locker");
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Spawn the locker child and block until it prints `LOCKED` on stdout
/// (proving it has acquired the lock). Returns the running child; the
/// caller is responsible for cleaning it up via `release_child`.
///
/// `BufRead::read_line` is blocking, so an inline read-then-check
/// loop would never honour `READY_TIMEOUT` if the child hangs before
/// printing anything (the read would wait forever). To make the
/// timeout *actually* bound the test's wall time, the read is driven
/// from a dedicated reader thread that forwards each line via an
/// `mpsc` channel; the main loop `recv_timeout`s with the remaining
/// budget. On timeout we kill the child; the reader thread then
/// observes EOF on the broken pipe and exits naturally.
///
/// Per `arborist_test_locker`'s protocol, the locker writes exactly
/// one line (`LOCKED` or `CONTENDED`) and then blocks on stdin until
/// EOF — no further stdout writes happen, so leaving the reader
/// thread parked on `read_line` after we see `LOCKED` is harmless;
/// it terminates when `release_child` drops stdin and the child
/// exits.
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
    let (tx, rx) = mpsc::channel::<Result<String, std::io::Error>>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF: child exited
                Ok(_) => {
                    if tx.send(Ok(line)).is_err() {
                        break; // main dropped the receiver
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        }
    });

    let start = Instant::now();
    loop {
        let elapsed = start.elapsed();
        let remaining = match READY_TIMEOUT.checked_sub(elapsed) {
            Some(d) if !d.is_zero() => d,
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("timed out waiting for locker READY sentinel");
            }
        };
        match rx.recv_timeout(remaining) {
            Ok(Ok(line)) => {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed == "LOCKED" {
                    return child;
                }
                if trimmed == "CONTENDED" {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("locker reported CONTENDED on initial acquire — test bug");
                }
                // Unknown line: keep reading; useful for debug dumps.
            }
            Ok(Err(e)) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("read from locker stdout failed: {e}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("timed out waiting for locker READY sentinel");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("locker exited before producing READY sentinel");
            }
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
