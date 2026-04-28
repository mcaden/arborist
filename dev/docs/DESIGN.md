# Grove — Design Document
_Version 0.5_

## 1. Technology Stack

| Layer | Choice | Rationale |
|-------|--------|-----------|
| Framework | **Tauri v2 (Rust)** | Cross-platform desktop app with a tiny binary (~5–15 MB), native OS WebView, memory-safe Rust backend. No bundled Chromium. |
| UI | **React + TypeScript** | Component-driven, strong typing, large community. Runs inside the OS WebView. |
| Terminal emulator | **xterm.js** | De-facto standard for web/desktop terminal rendering. Works in any WebView. |
| PTY backend | **portable-pty** (Rust crate) | Unified cross-platform PTY API (Windows ConPTY + Unix PTY) from the WezTerm project. Production-proven. |
| State management | **Zustand** | Lightweight, minimal boilerplate, good for session state. |
| Styling | **Tailwind CSS** | Utility-first; fast iteration for layout-heavy UI. |
| Build / bundle | **Vite + Tauri CLI** | Fast HMR for the frontend; Cargo for the Rust backend. |
| Persistence | **tauri-plugin-store** | JSON file-backed store exposed to the frontend via Tauri commands. |

## 2. Architecture Overview

```
┌─────────────────────────────────────────────────────┐
│                   Tauri Rust Backend                │
│  ┌───────────┐  ┌──────────────┐  ┌──────────────┐  │
│  │ PTY Pool  │  │ Config Store │  │   Commands   │  │
│  └───────────┘  └──────────────┘  └──────────────┘  │
│        │           (tauri-plugin-store)      │        │
│        │            Tauri Commands / Events  │        │
├────────┼──────────────────────────────────────┼──────┤
│        ▼            OS WebView               ▼       │
│  ┌──────────────────────────────────────────────┐    │
│  │                React App                      │    │
│  │  ┌──────────┐  ┌─────────────────────────┐   │    │
│  │  │ Sidebar  │  │   Terminal Viewport      │   │    │
│  │  │ (Tabs)   │  │   (xterm.js instances)   │   │    │
│  │  └──────────┘  └─────────────────────────┘   │    │
│  └──────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────┘
```

### 2.1 Rust Backend Responsibilities

- **PTY Pool**: Manages `portable-pty` instances. One PTY per session. Responsible for composing the session's shell invocation (pre-launch commands joined with `&&` followed by the CLI launch command), spawning the PTY with that composed invocation in the worktree directory, handling resize and data relay, restarting on demand, and kill. PTY output is streamed to the frontend via Tauri events.
- **Config Store**: Reads/writes settings and session state to disk via `tauri-plugin-store`.
- **Commands**: Exposes a typed, capability-gated API to the frontend via Tauri `#[command]` handlers. Replaces the Electron `contextBridge` / `ipcMain` pattern.

### 2.2 Frontend Responsibilities

- **Sidebar component**: Renders vertical tabs, handles new-session flow, tab switching.
- **Terminal Viewport**: Hosts `xterm.js` Terminal instances. Only the active session's terminal is attached to the DOM; others are detached but keep their PTY alive in the Rust backend.
- **`use-terminal` hook**: Encapsulates the lifecycle of an `xterm.js` Terminal instance — initialization, attachment/detachment from the DOM, resize observation via `ResizeObserver`, and binding/unbinding of the PTY data stream via Tauri events.

## 3. Data Model

The data model is defined in Rust (serialized via `serde`) and mirrored as TypeScript types in the frontend.

Two distinct types are used for Session: the full `Session` (Rust-internal, persisted to the store) and the `SessionView` (sent to the frontend). `composedCommand` and `tempFiles` are backend-only — the frontend has no UI use for the raw shell command string or the on-disk temp-file specs and does not need them.

### 3.1 Session (Rust-internal / persisted)

