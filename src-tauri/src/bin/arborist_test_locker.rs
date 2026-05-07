//! `arborist-test-locker` — deterministic helper for cross-process workspace-lock integration tests.
//!
//! Protocol:
//!
//! ```text
//! arborist-test-locker <lock_path>
//! ```
//!
//! Behaviour:
//! 1. Acquire an exclusive lock on `<lock_path>` via
//!    [`WorkspaceLockGuard::acquire`].
//! 2. On success: print `LOCKED\n` to stdout (flushed) and block on stdin until
//!    EOF, then drop the guard and exit 0.
//! 3. On contention: print `CONTENDED\n` to stdout (flushed) and exit 2.
//! 4. On any other error: print `ERROR: <message>\n` to stderr and exit 3.
//!
//! The `LOCKED` sentinel is the test's signal that the lock is held, so the parent test can attempt a contending acquire.

use arborist_lib::workspace_lock::{LockError, WorkspaceLockGuard};
use std::io::{self, BufRead, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(lock_path) = args.next() else {
        let _ = writeln!(io::stderr(), "ERROR: usage: arborist-test-locker <lock_path>");
        return ExitCode::from(3);
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();

    match WorkspaceLockGuard::acquire(&lock_path) {
        Ok(_guard) => {
            if writeln!(out, "LOCKED").is_err() || out.flush().is_err() {
                return ExitCode::from(3);
            }
            // Block until parent closes our stdin, then release the lock by dropping `_guard` at end of scope.
            let stdin = io::stdin();
            stdin.lock().lines().for_each(|_| {});
            ExitCode::SUCCESS
        }
        Err(LockError::Contention) => {
            let _ = writeln!(out, "CONTENDED");
            let _ = out.flush();
            ExitCode::from(2)
        }
        Err(LockError::Io(e)) => {
            let _ = writeln!(io::stderr(), "ERROR: {e}");
            ExitCode::from(3)
        }
    }
}
