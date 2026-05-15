#!/usr/bin/env node
/**
 * release-prep.mjs — Bump version across all manifest files, commit, tag, and push.
 *
 * Usage:
 *   node scripts/release-prep.mjs <version>
 *   pnpm run release:prep 0.1.2
 *
 * The version argument should be a bare semver (no "v" prefix). The script will:
 *  1. Validate the version string
 *  2. Update package.json, src-tauri/Cargo.toml, crates/arborist-types/Cargo.toml, src-tauri/tauri.conf.json
 *  3. Run `cargo update -p arborist -p arborist-types` to update Cargo.lock
 *  4. Commit the changes
 *  5. Create an annotated tag `v<version>`
 *  6. Push the commit and tag to origin
 *
 * Requires Node.js >= 21.2 (uses import.meta.dirname).
 */

import { readFileSync, writeFileSync } from 'node:fs';
import { execSync } from 'node:child_process';
import { resolve } from 'node:path';

const SEMVER_RE = /^\d+\.\d+\.\d+(?:-[\w.]+)?$/;

const version = process.argv[2];
if (!version) {
  console.error('Usage: node scripts/release-prep.mjs <version>');
  console.error('Example: node scripts/release-prep.mjs 0.1.2');
  process.exit(1);
}

if (!SEMVER_RE.test(version)) {
  console.error(`Invalid semver: "${version}". Expected format: X.Y.Z or X.Y.Z-prerelease`);
  process.exit(1);
}

const tag = `v${version}`;

// --- Preflight checks (all before any file modification) ---

// Check for uncommitted changes
try {
  const status = execSync('git status --porcelain', { encoding: 'utf8' }).trim();
  if (status) {
    console.error('Working tree is dirty. Commit or stash changes before running release-prep.');
    process.exit(1);
  }
} catch {
  console.error('Failed to check git status.');
  process.exit(1);
}

// Ensure we're on main to prevent accidental releases from feature branches
const root = resolve(import.meta.dirname, '..');
const branch = execSync('git rev-parse --abbrev-ref HEAD', { cwd: root, encoding: 'utf8' }).trim();
if (branch !== 'main') {
  console.error(`Current branch is '${branch}', but releases must be cut from 'main'.`);
  console.error('Switch to main and try again, or use --allow-branch to override.');
  if (!process.argv.includes('--allow-branch')) {
    process.exit(1);
  }
  console.warn('⚠ --allow-branch override: proceeding on non-main branch.');
}

// Check that the tag doesn't already exist (locally or on origin)
try {
  execSync(`git rev-parse --verify refs/tags/${tag}`, { stdio: 'pipe' });
  console.error(`Tag ${tag} already exists locally. Delete it first or choose a different version.`);
  process.exit(1);
} catch {
  // Tag doesn't exist locally — check remote
  const remote = execSync(`git ls-remote --tags origin refs/tags/${tag}`, { encoding: 'utf8' }).trim();
  if (remote) {
    console.error(`Tag ${tag} already exists on origin. Delete it first or choose a different version.`);
    process.exit(1);
  }
}

// --- Update version files ---

function updateJson(relPath, key) {
  const filePath = resolve(root, relPath);
  const content = readFileSync(filePath, 'utf8');
  const json = JSON.parse(content);
  const old = json[key];
  json[key] = version;
  // Preserve formatting: detect indent from original file
  const indent = content.match(/^(\s+)"/m)?.[1] || '  ';
  writeFileSync(filePath, JSON.stringify(json, null, indent) + '\n');
  console.log(`  ${relPath}: ${old} → ${version}`);
}

function updateCargoToml(relPath) {
  const filePath = resolve(root, relPath);
  const content = readFileSync(filePath, 'utf8');
  const updated = content.replace(/^(version\s*=\s*")([^"]+)(")/m, `$1${version}$3`);
  if (updated === content) {
    console.error(`  ${relPath}: no version field found!`);
    process.exit(1);
  }
  const old = content.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  writeFileSync(filePath, updated);
  console.log(`  ${relPath}: ${old} → ${version}`);
}

console.log(`\nBumping to ${version}:\n`);

updateJson('package.json', 'version');
updateJson('src-tauri/tauri.conf.json', 'version');
updateCargoToml('src-tauri/Cargo.toml');
updateCargoToml('crates/arborist-types/Cargo.toml');

// Update only workspace crate entries in Cargo.lock (avoids upgrading transitive deps)
console.log('\n  Updating Cargo.lock (workspace crates only)...');
execSync('cargo update -p arborist -p arborist-types', { cwd: root, stdio: 'pipe' });

// --- Git operations ---

// Detect no-op (e.g. re-running after a partial failure with the same version)
const diff = execSync('git status --porcelain', { cwd: root, encoding: 'utf8' }).trim();
if (!diff) {
  console.error(`\nNo changes to commit — manifests already at ${version}. Nothing to release.`);
  process.exit(1);
}

console.log(`\nCommitting and tagging ${tag}...\n`);

execSync('git add -A', { cwd: root });
execSync(`git commit -m "chore: release ${tag}"`, { cwd: root, stdio: 'inherit' });
execSync(`git tag -a "${tag}" -m "Release ${tag}"`, { cwd: root });

console.log(`\nPushing to origin...\n`);
execSync(`git push origin HEAD --follow-tags`, { cwd: root, stdio: 'inherit' });

console.log(`\n✅ Done! Tag ${tag} pushed. Trigger the Release workflow from the Actions tab.`);