```rust
struct Session {
    id: String,                  // UUID
    tool: Tool,                  // Claude | Copilot
    worktree_path: PathBuf,      // Canonicalized absolute path to the worktree directory
    worktree_name: String,       // Display name (basename of path)
    label: String,               // Tab label (worktreeName, or "worktreeName N" for duplicates)
    instruction_set_id: String,  // Reference to an InstructionSet
    composed_command: String,    // Full shell command string — backend-only; used for restart
    status: SessionStatus,       // Starting | Running | Exited | Error
    pid: Option<u32>,            // OS PID of the PTY process; cleared on exit
    created_at: i64,             // Unix timestamp
    tab_index: usize,            // Display order in the sidebar
    temp_files: Vec<TempFileSpec>, // Backend-only; on-disk artefacts the session owns
                                   // (e.g. Claude's --system-prompt file). Persisted so
                                   // respawn_existing can rematerialise them after a
                                   // crash/restart. Omitted from SessionView.
}

struct TempFileSpec {
    path: PathBuf,
    contents: String,
}
```

### 3.1.1 SessionView (sent to frontend)

`composedCommand` and `tempFiles` are intentionally omitted. The frontend receives only what it needs to render the UI.

```typescript
interface SessionView {
  id: string;
  tool: 'claude' | 'copilot';
  worktreePath: string;
  worktreeName: string;
  label: string;
  instructionSetId: string;
  status: 'starting' | 'running' | 'exited' | 'error';
  pid?: number;
  createdAt: number;
  tabIndex: number;
}
```

### 3.2 InstructionSet

```typescript
interface InstructionSet {
  id: string;
  name: string;
  tool: 'claude' | 'copilot';
  filePath: string;   // Path to the instructions file on disk
  isDefault: boolean;
}
```

### 3.3 AppConfig

```typescript
interface AppConfig {
  configVersion: number; // On-disk schema version (currently 1; bumped on breaking changes)
  defaultInstructionSets: {
    claude: string; // InstructionSet ID
    copilot: string; // InstructionSet ID
  };
  instructionSetsDir: string; // Path to directory containing instruction files
  worktreeRoots: string[]; // Root repo paths to scan for Git worktrees
  prelaunchCommands: string[]; // Global commands run before CLI launch
  worktreePrelaunchCommands: Record<string, string[]>; // Per-worktree overrides (key = worktree path)
  lastOpenSessions: string[]; // Session IDs to restore on next launch
  tabOrder: string[]; // Session IDs in sidebar display order
}
```

`AppConfig` lives in `<app-data>/config.json`. A separate
`<app-data>/sessions.json` file holds the full `Session` records; the path
discipline, atomic-write semantics, and quarantine behaviour for both files
are documented in `dev/docs/CONFIGURATION.md`.

## 4. Component Hierarchy

```
<App>
  <Sidebar>
    <SidebarTab />       // One per session (icon + worktree name)
    <SidebarTab />
    <NewSessionButton />
  </Sidebar>
  <MainArea>
    <TerminalView />     // Active session's xterm.js instance
  </MainArea>
  <NewSessionDialog />   // Modal: tool picker → worktree picker → confirm
</App>
```

## 5. Key Flows

### 5.1 Create Session

```
User clicks [+]
  → NewSessionDialog opens
  → User selects tool (Claude | Copilot)
  → User selects worktree (from detected worktrees list or OS file picker)
  → User optionally selects instruction set (default pre-selected)
  → Frontend invokes Tauri command: session_create { tool, worktreePath, instructionSetId }
  → Rust backend:
      1. Creates Session record; assigns label — appends " N" suffix if a session
         with the same tool + worktreePath already exists (e.g., "my-feature 2")
      2. Resolves user's default shell and platform shell flag:
           macOS / Linux: $SHELL,   flag "-c"
           Windows:       %COMSPEC%, flag "/c"
      3. Merges global prelaunchCommands with any worktree-specific overrides
      4. Builds CLI launch command (see §5.6 — CLI Launch Commands)
      5. Composes command string: [...prelaunchCmds, cliCmd].join(' && ')
      6. Stores composedCommand on Session record (used for restart)
      7. Spawns PTY via portable-pty:
           PtySize { cols, rows }
           Command { program: shell, args: [shellFlag, composedCommand], cwd: worktreePath }
      8. Records pid on Session; sets status → 'starting'
      9. Spawns read thread: forwards PTY output to frontend via
           Tauri event: session://output { sessionId, data }
           The read thread applies backpressure: if the pending event queue
           exceeds a threshold (default: 512 events), output is buffered and
           flushed at a capped rate to prevent memory exhaustion from runaway
           CLI processes.
     10. Returns SessionView to frontend (composedCommand excluded)
  → Frontend:
      1. Creates xterm.js Terminal, subscribes to session://output events for this sessionId
      2. Adds SidebarTab for new session
      3. Switches active view to new terminal
```

