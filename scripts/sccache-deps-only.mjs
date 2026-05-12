#!/usr/bin/env node
// rustc wrapper that scopes sccache to dependency crates only.
//
// Cargo invokes `rustc-wrapper` as: <wrapper> <rustc-path> <rustc-args...>.
// We sniff `--crate-name`, and for `build_script_build` also inspect the
// source path so only workspace build scripts bypass sccache. Workspace crates
// invoke rustc directly so multiple dogfooded branches running concurrently can
// never collide on cache entries for our own code. Dependency crates proxy to
// `sccache`, which is where the bulk of build time is anyway.

import { spawn } from 'node:child_process';
import { dirname, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const argv = process.argv.slice(2);
const rustc = argv[0];
const rustArgs = argv.slice(1);

const crateNameIdx = rustArgs.indexOf('--crate-name');
const crate = crateNameIdx >= 0 ? rustArgs[crateNameIdx + 1] : '';

const workspaceCrates = new Set([
  'arborist',
  'arborist_lib',
  'arborist_types',
  'arborist-types',
  'arborist_test_child',
  'arborist_test_locker',
  'arborist-test-child',
  'arborist-test-locker',
]);

const normalizePath = (value) => (process.platform === 'win32' ? value.toLowerCase() : value);
const workspaceRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const workspaceRootPrefix = `${normalizePath(workspaceRoot)}${sep}`;

const sourcePath = [...rustArgs].reverse().find((arg) => arg.endsWith('.rs') && !arg.startsWith('-'));
const sourcePathNormalized = sourcePath ? normalizePath(resolve(sourcePath)) : null;
const isWorkspaceBuildScript =
  (crate === 'build_script_build' || crate === 'build-script-build') &&
  sourcePathNormalized !== null &&
  (sourcePathNormalized === normalizePath(workspaceRoot) || sourcePathNormalized.startsWith(workspaceRootPrefix));

const isWorkspaceCrate = workspaceCrates.has(crate) || isWorkspaceBuildScript;

const cmd = isWorkspaceCrate ? rustc : 'sccache';
const cmdArgs = isWorkspaceCrate ? rustArgs : [rustc, ...rustArgs];

const child = spawn(cmd, cmdArgs, { stdio: 'inherit', shell: false });
child.on('exit', (code, signal) => {
  if (signal) {
    try {
      process.kill(process.pid, signal);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      process.stderr.write(`sccache-deps-only: child terminated by signal ${signal}; unable to forward signal (${message})\n`);
      process.exit(1);
    }
    return;
  }
  process.exit(code ?? 1);
});
child.on('error', (err) => {
  process.stderr.write(`sccache-deps-only: ${err.message}\n`);
  process.exit(1);
});
