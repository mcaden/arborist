# Arborist — Product Specification
_Version 0.3_

## 1. Overview

Arborist is a desktop application that provides a unified workspace for managing multiple AI coding assistant sessions (Claude CLI and GitHub Copilot CLI) across Git worktrees. It features a vertical-tab sidebar for session management and an integrated terminal area that hosts the active CLI session.

## 2. Problem Statement

Developers working across multiple Git worktrees frequently need to spin up AI assistant sessions scoped to each worktree. Today this requires manually opening terminals, navigating to the correct directory, and launching the CLI with the right flags. Arborist eliminates this friction by providing a single UI that:

- Launches Claude or Copilot CLI sessions bound to a specific worktree.
- Passes pre-configured instructions/system prompts to each session.
- Keeps every session accessible via a persistent sidebar.

## 3. Target Users

- Software engineers who use Git worktrees for parallel feature/branch development.
- Teams that use Claude CLI (`claude`) and/or GitHub Copilot CLI (`gh copilot`) as daily tools.

## 4. Core Concepts

| Concept        | Definition |
|----------------|------------|
| **Session**    | A running instance of a CLI tool (Claude or Copilot) bound to a specific worktree path. |
| **Tool**       | Either `claude` (Anthropic Claude CLI) or `copilot` (GitHub Copilot CLI). |
| **Worktree**   | A Git worktree directory on the local filesystem. |
| **Instruction Set** | A set of predefined instructions/system prompt text passed to the CLI tool at launch. |

## 5. Functional Requirements

### 5.1 Sidebar (Vertical Tabs)

| ID     | Requirement |
|--------|-------------|
| S-01   | The sidebar MUST be displayed as a vertical tab bar on the left edge of the window. |
| S-02   | Each tab MUST display the tool icon (Claude or Copilot logo) and the session label. |
| S-03   | Clicking a tab MUST switch the main area to that session's terminal. |
| S-04   | The sidebar MUST include a "+" button (or equivalent) to create a new session. |
| S-05   | Tabs MUST support close/remove to terminate a session. |
| S-06   | Tabs SHOULD visually indicate the active session (highlight, border, etc.). |
| S-07   | Tabs SHOULD support drag-to-reorder; order MUST persist across restarts. |
| S-08   | Clicking the close button MUST present a confirmation dialog before terminating the session. |

### 5.2 Session Creation Flow

| ID     | Requirement |
|--------|-------------|
| C-01   | Pressing the "+" button MUST present a choice of tool: Claude or Copilot. |
| C-02   | After tool selection, the app MUST prompt the user to select a worktree directory. The picker SHOULD offer a list of worktrees detected from configured root repositories (see §5.5) in addition to a manual OS file picker. |
| C-03   | After worktree selection, the app MUST open a new terminal in the main area that runs the CLI launch command in the worktree's directory. (Issue #63 separated one-shot setup from per-session launch — see C-06; pre-launch joining was removed in `configVersion = 5`.) |
| C-04   | The instruction set used MAY be configurable per-tool (see §5.4); when none is configured, the CLI relies on its built-in `cwd`-based discovery (see I-04). |
| C-05   | Multiple sessions for the same tool and worktree MUST be allowed. The new session's tab label MUST append a numeric suffix to disambiguate (e.g., "my-feature 2", "my-feature 3"). |
| C-06   | When a new worktree is created via `worktree_create` and `AppConfig.worktreePrepCommands` is non-empty, the app MUST asynchronously run those commands once in the worktree's directory, capture combined stdout/stderr to a per-prep log under `<app_data_dir>/worktree-prep-logs/`, and surface lifecycle (running / success / failure) via an in-app banner with a "View log" affordance. Prep failures MUST NOT prevent the worktree from being created. |

### 5.3 Main Terminal Area

