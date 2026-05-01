# Arborist — Design Document
_Version 0.6_

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
| OS dialogs | **tauri-plugin-dialog** | Native file/directory picker; gated by the `dialog:allow-open` capability. Used by the New-Session flow's manual "Browse…" fallback (Phase 10). |

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
    instruction_set_id: Option<String>,  // Optional reference to an InstructionSet. None means
                                         // the session was launched without a user-curated overlay
                                         // (the CLI still auto-discovers repo instructions from cwd).
    composed_command: String,    // Full shell command string — backend-only; used for restart
    status: SessionStatus,       // Starting | Running | Exited | Error
    pid: Option<u32>,            // OS PID of the PTY process; cleared on exit
    created_at: i64,             // Unix timestamp
    tab_index: usize,            // Display order in the sidebar
    temp_files: Vec<TempFileSpec>, // Backend-only; on-disk artefacts the session owns
                                   // (e.g. Claude's --system-prompt file). Persisted so
                                   // respawn_existing can rematerialise them after a
                                   // crash/restart. Omitted from SessionView.
    ai_session_id: Option<String>, // Backend-only; the underlying CLI's session id
                                   // (Claude's JSONL stem; Copilot's gen_ai.conversation.id).
                                   // Discovered by the metrics watcher post-spawn and
                                   // persisted so restore_all_sessions can append
                                   // `--resume <id>` and continue the conversation
                                   // across an app restart. Cleared on session_create.
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
  instructionSetId?: string;
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
  configVersion: number; // On-disk schema version (currently 4; bumped on breaking changes)
  defaultInstructionSets: {
    claude: string; // InstructionSet ID
    copilot: string; // InstructionSet ID
  };
  instructionSetsDir: string; // Path to directory containing instruction files
  workspaceRoot: string | null; // Single anchor repo (Roadmap §1); takes precedence over worktreeRoots
  worktreeRoots: string[]; // Legacy: additional repo roots to scan (kept for forward compatibility)
  prelaunchCommands: string[]; // Global commands run before CLI launch
  worktreePrelaunchCommands: Record<string, string[]>; // Per-worktree overrides (key = worktree path)
  lastOpenSessions: string[]; // Session IDs to restore on next launch
  tabOrder: string[]; // Session IDs in sidebar display order
  activeSessionId: string | null; // Focused session at last shutdown (restored on launch)
  aiLaunchCommands: { claude: string; copilot: string }; // Per-agent CLI launch overrides ('' = built-in default)
  customProcesses: CustomProcessDef[]; // User-defined launchers exposed in the tab context menu (§3.4)
  lastOpenSubSessions: SubSessionRecord[]; // Sub-tabs to restore on next launch (§3.4)
}
```

Schema version history (`configVersion`):

- `1` — initial release.
- `2` — added `activeSessionId`.
- `3` — added `workspaceRoot` (single-workspace model, Roadmap §1).
- `4` — added `customProcesses` and `lastOpenSubSessions` (custom-process /
  sub-tab feature, §3.4). Migration seeds the built-in `shell`,
  `open-folder`, and `vscode` defs **additively** — only IDs not already
  present are inserted, so user edits to a built-in def are never
  overwritten. Future versions (`> CONFIG_VERSION_CURRENT`) are quarantined
  on load and replaced with defaults to protect downgrade scenarios.

`AppConfig` lives in `<app-data>/config.json`. A separate
`<app-data>/sessions.json` file holds the full `Session` records; the path
discipline, atomic-write semantics, and quarantine behaviour for both files
are documented in `dev/docs/CONFIGURATION.md`.

### 3.4 Custom Processes & Sub-Sessions

Custom processes are user-defined launchers exposed by right-clicking a
session tab. Two flavours, distinguished by `kind`:

- **`terminal`** — a PTY child hosted in-app. Reuses the same `portable-pty`
  machinery as top-level sessions (in a parallel `SubPtyPool`). The sub-tab
  renders an `xterm.js` viewport just like a session.
- **`application`** — an external GUI program spawned **detached** from
  Arborist. The sub-tab tracks only the OS PID; clicking it focuses the
  program's window via the platform-specific `WindowFocuser` (see §5.7.4).
  Closing the sub-tab does **not** kill the external app — closing
  Arborist's tab must not terminate the user's editor / file browser.

```typescript
type CustomProcessKind = 'terminal' | 'application';

