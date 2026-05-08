// =============================================================================
// Workspace fixture helpers for e2e specs.
//
// Creates temporary git repos for use as workspace roots in tests.
// =============================================================================

import { execSync } from "child_process";
import fs from "fs";
import os from "os";
import path from "path";

/**
 * Create a temporary primary git repo (`.git` is a directory) that can serve
 * as a valid Arborist workspace root.
 *
 * Returns the absolute path to the repo root.
 */
export function createFixtureRepo(name = "fixture"): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `arborist-e2e-${name}-`));

  execSync("git init", { cwd: dir, stdio: "ignore" });
  execSync('git config user.email "test@arborist.dev"', { cwd: dir, stdio: "ignore" });
  execSync('git config user.name "Arborist E2E"', { cwd: dir, stdio: "ignore" });

  // Create an initial commit so HEAD is valid
  fs.writeFileSync(path.join(dir, "README.md"), "# Fixture repo\n");
  execSync("git add -A", { cwd: dir, stdio: "ignore" });
  execSync('git commit -m "initial commit"', { cwd: dir, stdio: "ignore" });

  return dir;
}

/**
 * Create a linked worktree off `primaryRepo` — this should be **rejected** by
 * Arborist's workspace validation (`.git` is a file, not a directory).
 *
 * Returns the absolute path to the linked worktree.
 */
export function createLinkedWorktree(primaryRepo: string, branchName = "linked-wt"): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `arborist-e2e-linked-`));
  // Remove the dir so git worktree add can create it
  fs.rmSync(dir, { recursive: true });

  execSync(`git worktree add -b ${branchName} "${dir}"`, {
    cwd: primaryRepo,
    stdio: "ignore",
  });

  return dir;
}
