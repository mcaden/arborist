#!/usr/bin/env node
// Build the `arborist-claude-hook` helper binary and stage it in the layout Tauri's `externalBin` expects.
//
// Tauri's bundler ships the main `arborist` binary by default but ignores sibling `[[bin]]` entries unless they're declared in
// `tauri.conf.json::bundle.externalBin`. `externalBin` expects each binary to live at `<base>-<target-triple>{.exe}` relative to the
// `tauri.conf.json` directory. This script:
//
//   1. Runs `cargo build --release --bin arborist-claude-hook` so the artifact exists on disk regardless of whether the surrounding `tauri build`
//      invocation passed `--bin arborist` (which it does — Tauri's CLI restricts the cargo target).
//   2. Resolves the host target triple via `rustc -vV` so we don't have to hardcode it across platforms.
//   3. Copies the built binary to `src-tauri/binaries/arborist-claude-hook-<triple>{.exe}` so `externalBin` picks it up.
//
// Wired via `tauri.conf.json::build.beforeBundleCommand` so it runs after frontend + main-bin builds but before bundling.

import { execSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(__dirname, '..');
const tauriDir = join(repoRoot, 'src-tauri');
const targetDir = join(tauriDir, 'target', 'release');
const outDir = join(tauriDir, 'binaries');

function exe(name) {
  return process.platform === 'win32' ? `${name}.exe` : name;
}

function rustcHostTriple() {
  const out = execSync('rustc -vV', { encoding: 'utf8' });
  const match = out.match(/^host:\s*(.+)$/m);
  if (!match) {
    throw new Error('could not parse host target triple from `rustc -vV`');
  }
  return match[1].trim();
}

function ensureDir(path) {
  if (!existsSync(path)) mkdirSync(path, { recursive: true });
}

console.log('[prepare-claude-hook-sidecar] building arborist-claude-hook (release)…');
execSync('cargo build --release --bin arborist-claude-hook', {
  cwd: tauriDir,
  stdio: 'inherit',
});

const src = join(targetDir, exe('arborist-claude-hook'));
if (!existsSync(src) || !statSync(src).isFile()) {
  throw new Error(`expected built helper at ${src} but did not find one`);
}

const triple = rustcHostTriple();
ensureDir(outDir);
const dstBase = `arborist-claude-hook-${triple}`;
const dst = join(outDir, exe(dstBase));
copyFileSync(src, dst);
console.log(`[prepare-claude-hook-sidecar] copied ${src} -> ${dst}`);