interface CustomProcessDef {
  id: string;            // Slug matching [A-Za-z0-9_-]+; user-editable for new rows, locked once persisted
  name: string;          // User-facing label (shown in the Launch submenu and on the sub-tab)
  kind: CustomProcessKind;
  command: string;       // Composed once at sub-session creation; passed verbatim to $SHELL -c (or %COMSPEC% /c)
  enabled: boolean;      // false hides the def from the Launch submenu (existing sub-sessions keep running)
  icon?: string;         // Optional UI hint (reserved; v1 sidebar renders a generic icon)
}

type SubSessionStatus = 'starting' | 'running' | 'exited' | 'error';

interface SubSession {
  id: string;            // UUID v4; distinct type (SubSessionId) from SessionId at the Rust level
  parentSessionId: string;
  defId: string;
  kind: CustomProcessKind;
  label: string;
  status: SubSessionStatus;
  pid?: number;          // Cleared on exit
  composedCommand: string; // Captured at creation time; reused verbatim on relaunch (§5.4 mirror)
  createdAt: number;     // Unix epoch (whole seconds)
}

interface SubSessionRecord {  // Lightweight restore record persisted in lastOpenSubSessions
  id: string;
  parentSessionId: string;
  defId: string;
  kind: CustomProcessKind;
  label: string;
  composedCommand: string;
}
```

**Default seeded defs** (inserted on fresh install and additively at v3→v4
migration):

| id            | name        | kind        | command                                  | enabled by default |
|---------------|-------------|-------------|------------------------------------------|--------------------|
| `shell`       | Shell       | terminal    | `$SHELL -i` on Unix, `%COMSPEC%` on Windows | true |
| `open-folder` | Open Folder | application | `xdg-open .` / `open .` / `explorer .`   | true |
| `vscode`      | VS Code     | application | `code .`                                  | auto: true if `code` is on `PATH` at seed time, else false |

`default_shell_command` rejects suspicious `$SHELL` values (relative paths,
embedded shell metacharacters/whitespace) and falls back to `sh -i`.
`command_on_path` requires the executable bit on Unix (`mode & 0o111`); on
Windows it accepts the `.exe`/`.cmd`/`.bat` suffix without a bit check.

Built-in defs are user-editable and user-deletable just like custom ones;
nothing special-cases them after seeding.

**Validation** (`config_store::validate_custom_processes`, enforced at the
`config_set` boundary; the Settings tab mirrors the same rules):

- `id` matches `^[A-Za-z0-9_-]+$` and is unique within the list.
- `name` and `command` non-empty after trim.
- `kind` is `terminal` or `application`.

A weaker `sanitize_loaded_custom_processes` runs on load: invalid
persisted defs (empty/garbage id, blank command, duplicate id) are dropped
with a warn log rather than poisoning the whole config.

## 4. Component Hierarchy

```
<App>
  <Sidebar>
    <SidebarTab />          // One per session (icon + worktree name)
      <SidebarSubTab />     // Indented sub-tab (one per SubSession)
      <SidebarSubTab />
    <SidebarTab />
    <NewSessionButton />
  </Sidebar>
  <MainArea>
    <TerminalView />        // Active session's xterm.js instance, OR…
    <SubTerminalView />     // …active terminal sub-session's xterm.js instance
                            //   (application sub-tabs leave the previous viewport visible)
  </MainArea>
  <TabContextMenu />        // Right-click / Shift+F10 / Apps key on a SidebarTab
  <NewSessionDialog />      // Modal: tool picker → worktree picker → confirm
  <SettingsDialog>          // Tabbed: General / Custom Processes
    <GeneralTab />
    <CustomProcessesTab />  // CRUD over AppConfig.customProcesses
  </SettingsDialog>
</App>
```

## 5. Key Flows

### 5.1 Create Session

```
User clicks [+]
  → NewSessionDialog opens
  → User selects tool (Claude | Copilot)
  → User selects worktree (from detected worktrees list or OS file picker)
        ↳ Detected list comes from `worktrees_list { repoRoot }`, which runs
          through the injectable `GitRunner` seam (production: shells out
          to `git worktree list --porcelain`, parsed in
          `src-tauri/src/git.rs`). Discovery failures degrade to an empty
          list, so the manual "Browse…" button (powered by
          `tauri-plugin-dialog`) is always available.
  → User optionally selects instruction set (default pre-selected)
  → Frontend invokes Tauri command: session_create { tool, worktreePath } (instructionSetId is optional and currently never sent by the new-session wizard)
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

User-initiated restart deliberately starts a fresh AI conversation —
`Session.aiSessionId` is **not** appended on this path. The user clicked
"Restart" (not "Resume"); honouring that contract avoids surprising them
when a session has gotten into a bad state mid-conversation.

