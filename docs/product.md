# Product contract

This document is the current product contract for Arborist. If implementation and this document disagree, update the implementation or this document
in the same change so behavior and documentation do not silently drift.

## Users and goals

Arborist is for developers who use local Git worktrees and AI coding CLIs. The app should make it easy to keep several worktree-scoped assistant
sessions alive, visible, restorable, and safe to switch between.

Success looks like:

- First prompt after app launch in under 5 seconds for a typical restored session.
- Tab switch to visible terminal in under 200 ms.
- Background sessions continue running when hidden.
- No credential storage by Arborist.
- Clear recovery paths when a CLI exits, a worktree disappears, or config is invalid.

## Core concepts

| Concept                    | Definition                                                                                       |
| -------------------------- | ------------------------------------------------------------------------------------------------ |
| Workspace root             | The primary local Git clone Arborist is bound to. Must contain `.git` as a directory.            |
| Worktree tab               | A top-level sidebar tab for a worktree path. Its dashboard is shown when no child is active.     |
| AI session                 | A Claude, Copilot, or Codex CLI process running in a PTY in the worktree `cwd`.                  |
| Custom-process sub-session | A child tab launched from a configured custom process definition.                                |
| Worktree prep              | One-shot commands run after `worktree_create`, with output logged and surfaced through a banner. |

## Functional requirements

### Sidebar and worktree tabs

| ID   | Requirement                                                                                                                                                                |
| ---- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| S-01 | The sidebar must be a vertical tab bar on the left edge of the window.                                                                                                     |
| S-02 | Top-level rows are worktree tabs. Child rows are AI sessions and custom-process sub-sessions.                                                                              |
| S-03 | Clicking a worktree tab shows its dashboard unless it has an active child. Clicking a child shows that child's terminal or app-tracking view.                              |
| S-04 | The `+` flow opens or creates a worktree tab. AI agents are launched later from the worktree context menu.                                                                 |
| S-05 | Closing a worktree tab cascades to children. AI sessions and terminal sub-sessions terminate; application sub-sessions detach or terminate according to the chosen policy. |
| S-06 | Active worktree and active child selection must be visually clear and keyboard navigable.                                                                                  |
| S-07 | Top-level worktree order must persist. Child reordering can be added later.                                                                                                |
| S-08 | Close actions that terminate processes must require confirmation.                                                                                                          |
| S-09 | The worktree-tab context menu must expose Launch Claude, Launch Copilot, Launch Codex, enabled custom-process entries, custom-process settings, and close.                 |

### Workspaces and worktrees

| ID   | Requirement                                                                                                                                                            |
| ---- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| W-01 | Arborist must bind to one active workspace root per process.                                                                                                           |
| W-02 | The workspace root must be a primary clone, not a linked worktree or submodule working tree.                                                                           |
| W-03 | New worktrees created by Arborist must live under `<workspace>/.arborist/.worktrees/<name>/`.                                                                          |
| W-04 | Worktree names must be validated on the frontend and backend before shelling out to Git.                                                                               |
| W-05 | The user must still be able to open a worktree path manually when it is outside the managed directory.                                                                 |
| W-06 | Workspace switching must park old workspace sessions, preserve their records, bind the new workspace, and restore its sessions atomically from the user's perspective. |

### AI sessions and terminals

| ID   | Requirement                                                                                                        |
| ---- | ------------------------------------------------------------------------------------------------------------------ |
| T-01 | Each AI session must run in an interactive PTY with ANSI output, input, and resize support.                        |
| T-02 | Only one terminal viewport is visible at a time. Hidden sessions must keep running.                                |
| T-03 | Terminal instances in the frontend must survive tab switches; only DOM attachment changes.                         |
| T-04 | Session restart must reuse the persisted `composedCommand` and worktree path, not recompose from current settings. |
| T-05 | Restored sessions must register after frontend listeners are attached and spawn at the measured terminal size.     |
| T-06 | Unexpected non-zero exit must surface an error state with a restart action.                                        |
| T-07 | Clean exit must surface an exited state instead of silently pretending the session is still active.                |

### AI launch commands