### 5.2 Switch Session

```
User clicks a SidebarTab
  → Frontend detaches current xterm instance from DOM (keeps buffer)
  → Frontend attaches target session's xterm instance to DOM
  → Frontend invokes Tauri command: session_focus { sessionId }
  → Rust backend records active session (for restore-on-restart)
```

### 5.3 Close Session

```
User clicks [x] on a SidebarTab
  → Frontend shows confirmation dialog: "Terminate session '<label>'?"
  → User confirms
  → Frontend invokes Tauri command: session_close { sessionId }
  → Rust backend:
      1. Sends SIGTERM to PTY child process
      2. After grace period, sends SIGKILL if still alive
      3. Drops PTY handle; removes Session record from store
  → Frontend:
      1. Destroys xterm.js Terminal
      2. Removes SidebarTab
      3. Switches to adjacent session (or shows empty state)
```

### 5.4 Session Error & Restart

```
PTY process exits with non-zero code
  → Rust backend emits Tauri event: session://status { sessionId, status: 'error' }
  → Frontend:
      1. Marks tab with error indicator (e.g., red dot)
      2. Displays error overlay in terminal area with a "Restart" button

User clicks "Restart"
  → Frontend invokes Tauri command: session_restart { sessionId }
  → Rust backend:
      1. Re-spawns PTY using stored Session.composedCommand and Session.worktreePath
      2. Records new pid on Session; sets status → 'starting'
      3. Emits Tauri event: session://status { sessionId, status: 'starting' }
  → Frontend clears error overlay, re-attaches xterm.js to new PTY stream
```

### 5.5 Session Restore on Launch

```
App starts
  → Rust backend reads AppConfig.lastOpenSessions and AppConfig.tabOrder from store
  → For each session ID (in tabOrder order), re-runs Create Session flow
    using the stored Session.composedCommand and Session.worktreePath
  → First session in tabOrder becomes the active session
```

### 5.6 CLI Launch Commands

Grove builds the CLI portion of the composed command differently per tool.
The instruction set file path is always resolved to an absolute, canonicalized path
before use (Rust `std::fs::canonicalize()`), which resolves symlinks and prevents
directory-traversal via symlink attacks.

**Shell argument quoting**: All dynamic values inserted into the composed command
string (file paths, the `-i` context string) MUST be shell-quoted before
interpolation using a proper quoting function — not simple string wrapping — to
handle paths with spaces, quotes, or special characters on all platforms.
The worktree `cwd` is ALWAYS passed as a discrete `cwd` field to `portable-pty`,
never interpolated into the command string.

#### Claude

Claude automatically reads `CLAUDE.md` from the `cwd` (worktree) and walks up the
directory tree — this happens regardless of any flags and cannot be disabled. The
`--system-prompt` flag injects an *additional* system prompt on top of `CLAUDE.md`;
the two coexist without conflict.

Grove uses `--system-prompt` to pass the user's selected instruction set file
and a worktree context block. The context block is prepended to the instruction set
content and written to a session-scoped temp file at session creation time:

```
Context block  (generated by Grove — worktree label + path)
---
Instruction set file contents  (user's selected .md file)
```

The composed temp file is deleted when the session is closed.

```
claude --system-prompt <temp-instruction-file>
```

`CLAUDE.md` (repo instructions) is loaded automatically by Claude from the worktree
`cwd` — Grove does not need to handle it.

#### Copilot

Copilot automatically reads `.github/copilot-instructions.md` from the repo root
when no `--instructions` flag is provided. Grove deliberately omits
`--instructions` so that auto-discovery from `cwd` (the worktree) is preserved.

