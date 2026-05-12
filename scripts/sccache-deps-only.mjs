#!/usr/bin/env node
// rustc wrapper that scopes sccache to dependency crates only.
//
// Cargo invokes `rustc-wrapper` as: <wrapper> <rustc-path> <rustc-args...>.
// We sniff `--crate-name`. For the workspace's own crates (arborist*) we
// invoke rustc directly so multiple dogfooded branches running concurrently
// can never collide on cache entries for our own code. For dependency
// crates we proxy to `sccache`, which is where the bulk of build time is
// anyway.

import { spawn } from 'node:child_process';

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
  'build_script_build',
  'build-script-build',
]);

const isWorkspaceCrate = workspaceCrates.has(crate);

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
