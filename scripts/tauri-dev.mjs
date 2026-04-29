#!/usr/bin/env node
// Run `tauri dev` on a per-worktree dev-server port so multiple branches /
// worktrees can be developed in parallel without colliding on port 1420.
//
// Port resolution:
//   1. `ARBORIST_DEV_PORT` env var (explicit override).
//   2. Otherwise a deterministic hash of the current working directory in
//      the range [PORT_MIN, PORT_MIN + PORT_RANGE).
//
// The chosen port is exported to the child process so:
//   - `vite.config.ts` picks it up for the dev server.
//   - This script overrides Tauri's `build.devUrl` via `--config` so the
//     desktop shell loads the matching URL.
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { writeFileSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const PORT_MIN = 1420;
const PORT_RANGE = 100;
const PORT_MAX = 65535;

function failInvalidPortOverride(override) {
  console.error(
    `[arborist] ARBORIST_DEV_PORT must be an integer between 1 and ${PORT_MAX}; received "${override}"`,
  );
  process.exit(1);
}

function pickPort() {
  const override = process.env.ARBORIST_DEV_PORT;
  if (override !== undefined && override !== '') {
    if (!/^\d+$/.test(override)) {
      failInvalidPortOverride(override);
    }
    const port = Number(override);
    if (!Number.isInteger(port) || port < 1 || port > PORT_MAX) {
      failInvalidPortOverride(override);
    }
    return port;
  }
  const hash = createHash('sha1').update(process.cwd()).digest();
  return PORT_MIN + (hash.readUInt16BE(0) % PORT_RANGE);
}

const port = pickPort();
process.env.ARBORIST_DEV_PORT = String(port);

// Write the override to a temp JSON file rather than passing it inline; on
// Windows the shell strips quotes from inline JSON args, breaking parsing.
const dir = mkdtempSync(join(tmpdir(), 'arborist-dev-'));
const overridePath = join(dir, 'tauri.conf.override.json');
writeFileSync(overridePath, JSON.stringify({ build: { devUrl: `http://localhost:${port}` } }));

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

// On POSIX we can spawn `npx` directly without a shell and put the child in
// its own process group (`detached: true`); this lets us deliver signals to
// the whole tree (`tauri dev`, `vite`, …) via `process.kill(-pid, sig)`.
//
// On Windows we still need `shell: true` because Node's `spawn` refuses to
// execute `.cmd`/`.bat` files directly without a shell since the CVE-2024-27980
// fix.  In return we use `taskkill /pid <pid> /T /F` to terminate the whole
// process tree on shutdown — `child.kill()` against the wrapping `cmd.exe`
// alone does not reliably reach `tauri`/`vite`.
const child = spawn(
  isWindows ? 'npx.cmd' : 'npx',
  ['tauri', 'dev', '--config', overridePath, ...process.argv.slice(2)],
  {
    stdio: 'inherit',
    env: process.env,
    shell: isWindows,
    detached: !isWindows,
  },
);

function killChildTree(sig) {
  if (isWindows) {
    try {
      // /T = tree, /F = force. Spawn synchronously and ignore output —
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
const forwardSignals = isWindows
  ? ['SIGINT', 'SIGTERM', 'SIGHUP', 'SIGBREAK']
  : ['SIGINT', 'SIGTERM', 'SIGHUP'];
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
