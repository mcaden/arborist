//! `grove-test-child` — a deterministic, dependency-free child process used by
//! the PTY-pool integration tests.
//!
//! Protocol (line-based, `\n`-terminated; both `\n` and `\r\n` are tolerated
//! because some PTY layers translate newlines):
//!
//! - Prints `GROVE-TEST-CHILD READY\n` on startup.
//! - `quit\n`               → exit 0.
//! - `exit N\n`             → exit N (N is a non-negative i32).
//! - `flood K\n`            → write K lines `flood-i\n` as fast as possible
//!   (used by backpressure tests).
//! - `unicode\n`            → write a known multibyte UTF-8 string (used by
//!   the streaming-decoder tests).
//! - any other line `X`     → write `echo: X\n`.
//!
//! Cross-platform: pure stdlib, no extra dependencies. The binary is wired
//! into `Cargo.toml` so integration tests in this crate receive its path via
//! `env!("CARGO_BIN_EXE_grove-test-child")`.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if writeln!(out, "GROVE-TEST-CHILD READY").is_err() {
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
            // Echo a sentinel so tests can confirm the child reached this
            // branch even when ConPTY echoes are noisy.
            let _ = writeln!(out, "GROVE-TEST-CHILD QUITTING");
            let _ = out.flush();
            return ExitCode::SUCCESS;
        }
        if let Some(rest) = line.strip_prefix("exit ") {
            let code: i32 = rest.trim().parse().unwrap_or(0);
            // ExitCode only takes u8; clamp into that range so any non-zero
            // code reliably surfaces as "non-zero" to the parent.
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
            // 「世界 😀」 — mix of 3-byte and 4-byte UTF-8 scalars so the
            // streaming decoder is exercised even without test-controlled
            // splits.
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
