# Changelog

All notable changes to Arborist are documented in this file.

The format is loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows semantic versioning once it reaches a stable v1.

## [Unreleased]

### Changed — Dependency upgrades

- **`portable-pty` `0.8` → `0.9`** ([#218](https://github.com/mcaden/arborist/pull/218), closes [#215](https://github.com/mcaden/arborist/issues/215)). The new
  release enables `PSUEDOCONSOLE_INHERIT_CURSOR` on Windows ConPTY, which makes the pseudo-console emit `ESC[6n` (DSR cursor-position request) at startup
  and refuse to forward any child stdout until the host answers with `ESC[<row>;<col>R`. The production frontend handles this transparently via
  `xterm.js`; the `pty_pool` integration tests gained a streaming `DsrScanner` + `dsr_responding_sink` helper that does the same so real-ConPTY tests
  don't deadlock waiting on a banner.
- **`sha2` `0.10` → `0.11`**. The `Digest` trait no longer ships an inherent `hex` helper; `shell_trust` formats the fingerprint manually with
  `std::fmt::Write` to keep the on-disk format byte-identical.
- **`fs2` removed** ([#211](https://github.com/mcaden/arborist/pull/211)). Workspace locking moved off the `fs2` crate (unmaintained) and onto the
  platform-native primitives wrapped by `std::fs::File::try_lock` (stabilized in Rust 1.89). No behavioural change — `WorkspaceLockGuard` still emits
  `LockError::Contention` on a busy lock and `cross-process` lock tests still pass.
- **Trimmed unused tokio / tokio-util features** ([#211](https://github.com/mcaden/arborist/pull/211)). Dropped `tokio`'s `io-util` feature (no code
  imports `AsyncReadExt`/`AsyncWriteExt`) and `tokio-util`'s `rt` feature, shaving compile time and dependency surface.

### Changed — Documentation refresh for public open-source readiness (issue #106)

- Consolidated active project documentation under lowercase `docs/` files, added root GitHub community-health files, and removed the previous split
  documentation set.
- Reworked architecture, runtime-flow, configuration, worktree, development, testing, contributing, release, roadmap, security, support, and conduct
  documentation for public readers.
- Standardized active documentation diagrams on Mermaid.
- Historical changelog entries may still mention old documentation paths because they describe the project state at the time of those changes.

### Added — Worktree as parent tab, AI agents as child tabs (issue #44)

The sidebar is now a two-level hierarchy: each top-level row is a **worktree
tab** (one per `WorktreeTab` record), and its child rows are the AI-agent
sessions and custom-process sub-sessions owned by that worktree tab. Clicking a
worktree row clears its `activeChildId` and shows a **WorktreeDashboard**
placeholder in the main area; clicking a child shows its terminal as before.
Right-clicking a worktree row exposes flat **Launch Claude** / **Launch
Copilot** entries plus enabled custom-process entries. Closing a worktree row
cascades close to all of its children.

The `+` button now opens `NewSessionDialog` as a worktree-only flow: it opens
an existing worktree tab or creates a new worktree and opens its tab. AI agents
are launched afterward from the worktree row context menu.

Drag-to-reorder and Alt+Arrow reorder are **deferred for v1**: the grouped
layout breaks the previous flat-id reorder model, and per-group reorder is a
planned follow-up. The drag pipeline test still passes because it exercises
the store directly, not the sidebar UI.

### Added — Tab context menu & custom-process sub-tabs

A right-click (or Shift+F10 / Apps key) on a sidebar tab now opens a context
menu whose **Launch…** submenu lists every enabled custom-process definition
from `AppConfig.customProcesses`. Selecting one spawns a **sub-tab** under
that session in two flavours:

- **Terminal** — a PTY hosted in-app (cwd = the parent's worktree),
  rendered with `xterm.js` exactly like a top-level session.
- **Application** — an external GUI program spawned **detached** so closing
  Arborist (or the parent worktree tab) does not kill it. Clicking the sub-tab
  attempts to focus the program's OS window via a platform-gated focuser
  (`user32` on Windows, `osascript` on macOS, `wmctrl` on Linux X11; Wayland
  reports `Unsupported`). Closing the sub-tab only drops Arborist's tracking.

Highlights:

- **Settings → Custom Processes** tab: full CRUD over the def list with
  validation that mirrors the backend's `validate_custom_processes`
  (`id` matches `[A-Za-z0-9_-]+` and is unique, trimmed `name`/`command`
  non-empty, `kind` required).
- **Built-in defs** seeded on fresh install and additively at v3→v4
  migration: `shell` (terminal), `open-folder` (application), and
  `vscode` (application, auto-enabled if `code` is on `PATH`). Built-ins are
  user-editable and user-deletable.
- **Sub-tab persistence**: terminal sub-tabs respawn fresh at next launch;
  application sub-tabs come back greyed and re-launch on user click. The
  restore second pass runs on the same blocking thread as
  `restore_all_sessions` so children only spawn after their parents.
- **Parent-close cascade**: closing a session tears down its sub-sessions
  atomically. Terminal subs are killed; application subs are only detached.
  An RAII tombstone (`AppContext.closing_parents`) closes the
  `subsession_create` race window.
- **Relaunch under same id**: `subsession_relaunch` swaps the child while
  preserving the persisted record's id and the user's tab position. The
  composed command is re-derived from the current def so Settings-tab edits
  take effect.

### Schema

- `configVersion` bumped **3 → 4**. Migration is additive: existing
  `customProcesses` entries are preserved; missing built-in IDs are seeded.
  v3 files load cleanly. Unparseable or future-version (`> 4`) configs are
  quarantined and replaced with defaults.
- `AppConfig` gains:
  - `customProcesses: CustomProcessDef[]`
  - `lastOpenSubSessions: SubSessionRecord[]`

### Tauri command / event surface

New commands (gated by `permissions/allow-subsession.toml`):

- `subsession_create`, `subsession_close`, `subsession_focus`,
  `subsession_list`, `subsession_input`, `subsession_resize`,
  `subsession_relaunch`.

New events:

- `subsession://status` (lifecycle, includes PID once Running)
- `subsession://exited` (application kind exit notification)
- `subsession://restored` (one per record materialised by the restore pass)

Terminal sub-session output reuses the existing `session://output` channel —
the UUID id space is global across `Session` and `SubSession`.

### New error variants (stable `code()` mappings)

- `ToolMissing` — e.g. `wmctrl` absent on Linux, or the launcher
  preflight could not locate the first command token on `PATH`.
- `NotApplicable` — e.g. `subsession_input` against an application sub-tab.
- `PermissionDenied` — e.g. macOS Accessibility prompt declined.
- `Unsupported` — e.g. window-focus on Wayland.
- `AppSpawnFailed` — application kind spawn failure.
- `InvalidCustomProcessDef` — `config_set` boundary validation rejection.
- `InvalidArgument` (with message "parent worktree tab … is closing") —
  `subsession_create` / `subsession_relaunch` against a parent currently
  mid-cascade.

### Optional system dependencies

- **Linux X11**: install `wmctrl` to enable click-to-focus for application
  sub-tabs. Absence degrades to a logged no-op.

### Documentation

- The custom-process requirements and architecture notes now live in
  `docs/product.md`, `docs/architecture.md`, and `docs/runtime-flows.md`.
- `README.md` — Optional system dependencies section.