Worktree context is injected using the `-i` / `--interactive` flag, which starts an
interactive session and automatically sends the supplied string as the opening prompt
before the user types anything:

```
copilot --interactive "You are operating in Git worktree **<label>** at <worktreePath>."
```

The context prompt appears in the conversation timeline as Copilot's first exchange,
confirming to the user that context was received. No temp files are created.

| | Claude | Copilot |
|---|---|---|
| Repo instructions | Auto-loaded from `cwd` (`CLAUDE.md`) | Auto-loaded from `cwd` (`.github/copilot-instructions.md`) |
| Worktree context | `--system-prompt <temp-file>` | `--interactive "<context string>"` |
| Instruction set | Included in temp file | Not passed — Copilot uses its own instruction discovery |
| Temp file | Yes — cleaned up on session close | No |

## 6. Tauri Command & Event API

Frontend ↔ backend communication uses Tauri's typed command/event system. Commands
are invoked from the frontend via `invoke()`; events are pushed from the Rust backend
via `app_handle.emit()` and received in the frontend via `listen()`.

All commands are gated by Tauri capability declarations in `capabilities/main.json`.

### Commands (Frontend → Rust)

| Command | Payload | Return | Description |
|---------|---------|--------|-------------|
| `session_create` | `{ tool, worktreePath, instructionSetId }` | `Session` | Compose and spawn a new session |
| `session_list` | — | `Session[]` | Return all current Session records |
| `session_close` | `{ sessionId }` | — | Terminate a session (after UI confirmation) |
| `session_focus` | `{ sessionId }` | — | Mark session as active |
| `session_resize` | `{ sessionId, cols, rows }` | — | Resize PTY |
| `session_input` | `{ sessionId, data }` | — | Send keystrokes to PTY |
| `session_restart` | `{ sessionId }` | — | Re-spawn a session using its stored composedCommand |
| `config_get` | — | `AppConfig` | Retrieve AppConfig |
| `config_set` | `Partial<AppConfig>` | — | Update AppConfig |
| `instructions_list` | — | `InstructionSet[]` | List available instruction sets from `instructionSetsDir` |

### Events (Rust → Frontend)

| Event | Payload | Description |
|-------|---------|-------------|
| `session://output` | `{ sessionId, data: string }` | Stream PTY output to xterm.js |
| `session://status` | `{ sessionId, status }` | Notify session state changes (including `'error'`) |

## 7. Directory Structure (Proposed)

```
grove/
├── dev/
│   └── docs/
│       ├── SPEC.md
│       └── DESIGN.md
├── src/                          # React frontend (Vite)
│   ├── main.tsx
│   ├── App.tsx
│   ├── components/
│   │   ├── Sidebar.tsx
│   │   ├── SidebarTab.tsx
│   │   ├── NewSessionButton.tsx
│   │   ├── NewSessionDialog.tsx
│   │   ├── MainArea.tsx
│   │   └── TerminalView.tsx
│   ├── store/
│   │   └── session-store.ts
│   ├── hooks/
│   │   └── use-terminal.ts
│   ├── lib/
│   │   └── tauri-bridge.ts       # Typed wrappers around invoke() and listen()
│   └── assets/
│       ├── claude-icon.svg
│       └── copilot-icon.svg
├── src-tauri/                    # Rust backend (Cargo)
│   ├── src/
│   │   ├── main.rs
│   │   ├── pty_pool.rs           # portable-pty session management
│   │   ├── config_store.rs       # tauri-plugin-store wrapper
│   │   ├── commands.rs           # #[tauri::command] handlers
│   │   └── types.rs              # Session, InstructionSet, AppConfig (serde)
│   ├── capabilities/
│   │   └── main.json             # Tauri capability declarations
│   ├── Cargo.toml
│   └── tauri.conf.json
├── instructions/                 # Default instruction set files
│   ├── claude-default.md
│   └── copilot-default.md
├── package.json
├── tsconfig.json
└── vite.config.ts
```

## 8. Security Considerations

Grove is a local, single-user desktop application. It makes no inbound or
outbound network connections and is not exposed to untrusted content. The primary
concerns are (a) bugs in Grove's own command-building logic causing unintended
shell execution, and (b) robustness against bad input and resource exhaustion.

