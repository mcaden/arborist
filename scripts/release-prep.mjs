#!/usr/bin/env node
/**
 * release-prep.mjs — Tag the current version on main and open a PR to bump to the next version.
 *
 * The repo follows a "next version in code" convention: the version in manifests on main
 * is always the UPCOMING release. Running this script "closes out" that version by tagging,
 * then branches to bump manifests to the next version and opens a PR.
 *
 * Usage:
 *   node scripts/release-prep.mjs <next-version>
 *   pnpm run release:prep 0.1.3
 *   pnpm run release:prep --skip-tag 0.1.3   # skip tagging (tag already pushed)
 *
 * The <next-version> is the version that will follow the current release. The script will:
 *  1. Read the current version from manifests (this is the version being released)
 *  2. Validate: on main, clean tree, tag doesn't exist, all 4 manifests agree
 *  3. Tag current HEAD as `v<current>` and push the tag (unless --skip-tag)
 *  4. Create branch `chore/bump-<next>`, bump all manifests to <next-version>
 *  5. Commit, push, and open a PR for the version bump
 *
 * Flags:
 *   --skip-tag   Skip tag creation/push (use when replaying after a partial failure
 *                where the tag was already pushed successfully)
 *
 * Requires Node.js >= 21.2 (uses import.meta.dirname).
 */

import { readFileSync, writeFileSync } from 'node:fs';
import { execSync } from 'node:child_process';
import { resolve } from 'node:path';

const SEMVER_RE = /^\d+\.\d+\.\d+(?:-[\w.]+)?$/;

// Parse flags
const args = process.argv.slice(2);
const skipTag = args.includes('--skip-tag');
const positional = args.filter((a) => !a.startsWith('--'));

const nextVersion = positional[0];
if (!nextVersion) {
  console.error('Usage: node scripts/release-prep.mjs [--skip-tag] <next-version>');
  console.error('Example: pnpm run release:prep 0.1.3');
  console.error('         pnpm run release:prep --skip-tag 0.1.3');
  console.error('\nThis tags the current version in manifests and opens a PR to bump to <next-version>.');
  console.error('\nFlags:');
  console.error('  --skip-tag   Skip tag creation/push (for replaying after partial failure)');
  process.exit(1);
}

if (!SEMVER_RE.test(nextVersion)) {
  console.error(`Invalid semver: "${nextVersion}". Expected format: X.Y.Z or X.Y.Z-prerelease`);
  process.exit(1);
}

const root = resolve(import.meta.dirname, '..');
const exec = (cmd, opts = {}) => execSync(cmd, { cwd: root, encoding: 'utf8', ...opts }).trim();

// --- Read current version from manifests ---

const currentVersion = JSON.parse(readFileSync(resolve(root, 'src-tauri/tauri.conf.json'), 'utf8')).version;
const tag = `v${currentVersion}`;

// Validate all 4 manifests agree
const manifests = [
  { path: 'package.json', version: JSON.parse(readFileSync(resolve(root, 'package.json'), 'utf8')).version },
  { path: 'src-tauri/tauri.conf.json', version: currentVersion },
  {
    path: 'src-tauri/Cargo.toml',
    version: readFileSync(resolve(root, 'src-tauri/Cargo.toml'), 'utf8').match(/^version\s*=\s*"([^"]+)"/m)?.[1],
  },
  {
    path: 'crates/arborist-types/Cargo.toml',
    version: readFileSync(resolve(root, 'crates/arborist-types/Cargo.toml'), 'utf8').match(/^version\s*=\s*"([^"]+)"/m)?.[1],
  },
];

const mismatches = manifests.filter((m) => m.version !== currentVersion);
if (mismatches.length > 0) {
  console.error('Version mismatch across manifests:');
  for (const m of manifests) {
    console.error(`  ${m.path}: ${m.version}${m.version !== currentVersion ? ' ← MISMATCH' : ''}`);
  }
  console.error('\nAll manifests must agree before releasing. Fix manually and retry.');
  process.exit(1);
}

if (nextVersion === currentVersion) {
  console.error(`Next version (${nextVersion}) is the same as current version (${currentVersion}). Nothing to do.`);
  process.exit(1);
}

console.log(`Current version: ${currentVersion} (will be tagged as ${tag})`);
console.log(`Next version:    ${nextVersion} (will be the PR bump target)\n`);

// --- Preflight checks ---

// Clean working tree
try {
  const status = exec('git status --porcelain');
  if (status) {
    console.error('Working tree is dirty. Commit or stash changes before running release-prep.');
    process.exit(1);
  }
} catch {
  console.error('Failed to check git status.');
  process.exit(1);
}

// Must be on main
const branch = exec('git rev-parse --abbrev-ref HEAD');
if (branch !== 'main') {
  console.error(`Current branch is '${branch}', but releases must be cut from 'main'.`);
  process.exit(1);
}

// Local main must be up-to-date with origin/main
exec('git fetch origin main');
const localSha = exec('git rev-parse HEAD');
const remoteSha = exec('git rev-parse origin/main');
if (localSha !== remoteSha) {
  console.error(`Local main (${localSha.slice(0, 8)}) is not up-to-date with origin/main (${remoteSha.slice(0, 8)}).`);
  console.error('Run `git pull` and retry.');
  process.exit(1);
}

