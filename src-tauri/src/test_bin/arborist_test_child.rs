//! `arborist-test-child` — a deterministic, dependency-free child process used by the PTY-pool integration tests.
//!
//! Protocol (line-based, `\n`-terminated; both `\n` and `\r\n` are tolerated because some PTY layers translate newlines):
//!
//! - Prints `ARBORIST-TEST-CHILD READY\n` on startup.
//! - `quit\n`               → exit 0.
//! - `exit N\n`             → exit N (N is a non-negative i32).
//! - `flood K\n`            → write K lines `flood-i\n` as fast as possible
//!   (used by backpressure tests).
//! - `unicode\n`            → write a known multibyte UTF-8 string (used by the
//!   streaming-decoder tests).
//! - any other line `X`     → write `echo: X\n`.
//! - `--spawn-grandchild`   → spawn a long-lived `--hold` child, print its PID,
//!   and then stay alive (used by Windows PTY process-tree kill tests).
//!
//! Cross-platform: pure stdlib, no extra dependencies. The binary is wired into `Cargo.toml` so integration tests in this crate receive its path via
//! `env!("CARGO_BIN_EXE_arborist-test-child")`.
//!
//! Lives in `src/test_bin/` (not `src/bin/`) on purpose: Tauri's CLI does an unconditional `read_dir` of `src/bin/` and appends every file there as a
//! bundle binary, ignoring the matching `[[bin]]`'s `required-features = ["test-helpers"]` filter. Keeping the source outside `src/bin/` is what
//! prevents `tauri build` from trying to copy this helper into the AppImage / .deb / .app bundle. Do not move this file back into `src/bin/`.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == std::ffi::OsStr::new("--hold")) {
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
    if args.get(1).is_some_and(|arg| arg == std::ffi::OsStr::new("--spawn-grandchild")) {
        return spawn_grandchild(args.get(2));
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    if writeln!(out, "ARBORIST-TEST-CHILD READY").is_err() {
        return ExitCode::from(2);
    }
    if out.flush().is_err() {
        return ExitCode::from(2);
    }

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    while let Some(Ok(raw)) = lines.next() {
        // Strip a trailing CR in case the host PTY didn't translate it.
        let line = raw.strip_suffix('\r').unwrap_or(&raw);

        if line == "quit" {
            // Echo a sentinel so tests can confirm the child reached this branch even when ConPTY echoes are noisy.
            let _ = writeln!(out, "ARBORIST-TEST-CHILD QUITTING");
            let _ = out.flush();
            return ExitCode::SUCCESS;
        }
        if let Some(rest) = line.strip_prefix("exit ") {
            let code: i32 = rest.trim().parse().unwrap_or(0);
            // ExitCode only takes u8; clamp into that range so any non-zero code reliably surfaces as "non-zero" to the parent.
            let clamped = code.clamp(0, 255) as u8;
            return ExitCode::from(clamped);
        }
        if let Some(rest) = line.strip_prefix("flood ") {
            let n: u32 = rest.trim().parse().unwrap_or(0);
            for i in 0..n {
                if writeln!(out, "flood-{i}").is_err() {
                    return ExitCode::from(2);
                }
            }
            let _ = out.flush();
            continue;
        }
        if line == "unicode" {
            // 「世界 😀」 — mix of 3-byte and 4-byte UTF-8 scalars so the streaming decoder is exercised even without test-controlled splits.
            if writeln!(out, "u:世界😀").is_err() {
                return ExitCode::from(2);
            }
            let _ = out.flush();
            continue;
        }

        if writeln!(out, "echo: {line}").is_err() {
            return ExitCode::from(2);
        }
        let _ = out.flush();
    }

    ExitCode::SUCCESS
}

fn spawn_grandchild(marker_path: Option<&std::ffi::OsString>) -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(_) => return ExitCode::from(2),
    };
    let child = match std::process::Command::new(exe).arg("--hold").spawn() {
        Ok(child) => child,
        Err(_) => return ExitCode::from(2),
    };
    if let Some(marker_path) = marker_path {
        let marker_path = std::path::PathBuf::from(marker_path);
        if std::fs::write(&marker_path, child.id().to_string()).is_err() {
            return ExitCode::from(2);
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    if writeln!(out, "ARBORIST-TEST-CHILD GRANDCHILD {}", child.id()).is_err() {
        return ExitCode::from(2);
    }
    if out.flush().is_err() {
        return ExitCode::from(2);
    }

    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}
