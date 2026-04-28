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
| C-03   | After worktree selection, the app MUST compose a single shell invocation that runs configured pre-launch commands followed by the CLI launch command, all within the worktree directory, and open a new terminal in the main area executing that invocation. |
| C-04   | The instruction set used MUST be configurable per-tool (see §5.4). |
| C-05   | Multiple sessions for the same tool and worktree MUST be allowed. The new session's tab label MUST append a numeric suffix to disambiguate (e.g., "my-feature 2", "my-feature 3"). |

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
| I-01   | The app MUST allow users to define instruction sets (plain text files) stored in a configurable directory on disk. |
| I-02   | Each tool (Claude / Copilot) MUST have a default instruction set. |
| I-03   | Instruction sets SHOULD be selectable at session-creation time (with the default pre-selected). |
| I-04   | Instructions are passed to the CLI at launch using tool-appropriate mechanisms: for Claude, `--system-prompt <file>` passes a composed file containing the worktree context block and the user's instruction set (Claude auto-loads `CLAUDE.md` from the worktree `cwd` separately); for Copilot, `--instructions` is deliberately omitted so that `.github/copilot-instructions.md` is auto-discovered from the worktree `cwd`, and the worktree context is injected via `--interactive "<context string>"` as the opening prompt. |

### 5.5 Worktree Discovery

| ID     | Requirement |
|--------|-------------|
| W-01   | The app MUST support a configurable list of root repository paths (`worktreeRoots`) to scan for Git worktrees. |
| W-02   | Detected worktrees from configured root repos SHOULD be presented as a quick-pick list during session creation. |
| W-03   | The user MUST always be able to pick any directory manually via an OS file picker, regardless of W-01. |

### 5.6 Shell Commands at Launch

| ID     | Requirement |
|--------|-------------|
| L-01   | Before launching the CLI, the app MUST compose a single shell invocation consisting of a configurable list of commands (e.g., `nvm use`, `git status`, environment setup) followed by the CLI launch command, all run within the worktree directory. |
| L-02   | The command list SHOULD be configurable globally and overridable per-worktree. |
| L-03   | All commands in the invocation MUST be joined with `&&` so that a failing command halts the sequence and the session enters an error state. |

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
2. ~~What is the exact CLI flag syntax for passing instructions to each tool?~~ _Resolved: Claude uses `--system-prompt <composed-temp-file>`; Copilot uses `--interactive "<context>"` with no `--instructions` flag so repo instructions are auto-discovered from `cwd`. See §5.4 I-04 and DESIGN §5.6._