// Tag must not exist locally or on origin (skip check if --skip-tag)
if (!skipTag) {
  try {
    exec(`git rev-parse --verify refs/tags/${tag}`, { stdio: 'pipe' });
    console.error(`Tag ${tag} already exists locally. Delete it first or choose a different version.`);
    process.exit(1);
  } catch {
    const remote = exec(`git ls-remote --tags origin refs/tags/${tag}`);
    if (remote) {
      console.error(`Tag ${tag} already exists on origin. Delete it first or choose a different version.`);
      process.exit(1);
    }
  }
} else {
  // When skipping tag, verify it actually exists (otherwise --skip-tag is being misused)
  const remote = exec(`git ls-remote --tags origin refs/tags/${tag}`);
  if (!remote) {
    console.error(`--skip-tag was passed but tag ${tag} does not exist on origin. Cannot skip what doesn't exist.`);
    process.exit(1);
  }
  console.log(`ℹ️  --skip-tag: skipping tag creation (${tag} already exists on origin)\n`);
}

// Bump branch must not already exist
const bumpBranch = `chore/bump-${nextVersion}`;
try {
  exec(`git rev-parse --verify refs/heads/${bumpBranch}`, { stdio: 'pipe' });
  console.error(`Branch '${bumpBranch}' already exists locally. Delete it first or choose a different version.`);
  process.exit(1);
} catch {
  // expected — branch doesn't exist
}
const remoteBump = exec(`git ls-remote --heads origin refs/heads/${bumpBranch}`);
if (remoteBump) {
  console.error(`Branch '${bumpBranch}' already exists on origin. Delete it first or choose a different version.`);
  process.exit(1);
}

// --- Step 1: Tag current HEAD and push tag ---

if (!skipTag) {
  console.log(`Tagging ${tag} on main...\n`);
  execSync(`git tag -a "${tag}" -m "Release ${tag}"`, { cwd: root });
  execSync(`git push origin refs/tags/${tag}`, { cwd: root, stdio: 'inherit' });
  console.log(`\n✅ Tag ${tag} pushed.\n`);
} else {
  console.log(`⏭️  Skipping tag (--skip-tag). Tag ${tag} already on origin.\n`);
}

// --- Step 2: Branch, bump to next version, push, open PR ---

console.log(`Creating branch '${bumpBranch}' for next-version bump...\n`);

try {
  execSync(`git checkout -b "${bumpBranch}"`, { cwd: root, stdio: 'pipe' });

  // Bump all manifests to nextVersion
  function updateJson(relPath, key) {
    const filePath = resolve(root, relPath);
    const content = readFileSync(filePath, 'utf8');
    const json = JSON.parse(content);
    json[key] = nextVersion;
    const indent = content.match(/^(\s+)"/m)?.[1] || '  ';
    writeFileSync(filePath, JSON.stringify(json, null, indent) + '\n');
    console.log(`  ${relPath}: ${currentVersion} → ${nextVersion}`);
  }

  function updateCargoToml(relPath) {
    const filePath = resolve(root, relPath);
    const content = readFileSync(filePath, 'utf8');
    const updated = content.replace(/^(version\s*=\s*")([^"]+)(")/m, `$1${nextVersion}$3`);
    if (updated === content) {
      console.error(`  ${relPath}: no version field found!`);
      process.exit(1);
    }
    writeFileSync(filePath, updated);
    console.log(`  ${relPath}: ${currentVersion} → ${nextVersion}`);
  }

  updateJson('package.json', 'version');
  updateJson('src-tauri/tauri.conf.json', 'version');
  updateCargoToml('src-tauri/Cargo.toml');
  updateCargoToml('crates/arborist-types/Cargo.toml');

  // Update Cargo.lock for workspace crates only
  console.log('\n  Updating Cargo.lock (workspace crates only)...');
  execSync('cargo update -p arborist -p arborist-types', { cwd: root, stdio: 'pipe' });

  // Commit and push
  execSync('git add -A', { cwd: root });
  execSync(`git commit -m "chore: bump version to ${nextVersion}"`, { cwd: root, stdio: 'inherit' });
  execSync(`git push origin "${bumpBranch}"`, { cwd: root, stdio: 'inherit' });

  // Open PR
  console.log('\nOpening PR...\n');
  const prTitle = `chore: bump version to ${nextVersion}`;
  const prBody = `Automated version bump following the ${tag} release.\n\nBumps all manifests to ${nextVersion} in preparation for the next development cycle.`;
  execSync(`gh pr create --title "${prTitle}" --body "${prBody}" --base main --head "${bumpBranch}"`, {
    cwd: root,
    stdio: 'inherit',
  });
} catch (err) {
  console.error(`\n❌ Step 2 (version bump) failed: ${err.message}`);
  if (!skipTag) {
    console.error(`\n⚠️  Tag ${tag} was already pushed to origin.`);
    console.error(`The release build can still be triggered with: gh workflow run release.yml -f tag=${tag}`);
  }
  console.error(`\nTo retry the version bump, rerun with --skip-tag:`);
  console.error(`  pnpm run release:prep --skip-tag ${nextVersion}`);
  console.error(`\nTo clean up and start over:`);
  console.error(`  git checkout main`);
  console.error(`  git branch -D ${bumpBranch} 2>/dev/null`);
  console.error(`  git push origin :refs/heads/${bumpBranch} 2>/dev/null`);
  process.exit(1);
}

// Return to main
execSync('git checkout main', { cwd: root, stdio: 'pipe' });

console.log(`\n✅ Done!`);
console.log(`   • Tagged ${tag} and pushed to origin`);
console.log(`   • Opened PR to bump to ${nextVersion}`);
console.log(`   • Trigger the Release workflow: gh workflow run release.yml -f tag=${tag}`);
