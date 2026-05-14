# Worktrees

Arborist is built around one workspace root: a primary local Git clone. Worktree tabs and AI sessions run inside worktrees of that clone.

## Primary clone requirement

The workspace root must contain `.git` as a directory. A linked worktree has `.git` as a file containing `gitdir: ...`; Arborist rejects that case
because Git cannot create new linked worktrees from inside another linked worktree. Submodule working trees are rejected for the same reason.

Both boot-time validation and the `workspace_validate` command enforce this rule.

## Managed worktree location

Given workspace root `/path/to/repo`, Arborist-created worktrees live at:

```text
/path/to/repo/.arborist/.worktrees/<name>/
```

The command shape is:

```sh
git -C <workspaceRoot> worktree add .arborist/.worktrees/<name> -b <name>
```

`<name>` is both the directory name and the branch name. Keeping them the same makes the branch and path easy to reason about.

The `.arborist/` directory is intended to be source-controlled. Arborist writes `.arborist/.gitignore` so `.arborist/.worktrees/` itself remains
ignored.

## Validation rules

Worktree names are validated in both TypeScript (`validateWorktreeName`) and Rust (`compose::validate_worktree_name`):

- 1 to 255 Unicode scalar characters.
- Cannot be exactly `@` or start with `-`.
- No spaces or control characters.
- No `..`, `@{`, `//`, `~`, `^`, `:`, `?`, `*`, `[`, or `\`.
- Cannot start or end with `.` or `/`.
- Cannot end with `.lock`.
- Each `/`-separated path component must be non-empty, must not start with `.`, and must not end with `.lock`.

The same name is passed to Git as a branch reference and composed into a path under `.arborist/.worktrees/`, so both sides validate before the backend
shells out.

## Opening existing worktrees

The preferred existing-worktree path is `<workspace>/.arborist/.worktrees/<name>/`. Users can also open arbitrary existing worktree paths through the
manual directory picker. Manually created worktrees outside the managed directory are usable, but they are not normalized into the managed layout.

The legacy `worktreeRoots` config field may still contribute discovery results, but `workspaceRoot` and `.arborist/.worktrees/` are the primary model.

## Runtime behavior

| Operation                | Behavior                                                                                                                    |
| ------------------------ | --------------------------------------------------------------------------------------------------------------------------- |
| Open worktree tab        | Validates/canonicalizes the path and persists a `WorktreeTab`.                                                              |
| Launch AI session        | Passes the worktree path as child process `cwd`.                                                                            |
| Launch custom process    | Passes the parent worktree path as child process `cwd`.                                                                     |
| Close worktree tab       | Cascades close to children; optionally removes the worktree with Git.                                                       |
| Restore missing worktree | Behavior depends on context; restore/switch paths drop or error stale records rather than launching in the wrong directory. |

## Repo-stored settings

`<workspace>/.arborist/settings.json` can be committed to share repo-specific Arborist defaults. Supported overlays are:

- `defaultInstructionSets`
- `pluginSettings.ai.*.settings.launchCommand`
- `aiLaunchCommands.commands` legacy alias
- `worktreePrepCommands`

Arborist never writes this file. Malformed overlays are ignored with a warning.

## Safety invariants

- Worktree paths are canonicalized before use.
- Worktree paths are never interpolated into shell commands.
- The workspace root itself is never deleted by session/worktree close flows.
- Worktree deletion is refused when child process teardown is unconfirmed.
- Worktree deletion failures are reported as warnings in command results so UI state can still converge on "tab closed".