| ID   | Requirement                                                                                                                                                                                                                                                                                                                                                |
| ---- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| I-01 | Claude, Copilot, and Codex run in the selected worktree `cwd` so each tool can load its own repository-level instruction files.                                                                                                                                                                                                                            |
| I-02 | Arborist must not pass `--system-prompt` to Claude, `--instructions` to Copilot, or any instruction flag to Codex for newly created sessions.                                                                                                                                                                                                              |
| I-03 | Custom AI launch commands are plugin-keyed overrides. Missing or empty override means use the plugin default program.                                                                                                                                                                                                                                      |
| I-04 | Dynamic paths must be canonicalized before use and shell-quoted when inserted into command strings. Worktree paths are passed as `cwd`, not interpolated.                                                                                                                                                                                                  |
| I-05 | Minimum supported AI CLI versions: GitHub Copilot CLI `>= 1.0.51` (Arborist depends on the `--session-id <uuid>` create-or-resume flag introduced in that release; older versions only had `--resume`, which became strict-resume-only in 1.0.51 and would fail every Copilot session). Claude and Codex have no minimum-version requirement at this time. |

### Custom processes and sub-sessions

| ID    | Requirement                                                                                                                                       |
| ----- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| CP-01 | Users can define, edit, enable, disable, and delete custom process definitions in Settings.                                                       |
| CP-02 | Disabled definitions stay editable but do not appear in launch menus.                                                                             |
| CP-03 | Terminal definitions spawn an in-app PTY under the parent worktree tab.                                                                           |
| CP-04 | Application definitions spawn detached external processes. Closing the tab must not kill them unless the user chooses a terminating close policy. |
| CP-05 | Application focus is best-effort and platform-specific. Missing optional tools and unsupported desktops must degrade clearly.                     |
| CP-06 | Terminal sub-sessions are restored by respawn. Application sub-sessions restore as exited/greyed and relaunch on user action.                     |
| CP-07 | Cascading close must not silently leak failed terminal sub-session teardown; errors are reported in the close result.                             |

### Built-in plugins

| ID    | Requirement                                                                                                                                       |
| ----- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| BP-01 | Settings must expose a Plugins tab where registered built-in plugins can be enabled or disabled by kind.                                          |
| BP-02 | Disabled AI plugins must not appear as new-session launch actions; disabled dashboard widgets must not render on the worktree dashboard.          |
| BP-03 | Plugin-owned settings must be associated with the matching plugin entry. AI CLI launch command overrides belong to the Claude/Copilot plugin row. |

### Persistence and recovery

| ID   | Requirement                                                                                                                               |
| ---- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| P-01 | Config and session state persist as JSON through the Rust `ConfigStore`.                                                                  |
| P-02 | Config writes are atomic. Corrupt JSON is quarantined and replaced with defaults rather than crashing the app.                            |
| P-03 | Each running process holds an advisory lock for its `(branch, workspace)` store. Lock contention is a hard error at boot and switch time. |
| P-04 | Restore is per-session and best-effort. One bad session must not prevent other sessions from restoring.                                   |
| P-05 | Worktree prep logs are contained under app data and opened only after containment validation.                                             |

## Non-functional requirements

| ID    | Requirement                                                                                             |
| ----- | ------------------------------------------------------------------------------------------------------- |
| NF-01 | Arborist must run on Windows, macOS, and Linux.                                                         |
| NF-02 | PTY input-to-echo latency should be imperceptible in normal use.                                        |
| NF-03 | Startup should remain responsive while restore work registers in the backend.                           |
| NF-04 | The app must not store credentials or tokens. Auth belongs to the external CLIs.                        |
| NF-05 | Paths from users or config must be canonicalized before use.                                            |
| NF-06 | The command/event API must stay typed, capability-gated, and documented.                                |
| NF-07 | Accessibility must cover keyboard navigation, labels, focus, and non-color status cues where practical. |
| NF-08 | Logs and telemetry must avoid leaking secrets beyond what the invoked tools themselves write.           |

## Out of scope for v1

- Built-in chat UI.
- Remote or SSH worktrees.
- Multi-window support.
- In-app instruction-file editing.
- A general third-party plugin marketplace.
- Automatic app updates.
