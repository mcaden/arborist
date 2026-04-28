//! One-shot raw-byte capture of an interactive CLI under a PTY.
//!
//! Used to design the Tier-1 OSC scanner: spawns the requested CLI under
//! `portable-pty` (the same backend the app uses), pumps every byte to
//! both stdout (printable form) and a `.bin` file (raw), and after a
//! configurable settle-time sends Ctrl-C and exits.
//!
//! Usage:
//! ```text
//! cargo run -p arborist --example pty_capture -- claude  C:\some\worktree  out\claude.bin  6
//! cargo run -p arborist --example pty_capture -- copilot C:\some\worktree  out\copilot.bin 6
//! ```
//!
//! Args (positional):
//! 1. CLI program name (no flags) — e.g. `claude`, `copilot`.
//! 2. Working directory to launch in.
//! 3. Output `.bin` path (parent dir is created if missing).
//! 4. Seconds to wait before sending Ctrl-C and tearing the PTY down.
//!
//! All output is printed to stdout in a hex+ascii view as it streams in
//! so you can eyeball it. The raw bytes go to the file unchanged.

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

fn main() {
    let mut args = env::args().skip(1);
    let program = args.next().expect("program (claude|copilot|...)");
    let cwd = PathBuf::from(args.next().expect("cwd"));
    let out_path = PathBuf::from(args.next().expect("out path"));
    let seconds: u64 = args
        .next()
        .map(|s| s.parse().expect("seconds must be u64"))
        .unwrap_or(6);

    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).expect("create out dir");
    }
    let mut out = fs::File::create(&out_path).expect("create out file");

    println!("[capture] program  = {program}");
    println!("[capture] cwd      = {}", cwd.display());
    println!("[capture] out      = {}", out_path.display());
    println!("[capture] seconds  = {seconds}");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(&program);
    cmd.cwd(&cwd);
    let mut child = pair.slave.spawn_command(cmd).expect("spawn");

    // Drop the slave handle we don't need so the child sees EOF if the
    // master is closed.
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let writer = pair.master.take_writer().expect("take writer");

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut buf = [0u8; 4096];
    let mut total: u64 = 0;

    // Threaded read loop: portable-pty reads are blocking. Use a channel
    // to surface bytes to main with a timeout.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let _reader_thread = std::thread::spawn(move || loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    });

    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => {
                total += chunk.len() as u64;
                out.write_all(&chunk).expect("write file");
                print_pretty(&chunk);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    println!("\n[capture] settle expired — sending Ctrl-C");
    {
        let mut writer = writer;
        let _ = writer.write_all(b"\x03");
        let _ = writer.flush();
    }

    // Drain anything still in the pipe for a moment.
    let drain_until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < drain_until {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => {
                total += chunk.len() as u64;
                out.write_all(&chunk).expect("write file");
                print_pretty(&chunk);
            }
            Err(_) => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    println!("\n[capture] wrote {total} bytes -> {}", out_path.display());
}

/// Print a chunk in a hex+ascii view to stdout. Escapes control bytes as
/// `<HEX>` so OSC/CSI sequences stand out.
fn print_pretty(chunk: &[u8]) {
    let mut line = String::new();
    for &b in chunk {
        match b {
            0x1b => line.push_str("<ESC>"),
            0x07 => line.push_str("<BEL>"),
            b'\n' => {
                line.push_str("<LF>\n");
            }
            b'\r' => line.push_str("<CR>"),
            b'\t' => line.push_str("<TAB>"),
            0x00..=0x06 | 0x08 | 0x0b | 0x0c | 0x0e..=0x1a | 0x1c..=0x1f | 0x7f => {
                line.push_str(&format!("<{b:02X}>"));
            }
            _ => line.push(b as char),
        }
    }
    if !line.is_empty() {
        print!("{line}");
    }
    let _ = std::io::stdout().flush();
}