#!/usr/bin/env node
// Run `tauri dev` on a per-worktree dev-server port so multiple branches /
// worktrees can be developed in parallel without colliding on port 1420.
//
// Port resolution: a deterministic hash of the current working directory in
// the range [PORT_MIN, PORT_MIN + PORT_RANGE).
//
// The chosen port is propagated to the child process via an override
// `tauri.conf.json` written to a temp dir:
//   - `build.beforeDevCommand` runs vite with `--port=<port>` so the frontend
//     binds to the right port (no env var required).
//   - `build.devUrl` is set to the matching URL so the desktop shell loads it.
//
// Sidecar build: before launching `tauri dev`, we run `cargo build --bin
// arborist-claude-hook` so the helper binary exists at `target/debug/` as a
// sibling of the about-to-be-built `arborist` binary. `tauri dev` only
// compiles the main `arborist` target via cargo, so without this step the
// first dev run on a clean checkout starts arborist with no sibling helper —
// `resolve_claude_hook_helper()` returns `None`, every Claude session in that
// session launches without `--settings`, and the sidebar falls back to
// PTY-byte heuristics with no indication of why. The release path
// (`tauri.bundle.conf.json::beforeBuildCommand`) already builds and stages
// the helper; this puts dev on equal footing.
import { spawn, execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { writeFileSync, mkdtempSync, rmSync, existsSync } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const PORT_MIN = 1420;
const PORT_RANGE = 1000;

function pickPort() {
  const hash = createHash('sha1').update(process.cwd()).digest();
  return PORT_MIN + (hash.readUInt16BE(0) % PORT_RANGE);
}

const port = pickPort();

// Write the override to a temp JSON file rather than passing it inline; on
// Windows the shell strips quotes from inline JSON args, breaking parsing.
const dir = mkdtempSync(join(tmpdir(), 'arborist-dev-'));
const overridePath = join(dir, 'tauri.conf.override.json');
writeFileSync(
  overridePath,
  JSON.stringify({
    build: {
      beforeDevCommand: `pnpm run vite --port=${port} --strictPort`,
      devUrl: `http://localhost:${port}`,
    },
  }),
);

let cleanedUp = false;
function cleanup() {
  if (cleanedUp) return;
  cleanedUp = true;
  try {
    rmSync(dir, { recursive: true, force: true });
  } catch {
    // best-effort: nothing actionable if removal fails
  }
}
process.on('exit', cleanup);

console.log(`[arborist] tauri dev on port ${port}`);

const isWindows = process.platform === 'win32';

// Prefer a rustup-style absolute cargo path when present, but fall back to user override / PATH so Homebrew/Nix/system installs still work.
function cargoBin() {
  const explicitCargo = process.env.CARGO?.trim();
  if (explicitCargo) {
    return explicitCargo;
  }
  const cargoHome = process.env.CARGO_HOME ?? join(homedir(), '.cargo');
  const rustupCargo = join(cargoHome, 'bin', isWindows ? 'cargo.exe' : 'cargo');
  if (existsSync(rustupCargo)) {
    return rustupCargo;
  }
  return isWindows ? 'cargo.exe' : 'cargo';
}

// Build the Claude hook sidecar before `tauri dev` so the helper sits next to `arborist.exe` in `target/debug/` on first launch. Synchronous because
// tauri dev's own cargo build holds the workspace target lock — running these in parallel would just serialise on the lock. After the first build,
// this is a near-no-op (cargo cache hit).
const __dirname = dirname(fileURLToPath(import.meta.url));
const projectRoot = join(__dirname, '..');
const tauriDir = join(projectRoot, 'src-tauri');
console.log('[arborist] building arborist-claude-hook (debug)…');
try {
  execFileSync(cargoBin(), ['build', '--bin', 'arborist-claude-hook'], {
    cwd: tauriDir,
    stdio: 'inherit',
  });
} catch (err) {
  console.error('[arborist] failed to build arborist-claude-hook — aborting; sidebar status would be degraded without it.');
  cleanup();
  process.exit(err?.status ?? 1);
}

const tauriDevArgs = ['dev', '--config', overridePath, ...process.argv.slice(2)];

// Resolve the local `tauri` binary from node_modules/.bin so we don't depend
// on `npx` (which can misresolve under some Node versions / setups).
const tauriBin = isWindows ? join(projectRoot, 'node_modules', '.bin', 'tauri.cmd') : join(projectRoot, 'node_modules', '.bin', 'tauri');

// On POSIX we can spawn the binary directly without a shell and put the child
// in its own process group (`detached: true`); this lets us deliver signals to
// the whole tree (`tauri dev`, `vite`, …) via `process.kill(-pid, sig)`.
//
// On Windows Node's `spawn` won't execute `.cmd`/`.bat` files directly since
// the CVE-2024-27980 fix, so we invoke `%COMSPEC%` (`cmd.exe`) explicitly and
// pass a single carefully-quoted command string. This keeps `--config
// <overridePath>` intact even when the temp path contains spaces (e.g. user
// profiles like `C:\Users\Jane Doe\AppData\Local\Temp\...`).  Shutdown still
// goes through `taskkill /pid <pid> /T /F` to terminate the full tree because
// killing `cmd.exe` alone doesn't reliably reach `cmd`/`tauri`/`vite`.
//
// With `/s /c`, cmd.exe strips the first and last `"` from the command string.
// We wrap the whole command in an extra pair of quotes so the inner per-arg
// quotes survive the strip and keep arguments properly separated.
function quoteCmdArg(value) {
  return `"${String(value).replace(/"/g, '""')}"`;
}

const child = isWindows
  ? spawn(process.env.ComSpec ?? 'cmd.exe', ['/d', '/s', '/c', `"${[quoteCmdArg(tauriBin), ...tauriDevArgs.map(quoteCmdArg)].join(' ')}"`], {
      stdio: 'inherit',
      env: process.env,
      windowsVerbatimArguments: true,
    })
  : spawn(tauriBin, tauriDevArgs, {
      stdio: 'inherit',
      env: process.env,
      detached: true,
    });

function killChildTree(sig) {
  if (isWindows) {
    try {
      // /T = tree, /F = force. Spawn and ignore output (fire-and-forget) —
      // failure usually just means the child already exited.
      spawn('taskkill', ['/pid', String(child.pid), '/T', '/F'], {
        stdio: 'ignore',
      });
    } catch {
      // best-effort
    }
    return;
  }
  try {
    // Negative pid targets the process group created by `detached: true`.
    process.kill(-child.pid, sig);
  } catch {
    try {
      child.kill(sig);
    } catch {
      // child may already be gone
    }
  }
}

// SIGBREAK is Windows-only; registering it on POSIX would crash with
// `ERR_UNKNOWN_SIGNAL`.
const forwardSignals = isWindows ? ['SIGINT', 'SIGTERM', 'SIGHUP', 'SIGBREAK'] : ['SIGINT', 'SIGTERM', 'SIGHUP'];
for (const sig of forwardSignals) {
  process.on(sig, () => killChildTree(sig));
}

child.on('exit', (code, signal) => {
  cleanup();
  if (signal) {
    // Mirror the child's terminating signal in our exit code (128 + signum
    // is the conventional shell encoding). For unknown signals, fall back
    // to exit code 1.
    const signums = { SIGINT: 2, SIGTERM: 15, SIGHUP: 1, SIGBREAK: 21 };
    const signum = signums[signal];
    if (signum === undefined) {
      process.exit(1);
    }
    process.exit(128 + signum);
  }
  process.exit(code ?? 1);
});