### 8.1 Shell Command Correctness

The most consequential class of bug would be Grove constructing a malformed
shell command that runs something unintended on the user's machine.

- **Shell argument quoting**: Dynamic values inserted into the composed command string
  (instruction file paths, the Copilot `-i` context string) must be properly
  shell-quoted — not simply wrapped in double quotes — to handle paths with spaces,
  single quotes, or backslashes correctly on all platforms.
- **Config-only sources**: Pre-launch commands and CLI launch arguments are built
  exclusively from validated config values. No free-form user input from the UI is
  interpolated into a shell command.
- **`prelaunchCommands` execute as the user**: Users who configure `prelaunchCommands`
  are intentionally running shell commands. The session creation dialog displays the
  active pre-launch commands so users can review them before confirming.

### 8.2 Path & File Robustness

- **Path canonicalization**: Worktree and instruction file paths are resolved via
  `std::fs::canonicalize()` before use, normalizing `..` components and resolving
  symlinks. This prevents bugs where relative or indirect paths cause files to be read
  from unexpected locations.
- **Instruction file confinement**: After canonicalization, instruction file paths must
  lie within `instructionSetsDir`. Paths outside are rejected.
- **Worktree validation**: Worktree paths must exist and be directories before a session
  is created or restored. Stale paths (e.g., from a deleted worktree) produce a clear
  error rather than a confusing PTY failure.
- **Instruction file size cap**: Instruction files are capped at 1 MB to prevent
  accidental exhaustion from an unexpectedly large file.
- **Temp file cleanup (Claude)**: Session-scoped temp instruction files are deleted on
  session close. On startup, orphaned temp files from a previous crash are cleaned up.

### 8.3 Resource Management

- **PTY output backpressure**: The PTY read thread streams bytes through a bounded
  `tokio::sync::mpsc` channel (capacity: 512 chunks, see
  `pty_pool::OUTPUT_CHANNEL_CAPACITY`). When the channel is full, the read thread
  **drops the new chunk** (newest-first) and increments a per-session counter; a warning
  is logged every 256 drops. This prevents memory exhaustion if a CLI process produces
  runaway output (e.g., a verbose build or a `yes` loop).
- **ANSI reset after a drop**: Because dropping a partial output chunk could leave
  xterm.js mid-escape-sequence, the read thread prepends `ESC c` (full terminal reset)
  to the **next** successfully-sent chunk after a drop. Tests assert this in
  `tests/pty_pool.rs::backpressure_drops_chunks_and_inserts_reset_after_drain`.
- **Streaming UTF-8 decode**: PTY bytes are decoded incrementally, holding at most the
  three trailing bytes of a partial multibyte sequence between reads, so a multibyte
  scalar split across two `read()` calls is never truncated or replaced.
- **Orphan temp-dir cleanup**: On startup, `pty_pool::cleanup_orphans` scans
  `<os-temp>/grove/<uuid>/` and deletes any UUID-named directory whose UUID is not in
  the persisted session list **and** whose mtime is older than 1 hour
  (`pty_pool::ORPHAN_AGE_THRESHOLD`). The age threshold prevents racing against an
  in-flight session whose temp file was just written; the persisted-set check makes
  cleanup restore-safe.
- **Scrollback cap**: xterm.js is configured with a `scrollback` limit (default:
  5 000 lines) to bound per-session memory in the frontend.

### 8.4 Credential Handling

Grove stores no credentials. Authentication is managed entirely by the CLI tools
(`claude`, `copilot`) via their own credential stores. The app neither reads nor writes
tokens, API keys, or passwords.

## 9. Future Considerations

- **Split view**: Show two terminals side by side.
- **Worktree auto-discovery**: Deeper scanning (e.g., recursive search, multiple remotes). _(Basic discovery via `worktreeRoots` is included in v1.)_
- **Session snapshots**: Save/restore terminal scrollback.
- **Theming**: Respect system dark/light mode and allow custom terminal themes.
- **Keyboard shortcuts**: Ctrl+Tab to cycle sessions, Ctrl+N for new session.
- **In-app instruction set editor**: Create and edit instruction files without leaving the app.
