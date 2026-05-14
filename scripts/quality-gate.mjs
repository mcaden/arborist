#!/usr/bin/env node
/**
 * Quality gate script — runs the full acceptance gate with:
 * - Auto-fix for formatting (eslint, prettier, cargo fmt) on failure
 * - Parallel frontend + Rust pipelines with organized output
 * - Compact, context-efficient reporting (only shows failures)
 * - Per-step timings, wall-clock total, and parallelism factor
 *
 * Usage:
 *   node scripts/quality-gate.mjs          # full gate
 *   node scripts/quality-gate.mjs --fe     # frontend only
 *   node scripts/quality-gate.mjs --rust   # rust only
 */
import { execSync } from 'node:child_process';
import { Worker, isMainThread, parentPort, workerData } from 'node:worker_threads';
import { performance } from 'node:perf_hooks';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);

// ─── Worker thread: runs a pipeline and posts results back ────────────────────

if (!isMainThread) {
  const results = runPipelineSync(workerData.steps);
  parentPort.postMessage(results);
  parentPort.close();
}

// ─── Main thread ──────────────────────────────────────────────────────────────

const args = process.argv.slice(2);
const feOnly = args.includes('--fe');
const rustOnly = args.includes('--rust');
const runFe = !rustOnly;
const runRust = !feOnly;

// ─── Helpers ──────────────────────────────────────────────────────────────────

function elapsed(startMs) {
  return ((performance.now() - startMs) / 1000).toFixed(1);
}

/**
 * Run a command synchronously, return { ok, output, seconds, label }.
 * Suppresses output on success; captures everything on failure.
 */
function runCmd(label, cmd) {
  const start = performance.now();
  try {
    execSync(cmd, { stdio: 'pipe', encoding: 'utf8', maxBuffer: 10 * 1024 * 1024 });
    return { ok: true, output: '', seconds: elapsed(start), label };
  } catch (e) {
    const output = (e.stdout || '') + (e.stderr || '');
    return { ok: false, output: output.trim(), seconds: elapsed(start), label };
  }
}

/**
 * Run a pipeline of steps sequentially. Auto-fixes formatting on failure.
 */
function runPipelineSync(steps) {
  const results = [];
  for (const step of steps) {
    let result = runCmd(step.label, step.cmd);
    if (!result.ok && step.fix) {
      runCmd(`${step.label} (fix)`, step.fix);
      result = runCmd(step.label, step.cmd);
      if (result.ok) {
        result.label += ' (auto-fixed)';
      }
    }
    results.push(result);
    if (!result.ok && step.bail !== false) break;
  }
  return results;
}

// ─── Pipeline definitions ─────────────────────────────────────────────────────

const FE_STEPS = [
  { label: 'eslint', cmd: 'pnpm exec eslint .', fix: 'pnpm exec eslint . --fix' },
  { label: 'prettier', cmd: 'pnpm exec prettier --check .', fix: 'pnpm exec prettier --write .' },
  { label: 'typecheck + build', cmd: 'pnpm exec tsc --noEmit && pnpm exec vite build' },
  { label: 'vitest', cmd: 'pnpm exec vitest run' },
];

const RUST_STEPS = [
  { label: 'cargo fmt', cmd: 'cargo fmt --all -- --check', fix: 'cargo fmt --all' },
  { label: 'cargo clippy', cmd: 'cargo clippy --workspace --all-targets --features test-helpers -- -D warnings' },
  { label: 'cargo test', cmd: 'cargo test --workspace --features test-helpers' },
];

// ─── Run pipeline in a worker thread ──────────────────────────────────────────

function runInWorker(steps) {
  return new Promise((resolve, reject) => {
    const worker = new Worker(__filename, { workerData: { steps } });
    worker.on('message', resolve);
    worker.on('error', reject);
    worker.on('exit', (code) => {
      if (code !== 0) reject(new Error(`Worker exited with code ${code}`));
    });
  });
}

// ─── Report ───────────────────────────────────────────────────────────────────

function report(label, results) {
  const failed = results.filter((r) => !r.ok);

  console.log(`\n${'═'.repeat(60)}`);
  console.log(`  ${label}`);
  console.log('═'.repeat(60));

  for (const r of results) {
    const icon = r.ok ? '✓' : '✗';
    console.log(`  ${icon} ${r.label} (${r.seconds}s)`);
  }

  if (failed.length > 0) {
    for (const r of failed) {
      console.log(`\n${'─'.repeat(60)}`);
      console.log(`  FAIL: ${r.label}`);
      console.log('─'.repeat(60));
      const lines = r.output.split('\n');
      if (lines.length > 80) {
        const omitted = lines.length - 80;
        console.log(`  ... (${omitted} ${omitted === 1 ? 'line' : 'lines'} omitted)`);
        console.log(lines.slice(-80).join('\n'));
      } else {
        console.log(r.output);
      }
    }
  }

  return failed.length === 0;
}

// ─── Main ─────────────────────────────────────────────────────────────────────

async function main() {
  const totalStart = performance.now();

  const tasks = [];
  const labels = [];
  if (runFe) {
    tasks.push(runInWorker(FE_STEPS));
    labels.push('Frontend');
  }
  if (runRust) {
    tasks.push(runInWorker(RUST_STEPS));
    labels.push('Rust');
  }

  const allResults = await Promise.all(tasks);

  let allPassed = true;
  for (let i = 0; i < allResults.length; i++) {
    if (!report(labels[i], allResults[i])) allPassed = false;
  }

  // Timing summary
  const totalSec = elapsed(totalStart);
  const allSteps = allResults.flat();
  const stepTotal = allSteps.reduce((sum, r) => sum + parseFloat(r.seconds), 0).toFixed(1);

  console.log(`\n${'═'.repeat(60)}`);
  console.log(`  ${allPassed ? '✓ ALL PASSED' : '✗ GATE FAILED'}`);
  console.log('─'.repeat(60));
  console.log(`  Wall-clock:  ${totalSec}s`);
  console.log(`  Step total:  ${stepTotal}s (sum of all steps)`);
  console.log(`  Parallelism: ${(stepTotal / totalSec).toFixed(1)}x`);
  console.log('═'.repeat(60) + '\n');

  process.exit(allPassed ? 0 : 1);
}

main();
