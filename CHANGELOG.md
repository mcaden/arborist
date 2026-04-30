# Changelog

All notable changes to Arborist are documented in this file.

The format is loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows semantic versioning once it reaches a stable v1.

## [Unreleased]

### Added — Tab context menu & custom-process sub-tabs

A right-click (or Shift+F10 / Apps key) on a sidebar tab now opens a context
menu whose **Launch…** submenu lists every enabled custom-process definition
from `AppConfig.customProcesses`. Selecting one spawns a **sub-tab** under
that session in two flavours:

- **Terminal** — a PTY hosted in-app (cwd = the parent's worktree),
  rendered with `xterm.js` exactly like a top-level session.
- **Application** — an external GUI program spawned **detached** so closing
  Arborist (or the parent session) does not kill it. Clicking the sub-tab
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
- `ParentClosing` — `subsession_create` / `subsession_relaunch` against a
  parent currently mid-cascade.

### Optional system dependencies

- **Linux X11**: install `wmctrl` to enable click-to-focus for application
  sub-tabs. Absence degrades to a logged no-op.

### Documentation

- `dev/docs/SPEC.md` §5.7 (CP-01 – CP-11): functional requirements for the
  feature.
- `dev/docs/DESIGN.md` §3.4 / §5.7 / §6 / §7 / §8.1: data model, lifecycle
  flows, command/event API, directory layout, and the shell-injection
  invariant for custom-process commands.
- `README.md` — Optional system dependencies section.