### 5.5 Session Restore on Launch

```
Tauri setup (backend)
  → initialise plugins, build PtyPool with the production spawner,
    run cleanup_orphans(), register managed state. Window opens.
  → (No sessions are spawned yet.)

App.tsx mounts (frontend)
  → configStore.hydrate()           (reads AppConfig from store)
  → sessionStore.hydrate()          (calls session_list — returns the
                                     persisted snapshot as-is, sorted by
                                     tabIndex; statuses come back exactly
                                     as last persisted, which may include
                                     a stale Running from a prior crash —
                                     restore_all_sessions resolves it
                                     below)
  → initTerminalRouter()            (attaches global session://output)
  → subscribeToStatus()             (attaches global session://status)
  → frontendReady()                 (one-shot CAS on the backend)
       └─ backend kicks off restore_all_sessions on a blocking thread:
           for each persisted Session in tabOrder order it
             1. (re-)materialises any persisted temp_files,
             2. flips the persisted status to Starting and emits
                session://status,
             3. calls pty_pool::respawn_existing using the stored
                composedCommand and worktreePath; if Session.aiSessionId
                is set and the underlying transcript still exists on disk
                (`~/.claude/projects/<encoded-cwd>/<id>.jsonl` for Claude,
                `~/.copilot/session-state/<id>/` for Copilot), the spawn
                command is *augmented* (not recomposed) by appending
                `--resume <quoted-id>` to the trailing CLI invocation so
                the AI conversation continues across the restart. The
                persisted Session.composedCommand is never mutated by
                this augmentation. The wait thread later flips the
                status to Running / Exited / Error as the child reports.
           Spawn or temp-file failures map per-session to Error and
           never abort the loop.
  → first session in tabOrder remains the active session
```

This ordering guarantees:

- The window is interactive within the SPEC NF-03 startup budget,
  regardless of how slow individual sessions are to start (restore is
  fire-and-forget from the frontend's perspective).
- `session://output` and `session://status` listeners are attached
  *before* any spawn happens, so early output and status events cannot
  be lost.
- Restore is driven by the Rust backend (`restore_all_sessions`), not by
  re-invoking `session_create` from the frontend — the frontend never
  has to reconstruct a `composedCommand`.


### 5.6 CLI Launch Commands

Arborist builds the CLI portion of the composed command differently per tool.
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

Arborist *optionally* uses `--system-prompt` to pass the user's selected
instruction set file alongside a worktree context block. The context block is
prepended to the instruction set content and written to a session-scoped temp
file at session creation time:

```
Context block  (generated by Arborist — worktree label + path)
---
Instruction set file contents  (user's selected .md file)
```

The composed temp file is deleted when the session is closed.

```
claude --system-prompt <temp-instruction-file>
```

When no instruction set is attached to the session (the default for sessions
created through the new-session wizard), Claude is launched as bare `claude`
with no `--system-prompt` and no temp file. The agent still receives the
worktree as its `cwd`, so `CLAUDE.md` is auto-discovered and the agent can
derive its location from `pwd`/`git`.

`CLAUDE.md` (repo instructions) is loaded automatically by Claude from the worktree
`cwd` regardless of whether `--system-prompt` is supplied — Arborist does not need
to handle it.

#### Copilot

Copilot automatically reads `.github/copilot-instructions.md` from the repo root
when no `--instructions` flag is provided. Arborist deliberately omits
`--instructions` so that auto-discovery from `cwd` (the worktree) is preserved.

The modern `copilot` CLI starts in interactive mode by default. The legacy
`--interactive <string>` flag was removed and now triggers a "too many
arguments" error from the CLI, so Arborist spawns Copilot bare:

```
copilot
```

No worktree context preamble is injected; the agent can derive its location
from `pwd`/`git` (the PTY pool sets `cwd` to the worktree). No temp files are
created at compose time.

##### Per-session telemetry env (Copilot only)

Arborist enables Copilot's OpenTelemetry **file exporter** at spawn time so
the sidebar can surface real-time token usage and context-window state. The
PTY pool injects three environment variables into the spawned `copilot`
process (additive — the child still inherits the parent's env; Arborist
never calls `env_clear`):

| Variable | Value | Purpose |
|---|---|---|
| `COPILOT_OTEL_FILE_EXPORTER_PATH` | `<session_temp_dir>/otel.jsonl` | Redirects OTel spans to a per-session JSONL file Arborist tails (`session_metrics::run_copilot_watcher`). |
| `COPILOT_OTEL_ENABLED` | `true` | Activates the exporter. |
| `OTEL_BSP_SCHEDULE_DELAY` | `1000` | Standard OTel SDK env var; tightens the batch span processor flush from 5s to ~1s so the sidebar updates feel live. |

These values are computed by `compose::env_for_tool(tool, &session_id)` and
**not** persisted on `Session` — they are derived from the session id (which
*is* persisted) at every spawn / restart / restore. This keeps
`Session.composed_command` clean (it remains the same shell string we'd
write into the user's terminal if asked) and avoids stale paths leaking
across upgrades.

The pool also creates `<session_temp_dir>` if missing and removes any stale
`otel.jsonl` from a previous run before spawning, so restart / restore-on-
launch don't replay old spans and double-count totals.

| | Claude | Copilot |
|---|---|---|
| Repo instructions | Auto-loaded from `cwd` (`CLAUDE.md`) | Auto-loaded from `cwd` (`.github/copilot-instructions.md`) |
| Worktree context | `--system-prompt <temp-file>` (only when an instruction set is attached) | Not injected — agent derives from `pwd`/`git` |
| Instruction set | Included in temp file when attached; otherwise launches as bare `claude` | Not passed — Copilot uses its own instruction discovery |
| Compose-time temp file | Only when an instruction set is attached; cleaned up on session close | No |
| Telemetry env injected at spawn | None (Claude transcripts under `~/.claude/projects/` are read directly) | Three OTel env vars enable the file exporter (see above) |

### 5.7 Custom Process Sub-Sessions

Sub-tabs are launched from the tab context menu's "Launch…" submenu, which
lists every enabled `CustomProcessDef` from `AppConfig.customProcesses`.

#### 5.7.1 Create

```
User right-clicks a SidebarTab → TabContextMenu opens
  → Selects an enabled def from the Launch submenu
    → Frontend invokes Tauri command: subsession_create { parentSessionId, defId }
      → Backend (subsession_create_impl):
          1. Refuses if the parent session is in the closing-parent tombstone (Phase 7).
          2. Loads the def by id; refuses if missing or disabled.
          3. Loads the parent Session; refuses with WorktreeMissing if the worktree path is gone.
          4. Composes once: composedCommand = def.command (captured-and-stored, mirroring §5.4).
          5. Inserts the SubSession into the in-memory SubSessionStore FIRST,
             then appends a SubSessionRecord to AppConfig.lastOpenSubSessions.
             On persist failure, rolls back the in-memory insert so a record can never be orphaned.
          6. Branches on kind:
             - terminal:   SubPtyPool.spawn_terminal(id, composedCommand, cwd=parent.worktreePath, sink)
             - application: AppPool.spawn(id, composedCommand, cwd=parent.worktreePath, sink)
          7. On spawn failure: removes the in-memory entry and prunes the persisted record.
      → Sink callbacks emit subsession://status (with PID once Running) and the in-memory
        store is updated by the production sink before the event is dispatched.
```

The composed command is **never** built by interpolating the worktree path
into the command string — `cwd` is always passed as a discrete argument to
the spawner (DESIGN §8.1).

#### 5.7.2 Focus

```
Frontend invokes Tauri command: subsession_focus { id }
  → terminal kind:    no-op on the backend; the frontend swaps the active
                      terminal viewport in MainArea.
  → application kind: WindowFocuser.focus_pid(pid). Returns NotApplicable if
                      the captured PID is a launcher wrapper that has already
                      exited (e.g. `code .` on PATH delegates to a daemon);
                      the frontend surfaces the error code without rolling
                      back the selection.
```

Focus delegation is best-effort. For launcher wrappers (`code .`,
`explorer .`) the captured PID is the wrapper, not the GUI window owner;
the wrapper may have exited cleanly while the GUI is alive, in which case
focus is impossible without OS-level enumeration of all windows owned by
related processes. Documented as a v1 limitation.

#### 5.7.3 Close

```
Frontend invokes Tauri command: subsession_close { id }
  → terminal kind:    SubPtyPool.kill(id) — drains the read/wait threads,
                      removes the runtime entry, then prunes the in-memory
                      and persisted records.
  → application kind: AppPool.detach(id) — drops Arborist's tracking only.
                      The external process keeps running. Sub-tab disappears.
```

#### 5.7.4 Window Focus (application kind)

`window_focus::WindowFocuser` is a trait with platform-gated implementations:

- **Windows**: hand-rolled minimal `user32` FFI (`EnumWindows` to find the
  HWND for the PID, `AllowSetForegroundWindow`, `SetForegroundWindow`,
  `ShowWindow(SW_RESTORE)`).
- **macOS**: `osascript -e 'tell application "System Events" to set
  frontmost of (first process whose unix id is <pid>) to true'`. AppleScript
  errors `-1743` and `-1728` map to `PermissionDenied` and `NotFound`.
- **Linux**: detects Wayland (`WAYLAND_DISPLAY` set with no `DISPLAY`) and
  returns `Unsupported`; otherwise shells out to `wmctrl -lp` to find the
  window id, then `wmctrl -ia <wid>`. Returns `ToolMissing("wmctrl")` if
  the binary is absent.

`wmctrl` is documented as an optional system dependency in the README; its
absence degrades gracefully to a no-op (with a `tracing::warn`).

#### 5.7.5 Parent-Close Cascade

Closing a top-level session must tear down its sub-sessions atomically:

```
Frontend invokes Tauri command: session_close { sessionId, deleteWorktree? }
  → session_close wrapper (commands/mod.rs):
      1. Sets a tombstone: AppContext.closing_parents.insert(sessionId).
         Held via an RAII ClosingParentGuard so the entry is removed even
         if the close path panics. Refuses concurrent subsession_create
         and skips orphaned restore records under this parent.
      2. Calls subsession::close_for_parent_impl(parent):
         - terminal subs: SubPtyPool.kill(). On real failure (NotFound is
           treated as success), emits Error status and KEEPS the orphan
           record visible — a visible orphan beats a silent leak.
         - application subs: AppPool.detach() only. The user's editor must
           outlive the parent close.
         - On success, prunes both the in-memory entry and the persisted
           SubSessionRecord.
      3. Calls session::session_close_impl as before.
      4. Guard drops → tombstone cleared.
```

#### 5.7.6 Restore on Launch (second pass)

The existing `restore_all_sessions` runs first; the sub-session second pass
is dispatched on the **same** blocking thread so children only spawn after
their parents are present in `sessions.json`:

```
frontend_ready (one-shot) →
  spawn_blocking { restore_all_sessions(ctx); restore_all_sub_sessions_impl(ctx, sub_ctx) }

restore_all_sub_sessions_impl iterates AppConfig.lastOpenSubSessions:
  - If the parent session is missing or in closing_parents: drop the
    persisted record + skip (treats as orphan).
  - If def deleted: drop the persisted record + skip (sanitize_loaded_sub_session_records
    also runs at config-load time as a defence in depth).
  - Insert the SubSession into the in-memory store and emit
    subsession://restored with the full record so the frontend hydrates.
  - terminal:    spawn_terminal(record.composedCommand, parent.worktreePath).
                 Spawn failure flips status to Error but KEEPS the persisted
                 record so a future relaunch can retry.
  - application: leave as Exited (greyed). User click triggers relaunch.
```

The frontend attaches the `subsession://restored` listener **before**
calling `frontend_ready`, so events from the restore pass are not dropped
against the post-hydrate listener gap.

#### 5.7.7 Relaunch

Mirrors session_restart semantics — swaps the child under the **same** sub
id so the persisted record (and the user's tab position) is preserved:

```
Frontend invokes Tauri command: subsession_relaunch { id }
  → subsession_relaunch_impl:
      1. Looks up the existing SubSession; refuses if def deleted, disabled,
         or parent is closing.
      2. Best-effort tear-down of the prior child:
         - terminal:    SubPtyPool.kill (removes the runtime entry from the
                        pool synchronously before awaiting drain, so the
                        slot is free for the fresh spawn).
         - application: AppPool.detach.
      3. Re-derives composedCommand from the CURRENT def, so Settings-tab
         edits to the def take effect at relaunch time.
      4. Resets status to Starting, refreshes the persisted SubSessionRecord,
         and emits subsession://status before spawning so the UI can update
         immediately.
      5. Spawns under the same id. On failure: emits Error and keeps the row
         + persistence so the user can retry.
```

The frontend deduplicates rapid double-clicks via a per-id `relaunchPending`
set in the sub-session store.

## 6. Tauri Command & Event API

Frontend ↔ backend communication uses Tauri's typed command/event system. Commands
are invoked from the frontend via `invoke()`; events are pushed from the Rust backend
via `app_handle.emit()` and received in the frontend via `listen()`.

All commands are gated by Tauri capability declarations in `capabilities/main.json`.

### Commands (Frontend → Rust)

| Command | Payload | Return | Description |
|---------|---------|--------|-------------|
| `session_create` | `{ tool, worktreePath, instructionSetId? }` | `SessionView` | Compose and spawn a new session. `instructionSetId` is optional; when omitted, Claude is launched without `--system-prompt` and the CLI relies on `cwd`-based discovery for repo instructions. |
| `session_list` | — | `SessionView[]` | Return all current sessions (without composedCommand/tempFiles) |
| `session_close` | `{ sessionId, deleteWorktree? }` | `SessionCloseResult` | Terminate a session (after UI confirmation). When `deleteWorktree` is `true`, the backend additionally runs `git worktree remove --force <worktreePath>` after killing the PTY. Refuses to remove the configured `workspaceRoot` (main worktree), any path outside the workspace root, or a path still referenced by another live session. The session record + PTY are always torn down on success; if the worktree-remove step fails, the message is reported via `worktreeDeleteError` rather than as a hard error. |
| `session_focus` | `{ sessionId }` | — | Mark session as active |
| `session_resize` | `{ sessionId, cols, rows }` | — | Resize PTY |
| `session_input` | `{ sessionId, data }` | — | Send keystrokes to PTY |
| `session_restart` | `{ sessionId }` | — | Re-spawn a session using its stored `composedCommand` verbatim (DESIGN §5.4) |
| `frontend_ready` | — | — | One-shot signal from the frontend after first paint; triggers restore-on-launch (re-spawns every session in `lastOpenSessions` via `respawn_existing`). Idempotent — subsequent calls are no-ops. |
| `config_get` | — | `AppConfig` | Retrieve AppConfig |
| `config_set` | `Partial<AppConfig>` | — | Update AppConfig (`activeSessionId` is tri-state: omit to leave alone, `null` to clear, value to set) |
| `instructions_list` | — | `InstructionSet[]` | List available instruction sets from `instructionSetsDir` |
| `worktrees_list` | `{ repoRoot: string }` | `WorktreeInfo[]` | Enumerate git worktrees rooted at `repoRoot`. Implemented via the injectable `GitRunner` seam (production: `git worktree list --porcelain`, parsed in `src-tauri/src/git.rs`). **Always returns `Ok(vec![])` on failure** — git missing, repo_root not a directory, repo_root is not a git repository, or any IO/parse error degrades to an empty list (logged with `code="GitUnavailable"`) so the UI's "Browse…" fallback is never blocked by an error toast. `WorktreeInfo = { path, branch?, isMain, isLocked }`. |
| `workspace_validate` | `{ path: string }` | `{ valid: boolean, error?: string }` | Validate a candidate workspace root for the first-boot picker (Roadmap §1.1). Returns `valid: true` only when `path` is an absolute, existing directory that contains a git repository (probed via `git -C <path> rev-parse --is-inside-work-tree`). On failure, `error` carries a short human-readable reason (`"path is not an absolute directory"`, `"not a git repository"`, …). Never throws an `AppError` for the "invalid" case — the picker shows inline feedback. |
| `worktree_create` | `{ name: string }` | `{ path: string }` | Create a new linked worktree at `<workspaceRoot>/.worktrees/<name>` on a fresh branch named `<name>` (Roadmap §2.2). Requires `workspaceRoot` to be set in `AppConfig`; errors with `NotFound` otherwise. The `name` is re-validated server-side via the same rules as `validateWorktreeName` (no spaces; no `..`, `~`, `^`, `:`, `?`, `*`, `[`, `\\`; cannot start/end with `.` or `/`; cannot end with `.lock`; cannot be `@`; 1–255 chars); `InvalidPath` is returned for any rule violation. Runs `git -C <workspaceRoot> worktree add .worktrees/<name> -b <name>` via the injected `GitRunner`; bubbles up the captured stderr in the `Internal` error message on git failure. Returns the canonical absolute path to the new worktree directory. |
| `subsession_create` | `{ parentSessionId, defId }` | `SubSession` | Compose and spawn a sub-session under the named parent. See §5.7.1. Errors: `NotFound` (def or parent missing), `InvalidArgument` (def disabled, or parent in closing tombstone), `WorktreeMissing`, `PtySpawnFailed`/`AppSpawnFailed`/`ToolMissing`. |
| `subsession_close` | `{ id }` | — | Tear down a sub-session. Terminal kind kills the PTY; application kind only detaches Arborist's tracking (the external program keeps running). See §5.7.3. |
| `subsession_focus` | `{ id }` | — | For application kind, focus the OS window via `WindowFocuser` (§5.7.4). Errors: `NotApplicable` (PID is a launcher wrapper that exited), `PermissionDenied`, `ToolMissing`, `Unsupported` (Wayland). For terminal kind, no-op on the backend; the frontend swaps viewports. |
| `subsession_list` | `{ parentSessionId? }` | `SubSession[]` | List sub-sessions for a parent (or all sub-sessions if omitted). |
| `subsession_input` | `{ id, data }` | — | Send keystrokes to a terminal sub-session's PTY. Returns `NotApplicable` for application kind. |
| `subsession_resize` | `{ id, cols, rows }` | — | Resize a terminal sub-session's PTY. Returns `NotApplicable` for application kind. |
| `subsession_relaunch` | `{ id }` | `SubSession` | Re-spawn a sub-session under the same id, refreshing `composedCommand` from the current def. See §5.7.7. Errors: `NotFound` (def deleted), `InvalidArgument` (def disabled or parent closing), `WorktreeMissing`, `PtySpawnFailed`/`AppSpawnFailed`/`ToolMissing`. |
| `subsession_icon` | `{ id }` | `string \| null` | Best-effort fetch of the OS application icon for an `application`-kind sub-session, returned as a `data:image/png;base64,…` URI. Returns `null` (not an error) for terminal sub-sessions, exited PIDs, unsupported platforms, and lookup misses; the frontend falls back to a generic emoji on `null`. Backed by `IconCache` (keyed by canonical exe path; never re-extracts the same exe). Extraction runs on `tokio::task::spawn_blocking`. Per-platform: Windows uses `SHGetFileInfoW` + `DrawIconEx`; macOS shells out to `plutil` + `sips` against the `.app` bundle; Linux walks `~/.local/share/applications` + `/usr/share/applications` for a matching `.desktop` file and resolves `Icon=` against standard `hicolor` PNG paths (no SVG, no full XDG theme resolution). |

> **Test-only seam.** The Rust backend consults two env vars,
> `ARBORIST_CLI_OVERRIDE_CLAUDE` and `ARBORIST_CLI_OVERRIDE_COPILOT`, when composing
> a session's command. If set, the value replaces the bare `claude` / `copilot`
> program token (already shell-quoted). Production never sets these; they exist
> only so integration tests can drive the full lifecycle against a deterministic
> child process. The override path is encoded verbatim into the persisted
> `composedCommand`, so restarting the session with the env var unset will spawn
> the literal path (not fall back to `claude`/`copilot`). See
> `compose::cli_program_for_tool`.

### Events (Rust → Frontend)

| Event | Payload | Description |
|-------|---------|-------------|
| `session://output` | `{ sessionId, data: string }` | Stream PTY output to xterm.js |
| `session://status` | `{ sessionId, status }` | Notify session state changes (including `'error'`) |
| `session://activity` | `{ sessionId, kind: 'title' \| 'attention' \| 'working' \| 'idle' \| 'promptStart' \| 'commandStart' \| 'commandEnd', value?, exit? }` | Per-session activity inferred from the PTY stream by `src-tauri/src/activity.rs` (OSC parsing + output byte-rate). Drives sidebar tab state indicators (working spinner, attention dot). Best-effort & advisory — UI must degrade gracefully if a CLI emits nothing. |
| `session://metrics` | `{ sessionId, model?, contextUsedPct?, contextTokensUsed?, contextTokensLimit?, inputTokens?, outputTokens?, observedAt }` | Per-session token usage / context-window utilization. Emitted by `src-tauri/src/session_metrics.rs`, which polls Claude's `~/.claude/projects/<encoded-cwd>/<sid>.jsonl` transcripts (heuristic cwd+mtime mapping) and extracts cumulative `usage` from the latest assistant turn. Claude-only in v1; Copilot tabs receive no events. Drives the compact second line on each sidebar tab. Best-effort & debounced — UI must degrade gracefully when no snapshot is present. |
| `subsession://status` | `{ id, status, pid?, message? }` | Sub-session lifecycle change. The production sink mutates the in-memory `SubSessionStore` *before* dispatching the event so a `subsession_list` race observes the new value. PID is forced to `None` for terminal states (`exited`/`error`). |
| `subsession://exited` | `{ id, exitCode? }` | Application sub-session's external process closed itself (or its launcher wrapper exited). The frontend reducer maps `exitCode != 0` to status `error`; otherwise `exited`. |
| `subsession://restored` | `{ subSession }` | Emitted by the restore second pass (§5.7.6) for each `lastOpenSubSessions` record successfully re-materialised in the in-memory store. The frontend's `applyRestored` reducer is idempotent and never steals `activeByParent` from a tab the parent already owns. |
| `session://output` (sub) | `{ sessionId, data }` | Terminal sub-session output reuses the existing channel — the UUID id space is global across `Session` and `SubSession`, and the frontend filters by id when subscribing. |

### Plugin commands routed via the bridge

`src/lib/tauri-bridge.ts` also wraps one third-party plugin command so
that callers stay on the single bridge surface (no direct
`@tauri-apps/plugin-*` imports from components):

| Bridge function | Underlying plugin call | Capability | Purpose |
|-----------------|------------------------|------------|---------|
| `pickDirectory()` | `tauri-plugin-dialog`'s `open({ directory: true, multiple: false })` | `dialog:allow-open` | Native OS directory picker; powers the New-Session dialog's manual "Browse…" fallback when `worktrees_list` returns nothing useful (SPEC W-03). |

The `dialog:allow-open` permission is declared in
`src-tauri/capabilities/main.json`; the plugin itself is initialised in
`arborist_lib::run`. Adding any further plugin command must follow the same
capability + bridge-wrapper + mock-stub discipline (see
[`TESTING.md`](./TESTING.md) §6).

## 7. Directory Structure (Proposed)

```
arborist/
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
│   │   ├── sub_sessions.rs       # SubPtyPool + SubSessionStore (terminal sub-tabs)
│   │   ├── app_launcher.rs       # AppPool + AppSpawner (application sub-tabs)
│   │   ├── window_focus.rs       # Platform-gated WindowFocuser (Win32/osascript/wmctrl)
│   │   ├── config_store.rs       # tauri-plugin-store wrapper
│   │   ├── commands/             # #[tauri::command] handlers
│   │   │   ├── mod.rs            #   wiring + production sinks
│   │   │   ├── session.rs        #   session_* + restore
│   │   │   └── subsession.rs     #   subsession_* + cascade + restore second pass
│   │   └── types.rs              # Session, SubSession, CustomProcessDef, AppConfig (serde)
│   ├── permissions/
│   │   └── allow-subsession.toml # Tauri capability allow-list for subsession_* commands
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

Arborist is a local, single-user desktop application. It makes no inbound or
outbound network connections and is not exposed to untrusted content. The primary
concerns are (a) bugs in Arborist's own command-building logic causing unintended
shell execution, and (b) robustness against bad input and resource exhaustion.

### 8.1 Shell Command Correctness

The most consequential class of bug would be Arborist constructing a malformed
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
- **Custom-process commands**: A `CustomProcessDef.command` is composed at
  sub-session creation time and stored verbatim in `SubSession.composedCommand`,
  then passed to the platform shell (`$SHELL -c <cmd>` on Unix, `%COMSPEC% /c
  <cmd>` on Windows) with `cwd` set to the parent session's worktree path. The
  worktree path is **never** interpolated into the command string. Defs come
  exclusively from validated config (the Settings tab applies the same rules as
  `validate_custom_processes`); free-form user input from the chat / terminal
  is never spliced into a shell command. `subsession_relaunch` re-derives
  `composedCommand` from the current def at relaunch time so Settings-tab edits
  take effect, but the same path-as-`cwd` discipline applies.

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
  `<os-temp>/arborist/<uuid>/` and deletes any UUID-named directory whose UUID is not in
  the persisted session list **and** whose mtime is older than 1 hour
  (`pty_pool::ORPHAN_AGE_THRESHOLD`). The age threshold prevents racing against an
  in-flight session whose temp file was just written; the persisted-set check makes
  cleanup restore-safe.
- **Scrollback cap**: xterm.js is configured with a `scrollback` limit (default:
  5 000 lines) to bound per-session memory in the frontend.

### 8.4 Credential Handling

Arborist stores no credentials. Authentication is managed entirely by the CLI tools
(`claude`, `copilot`) via their own credential stores. The app neither reads nor writes
tokens, API keys, or passwords.

## 9. Future Considerations

- **Split view**: Show two terminals side by side.
- **Worktree auto-discovery**: Deeper scanning (e.g., recursive search, multiple remotes). _(Basic discovery anchored on `workspaceRoot` — and its legacy `worktreeRoots` companion — is included in v1; see `WORKTREES.md`.)_
- **Session snapshots**: Save/restore terminal scrollback.
- **Theming**: Respect system dark/light mode and allow custom terminal themes.
- **Keyboard shortcuts**: Ctrl+Tab to cycle sessions, Ctrl+N for new session.
- **In-app instruction set editor**: Create and edit instruction files without leaving the app.
