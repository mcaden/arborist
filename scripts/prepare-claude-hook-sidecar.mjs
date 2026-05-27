#!/usr/bin/env node
// Build Arborist sidecar binaries and stage them in the layout Tauri's `externalBin` expects.
//
// Tauri's bundler ships the main `arborist` binary by default but ignores sibling `[[bin]]` entries unless they're declared in
// `tauri.conf.json::bundle.externalBin`. `externalBin` expects each binary to live at `<base>-<target-triple>{.exe}` relative to the
// `tauri.conf.json` directory. This script stages both `arborist-claude-hook` and `arborist-mcp` by:
//
//   1. Running `cargo build --release -p <sidecar>` so the artifact exists on disk regardless of whether the surrounding `tauri build`
//      invocation passed `--bin arborist` (which it does — Tauri's CLI restricts the cargo target).
//   2. Asking Cargo for the workspace `target_directory` (it differs between a standalone crate and a workspace member — this repo is a workspace
//      and builds into the *repo-root* `target/`, not `src-tauri/target/`), resolving the active Tauri build target triple, and building the
//      sidecar for that target.
//   3. Copying (or for macOS universal, lipo-merging) the built helper to `src-tauri/binaries/<sidecar>-<triple>{.exe}` so `externalBin`
//      picks it up.
//
// Wired via `src-tauri/tauri.bundle.conf.json::build.beforeBuildCommand`, which is merged into the base config only for `pnpm tauri:build`
// (and the release workflow). The base `tauri.conf.json` is left clean so plain `cargo build`, `cargo test`, and `tauri dev` never trigger
// `externalBin` validation — those flows don't need the sidecar staged in `src-tauri/binaries/` (the cargo `[[bin]]` artifact at
// `target/{debug,release}/<sidecar>[.exe]` is the sibling-of-`arborist` that the runtime looks for at dev time).
//
// ## Security notes
//
// Both `cargo` and `rustc` are resolved as absolute paths under `$CARGO_HOME/bin/` (or `~/.cargo/bin/` when the env var isn't set — the rustup
// default) and spawned with `execFileSync` (no shell). This avoids spawning programs by name through a PATH lookup, which is the typical foot-gun
// SonarCloud flags as `javascript:S4036`. The host-triple parser splits `rustc -vV` line-by-line instead of using a multiline regex, sidestepping
// the `javascript:S5852` super-linear-backtracking warning.

import { execFileSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, statSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(__dirname, '..');
const tauriDir = join(repoRoot, 'src-tauri');
const outDir = join(tauriDir, 'binaries');
const SIDECARS = ['arborist-claude-hook', 'arborist-mcp'];

function exe(name, targetTriple) {
  const windowsTarget = targetTriple?.includes('windows') ?? false;
  return process.platform === 'win32' || windowsTarget ? `${name}.exe` : name;
}

// Resolve a rustup-installed binary (cargo, rustc) to an absolute path under `$CARGO_HOME/bin/`. Avoids PATH-based program resolution so a poisoned
// PATH (writable directory earlier than the rustup bin dir) can't hijack the build.
function cargoBin(name) {
  const cargoHome = process.env.CARGO_HOME ?? join(homedir(), '.cargo');
  return join(cargoHome, 'bin', exe(name));
}

function rustcHostTriple() {
  const out = execFileSync(cargoBin('rustc'), ['-vV'], { encoding: 'utf8' });
  for (const line of out.split('\n')) {
    if (line.startsWith('host:')) {
      return line.slice('host:'.length).trim();
    }
  }
  throw new Error('could not parse host target triple from `rustc -vV`');
}

function tauriBuildTargetTriple() {
  const fromTauri = process.env.TAURI_ENV_TARGET_TRIPLE?.trim();
  if (fromTauri) {
    return fromTauri;
  }
  return rustcHostTriple();
}

// Ask Cargo for the canonical workspace target directory. Hardcoding `src-tauri/target/release` is wrong for a workspace member —
// `cargo build` from `src-tauri/` still writes to the workspace's `target/`, which lives at the repo root for this project.
function cargoTargetDir() {
  const out = execFileSync(cargoBin('cargo'), ['metadata', '--no-deps', '--format-version', '1'], {
    cwd: tauriDir,
    encoding: 'utf8',
  });
  const meta = JSON.parse(out);
  if (!meta.target_directory) {
    throw new Error('cargo metadata did not return a target_directory');
  }
  return meta.target_directory;
}

function ensureDir(path) {
  if (!existsSync(path)) mkdirSync(path, { recursive: true });
}

function helperBuildEnv() {
  const env = { ...process.env };
  // Helper builds run inside Tauri's beforeBuildCommand. Strip Tauri_* vars so the
  // helper's Cargo build doesn't read the release overlay config (externalBin) and
  // fail before this script has staged the sidecars into src-tauri/binaries/.
  for (const key of Object.keys(env)) {
    if (key.startsWith('TAURI_')) {
      delete env[key];
    }
  }
  return env;
}

function buildSidecarForTarget(targetDir, targetTriple, binaryName) {
  console.log(`[prepare-claude-hook-sidecar] building ${binaryName} (release, target=${targetTriple})…`);
  execFileSync(cargoBin('cargo'), ['build', '--release', '-p', binaryName, '--target', targetTriple], {
    cwd: tauriDir,
    env: helperBuildEnv(),
    stdio: 'inherit',
  });

  const src = join(targetDir, targetTriple, 'release', exe(binaryName, targetTriple));
  if (!existsSync(src) || !statSync(src).isFile()) {
    throw new Error(`expected built sidecar at ${src} but did not find one`);
  }
  return src;
}

function stageSidecar(src, binaryName, targetTriple) {
  const dst = join(outDir, exe(`${binaryName}-${targetTriple}`, targetTriple));
  copyFileSync(src, dst);
  console.log(`[prepare-claude-hook-sidecar] copied ${src} -> ${dst}`);
  return dst;
}

const UNIVERSAL_MACOS_TARGET = 'universal-apple-darwin';
const MACOS_UNIVERSAL_TARGETS = ['aarch64-apple-darwin', 'x86_64-apple-darwin'];
const targetDir = cargoTargetDir();
const triple = tauriBuildTargetTriple();

ensureDir(outDir);

if (triple === UNIVERSAL_MACOS_TARGET) {
  if (process.platform !== 'darwin') {
    throw new Error(`target ${UNIVERSAL_MACOS_TARGET} requires a darwin host for lipo`);
  }

  for (const binaryName of SIDECARS) {
    const thinBinaries = MACOS_UNIVERSAL_TARGETS.map((target) => ({
      target,
      src: buildSidecarForTarget(targetDir, target, binaryName),
    }));
    for (const { target, src } of thinBinaries) {
      stageSidecar(src, binaryName, target);
    }

    const universalDst = join(outDir, exe(`${binaryName}-${triple}`, triple));
    console.log(`[prepare-claude-hook-sidecar] creating universal sidecar ${universalDst}`);
    execFileSync('/usr/bin/lipo', ['-create', ...thinBinaries.map(({ src }) => src), '-output', universalDst], {
      cwd: tauriDir,
      stdio: 'inherit',
    });
    if (!existsSync(universalDst) || !statSync(universalDst).isFile()) {
      throw new Error(`expected universal sidecar at ${universalDst} but did not find one`);
    }

    const cargoUniversalDst = join(targetDir, UNIVERSAL_MACOS_TARGET, 'release', exe(binaryName, UNIVERSAL_MACOS_TARGET));
    ensureDir(dirname(cargoUniversalDst));
    copyFileSync(universalDst, cargoUniversalDst);
    console.log(`[prepare-claude-hook-sidecar] copied ${universalDst} -> ${cargoUniversalDst}`);
    console.log(`[prepare-claude-hook-sidecar] created universal sidecar ${universalDst}`);
  }
} else {
  for (const binaryName of SIDECARS) {
    const src = buildSidecarForTarget(targetDir, triple, binaryName);
    stageSidecar(src, binaryName, triple);
  }
}