| ID     | Requirement |
|--------|-------------|
| T-01   | The main area MUST host a fully interactive terminal (PTY) running the CLI session. |
| T-02   | Only one session terminal is visible at a time; switching tabs swaps the visible terminal. |
| T-03   | Background sessions MUST remain running even when not visible. |
| T-04   | The terminal MUST support standard input/output, ANSI colors, and resize. |
| T-05   | If a session's process exits unexpectedly (non-zero exit code), the tab MUST show an error indicator and the terminal area MUST display an error message with a "Restart" button that re-runs the original shell invocation. |
| T-06   | On app launch, previously open sessions MUST be automatically restored by re-running their original shell invocations in sidebar order. |

### 5.4 Instruction Sets

| ID     | Requirement |
|--------|-------------|
| I-01   | The app MAY allow users to define instruction sets (plain text files) stored in a configurable directory on disk. Instruction sets are an opt-in overlay; sessions are fully usable without one. |
| I-02   | Each tool (Claude / Copilot) MAY have a default instruction set. When no default is configured, sessions launch without one and the CLI's `cwd`-based auto-discovery (see I-04) is the sole source of repo guidance. |
| I-03   | Instruction sets MAY be attached at session-creation time via Settings; the per-session new-session wizard does not prompt for one. |
| I-04   | When an instruction set is attached, instructions are passed to the CLI at launch using tool-appropriate mechanisms: for Claude, `--system-prompt <file>` passes a composed file containing the worktree context block and the user's instruction set; for Copilot the selected set is currently ignored at the CLI surface (the field is persisted only). When no instruction set is attached, Claude is launched as bare `claude` with no `--system-prompt`. Copilot is always launched bare (the modern `copilot` CLI starts in interactive mode by default; the legacy `--interactive <string>` flag was removed and now produces "too many arguments"). In both with-set and without-set cases, repo-level instructions are still auto-discovered from the worktree `cwd`: Claude reads `CLAUDE.md`, and Copilot reads `.github/copilot-instructions.md` (which is why `--instructions` is deliberately omitted so auto-discovery is preserved). |

### 5.5 Worktree Discovery

| ID     | Requirement |
|--------|-------------|
| W-01   | The app MUST anchor its worktree discovery on a single configurable `workspaceRoot` repository path. (`worktreeRoots` is retained as a legacy field for forward compatibility but is no longer the primary surface; see DESIGN §3 and `WORKTREES.md`.) |
| W-02   | Detected worktrees from configured root repos SHOULD be presented as a quick-pick list during session creation. |
| W-03   | The user MUST always be able to pick any directory manually via an OS file picker, regardless of W-01. |

### 5.6 Session Launch Composition

| ID     | Requirement |
|--------|-------------|
| L-01   | Session launch MUST execute the configured CLI command in the selected worktree directory using process `cwd` (not by interpolating `cd <path>` into the shell string). |
| L-02   | Session restart and restore MUST reuse each session's persisted composed command verbatim. |
| L-03   | One-shot setup commands are out-of-band from session launch and are governed by C-06 (`AppConfig.worktreePrepCommands` via `worktree_create`). |

### 5.7 Custom Processes & Sub-Tabs

