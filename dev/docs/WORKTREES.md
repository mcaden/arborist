# Worktrees in Arborist

Arborist is built around the assumption that a single repository — the
**workspace root** — is the anchor for everything. Every Arborist session
runs the CLI inside a Git worktree of that repository, and every worktree
created by Arborist lives at a fixed, predictable location underneath it.

This document explains the convention, why it exists, and how the rest of
the app cooperates with it.

## The convention

Given a configured `workspaceRoot` of `/path/to/repo`, all worktrees
created by Arborist are placed at:

```
/path/to/repo/.worktrees/<name>/
```

`<name>` is both the directory name and the branch name created for that
worktree. The two are kept in lockstep deliberately so that the path on
disk and the branch in `git branch` are always trivially derivable from
one another.

The exact `git` invocation Arborist uses (DESIGN §6, `worktree_create`):

```sh
git -C <workspaceRoot> worktree add .worktrees/<name> -b <name>
```

## Why a fixed subdirectory under the repo

- **Discovery is trivial.** The new-session dialog can list available
  worktrees by enumerating `<workspaceRoot>/.worktrees/` directly,
  without invoking `git worktree list` and post-filtering. This keeps
  the Step 2 (Existing) path responsive even on cold caches.
- **Single source of truth.** Putting every Arborist-created worktree at
  one canonical location means no per-user or per-OS configuration is
  needed; a teammate cloning the same repo gets the same layout.
- **`.gitignore`-friendly.** Recommended `.gitignore` for the workspace
  repo: add `/.worktrees/` so Git itself ignores the directory tree. (Git
  does not register linked worktrees inside the repo as untracked files,
  but tools that walk the working copy may.)
- **Predictable cleanup.** Removing a worktree is `git worktree remove`
  on a known path; there is no scattered list of arbitrary paths to
  reconcile.

## Validation

Worktree names are validated identically on the frontend (TS
`validateWorktreeName`) and the backend (Rust
`compose::validate_worktree_name`). Rules:

- 1–255 characters
- No spaces
- No `..`, `~`, `^`, `:`, `?`, `*`, `[`, `\`
- Cannot start or end with `.` or `/`
- Cannot end with `.lock`
- Cannot be exactly `@`

The same rules apply on both sides because the name is composed straight
into the path and the branch reference; getting it wrong would either
escape the `.worktrees/` directory or produce a refname Git will reject
(`git check-ref-format`).

## Existing worktrees outside `.worktrees/`

Worktrees not created by Arborist (e.g., the main checkout itself, or
worktrees a user created by hand at arbitrary paths) are still usable
via the **Browse…** option in the new-session dialog Step 2. They are
not auto-discovered. If they live under a directory listed in the
legacy `worktreeRoots` config field (see `CONFIGURATION.md`), they are
also enumerated via the `worktrees_list` command — but new
installations are encouraged to standardise on `workspaceRoot` +
`.worktrees/`.

## Lifecycle and persistence

- Creation goes through the Tauri command `worktree_create` (DESIGN §6).
- The chosen worktree path is stored verbatim on the `Session` record
  and passed to `portable-pty` as the `cwd` field — never interpolated
  into the composed shell command (DESIGN §8.1, SPEC NF-08).
- If a worktree directory is deleted out from under Arborist, restore
  and restart raise an `Error` status with a user-facing message (see
  `Session.statusMessage`); the session row remains so the user can
  decide whether to re-create the worktree or close the session.

## See also

- `SPEC.md` §5.5 — Worktree Discovery requirements
- `DESIGN.md` §3 — `AppConfig.workspaceRoot` / `worktreeRoots`
- `DESIGN.md` §6 — `worktree_create`, `worktrees_list`, `workspace_validate`
- `ROADMAP.md` §1, §2 — single-workspace model and worktree creation work
