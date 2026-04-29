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
import { writeFileSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const PORT_MIN = 1420;
const PORT_RANGE = 100;

function pickPort() {
  const override = process.env.ARBORIST_DEV_PORT;
  if (override && /^\d+$/.test(override)) {
    return Number(override);
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
writeFileSync(
  overridePath,
  JSON.stringify({ build: { devUrl: `http://localhost:${port}` } }),
);

console.log(`[arborist] tauri dev on port ${port}`);

const isWindows = process.platform === 'win32';
const child = spawn(
  isWindows ? 'npx.cmd' : 'npx',
  ['tauri', 'dev', '--config', overridePath, ...process.argv.slice(2)],
  { stdio: 'inherit', env: process.env, shell: isWindows },
);
child.on('exit', (code) => process.exit(code ?? 1));