| ID     | Requirement |
|--------|-------------|
| CP-01  | The user MUST be able to right-click a session tab (or invoke Shift+F10 / the Apps key with the tab focused) to open a context menu. |
| CP-02  | The context menu's "Launch…" submenu MUST list every enabled `CustomProcessDef` from `AppConfig.customProcesses`. Disabled defs MUST NOT appear. |
| CP-03  | Selecting a def MUST spawn a sub-tab (a `SubSession`) attached to the right-clicked session as its parent. The sub-tab MUST render indented under its parent in the sidebar. |
| CP-04  | A `terminal` sub-tab MUST host its own PTY (cwd = parent's worktree path) and render in `xterm.js` exactly like a top-level session when active. |
| CP-05  | An `application` sub-tab MUST spawn its program detached so closing Arborist does not kill it. Clicking the sub-tab MUST attempt to focus the program's OS window without changing the visible terminal viewport. Closing an `application` sub-tab MUST NOT terminate the external program — it only drops Arborist's tracking. |
| CP-06  | Sub-tabs MUST persist across app restarts. Terminal sub-tabs MUST respawn fresh on next launch; application sub-tabs MUST come back greyed (status `exited`) and re-launch on user click. |
| CP-07  | Closing a parent session MUST cascade: terminal sub-sessions are killed, application sub-sessions are merely detached. A sub-session whose PTY kill fails MUST be left visible in an error state rather than silently leaked. |
| CP-08  | The Settings dialog MUST expose a "Custom Processes" tab that allows the user to add, edit, enable/disable, and delete `CustomProcessDef` entries. The same validation rules MUST apply on the frontend and at the `config_set` boundary. |
| CP-09  | On fresh install (and additively at v3→v4 migration), the app MUST seed three built-in defs: `shell` (terminal, platform shell), `open-folder` (application, OS file browser), and `vscode` (application, `code .`; auto-disabled if `code` is not on `PATH`). Built-in defs MUST be user-editable and user-deletable. |
| CP-10  | A `CustomProcessDef.command` MUST be passed to the platform shell with the parent's worktree path as `cwd`; it MUST NOT be interpolated into the command string (DESIGN §8.1). |
| CP-11  | Window-focus on Linux MAY require the optional system dependency `wmctrl`. Its absence MUST degrade gracefully (logged warning, no error toast). Wayland sessions MUST report `Unsupported` rather than attempting an X11-only call. |

## 6. Non-Functional Requirements

| ID     | Requirement |
|--------|-------------|
| NF-01  | The app MUST run on Windows, macOS, and Linux. |
| NF-02  | Terminal latency MUST be imperceptible (<50 ms input-to-echo). |
| NF-03  | The app SHOULD start in under 2 seconds on modern hardware. |
| NF-04  | Session state (open tabs, worktree paths, tab order) MUST persist across app restarts. |
| NF-05  | The app MUST NOT store or transmit credentials; it delegates auth to the underlying CLI tools. |
| NF-06  | Sidebar tabs MUST be keyboard-navigable; tool icons MUST have accessible labels for screen readers. |
| NF-07  | File paths from user input or config MUST be canonicalized before use. Instruction file paths MUST be confirmed to lie within `instructionSetsDir`; worktree paths MUST be confirmed to exist as directories. |
| NF-08  | Shell commands MUST be constructed from validated config values only. Dynamic values inserted into shell command strings (paths, context strings) MUST be properly shell-quoted to handle spaces and special characters correctly. |
| NF-09  | Temporary files created by the app MUST be deleted when the associated session closes, and orphaned temp files from a previous crash MUST be cleaned up on next app startup. |
| NF-10  | Each running Arborist process MUST hold an exclusive advisory lock on its bound (branch, workspace) pair for the lifetime of the process, so concurrent instances cannot silently clobber each other's `config.json` or `sessions.json`. The lock failing to acquire at boot MUST cause a non-zero exit with a user-visible diagnostic; an in-app workspace switch MUST surface lock-contention as a hard error and leave the previously-bound workspace intact (DESIGN §5.5c). |

## 7. Out of Scope (v1)

- Built-in chat UI — the app delegates all interaction to the CLI tools in a terminal.
- Remote/SSH worktree support.
- Plugin/extension system.
- Multi-window support.
- In-app editing of instruction set files (users manage these as plain text files on disk).

## 8. Success Metrics

| Metric | Target |
|--------|--------|
| Time from app launch to first CLI prompt | < 5 seconds |
| Session switch latency (tab click to terminal visible) | < 200 ms |
| Crash-free session rate | > 99.5 % |

## 9. Open Questions

1. Should the app auto-detect worktrees from a configured root repo, or always prompt with a directory picker? _(Partially resolved: both are supported — see §5.5.)_
2. ~~What is the exact CLI flag syntax for passing instructions to each tool?~~ _Resolved: Claude uses `--system-prompt <composed-temp-file>` (only when an instruction set is attached); Copilot is launched bare with no flags so that `.github/copilot-instructions.md` is auto-discovered from `cwd`. See §5.4 I-04 and DESIGN §5.6._
