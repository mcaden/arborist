# Arborist — architecture tour

A guided walk through the codebase, mapping SPEC requirements and DESIGN
sections to the actual modules that satisfy them. Read SPEC and DESIGN
first; this document fills in "where does X live in code?".

## 1. Two-process picture

Arborist is one process with a clean Rust ↔ WebView split:

```
┌──────────── Rust backend (Tauri v2) ────────────┐
│  PTY pool (portable-pty)                         │
│  Config store (tauri-plugin-store, atomic write) │
│  Compose / git / commands                        │
│  ▲                                               │
│  │  Tauri commands  (invoke)                     │
│  │  Tauri events    (emit / listen)              │
│  ▼                                               │
├──────────── React frontend (Vite) ──────────────┤
│  Sidebar  ─  TerminalView  ─  NewSessionDialog   │
│  Zustand stores: session, config, dialog         │
│  use-terminal hook (xterm.js lifecycle)          │
└──────────────────────────────────────────────────┘
```

The contract between halves is the typed Tauri command/event surface in
DESIGN §6. Both sides go through a single bridge module
(`src/lib/tauri-bridge.ts`) — no React component imports
`@tauri-apps/api` directly.

## 2. Backend module map (`src-tauri/src/`)

| File                          | Purpose                                                                                              | Maps to                            |
| ----------------------------- | ---------------------------------------------------------------------------------------------------- | ---------------------------------- |
| `main.rs`                     | Process entry — calls `arborist_lib::run`.                                                              | —                                  |
| `lib.rs`                      | `init_tracing`, builds the `AppContext` (PtyPool + ConfigStore + GitRunner + production PtySink), registers commands, runs the Tauri event loop. | DESIGN §2.1                       |
| `types.rs`                    | All serde types: `Session`, `SessionView`, `AppConfig`, `PartialAppConfig`, `InstructionSet`, `WorktreeInfo`, `SessionStatus`, `Tool`, errors, event payloads, command arg structs. | DESIGN §3                  |
| `compose.rs`                  | Pure functions: `compose_command`, `dedupe_label`, `validate_worktree`, POSIX/`cmd.exe` shell quoting, `cli_program_for_tool` (test override seam). | DESIGN §5.1, §5.6, §8.1, §8.2     |
| `config_store.rs`             | `tauri-plugin-store` wrapper: load/save `config.json` + `sessions.json`, atomic write via `NamedTempFile::persist`, deep-merge `PartialAppConfig`, quarantine on parse failure, instruction-set discovery. | DESIGN §3.3, [`CONFIGURATION.md`](./CONFIGURATION.md) |
| `git.rs`                      | `GitRunner` trait + `RealGitRunner`; parses `git worktree list --porcelain`. Returns `Ok(vec![])` on every error path so the UI's manual "Browse…" fallback isn't blocked. | DESIGN §6 (`worktrees_list`)      |
| `pty_pool.rs`                 | `PtySpawner` / `ChildPty` traits, `PortablePtySpawner`, `PtyPool` (per-session runtime entry: child handle, drain task, cancel token), bounded mpsc with drop-newest backpressure (`OUTPUT_CHANNEL_CAPACITY = 512`), `ESC c` reset after a drop, streaming UTF-8 decoder, wait thread that persists final status, `cleanup_orphans` (`ORPHAN_AGE_THRESHOLD = 1h`, ignores UUIDs still in `sessions.json`). | DESIGN §2.1, §5.4, §8.3 |
| `commands/mod.rs`             | Thin `#[tauri::command]` wrappers. Each one resolves the `AppContext` from Tauri managed state and delegates to `commands::session`. Also contains `build_production_sink` which wires the PTY status / output callbacks to `app.emit` and `ConfigStore::update_session_status`. | DESIGN §6 |
| `commands/session.rs`         | All real handler logic: `session_create_impl`, `session_close_impl`, `session_focus_impl`, `session_resize_impl`, `session_input_impl`, `session_restart_impl`, `session_list_impl`, `frontend_ready_impl`, `restore_all_sessions`, `worktrees_list_impl`. Holds the `AppContext` struct (Pool + Store + Sink + GitRunner). | DESIGN §5.1, §5.3, §5.4, §5.5 |
| `test_bin/arborist_test_child.rs`  | Deterministic child binary used by integration tests. Lives under `src/test_bin/` (not `src/bin/`) so Tauri's CLI doesn't pick it up as a bundle binary. Not used in production.                          | [`TESTING.md`](./TESTING.md) §3   |
| `test_bin/arborist_test_locker.rs` | Deterministic locker binary used by `workspace_lock_multiprocess` integration tests. Same `src/test_bin/` location and rationale as the test child. Not used in production. | [`TESTING.md`](./TESTING.md) §3   |

### Key invariants enforced by the backend

- **Compose once, reuse forever.** `Session.composedCommand` is built in
  `compose_command` at create time and stored. `session_restart` →
  `pty_pool::respawn_existing` re-uses that string verbatim;
  `restore_all_sessions` does the same. The string is the persistence
  contract (DESIGN §5.4).
- **`cwd` is not interpolated into the shell string.** `portable-pty`'s
  spawn API accepts `cwd` as a discrete field and `pty_pool` always uses
  it. `compose.rs` tests assert that no `cd "<path>" &&` ever appears
  (DESIGN §8.1, SPEC NF-08).
- **Path values are canonicalized at the boundary.** `config_store::save_config`
  rejects relative paths and any instruction file outside
  `instructionSetsDir`; `validate_worktree` requires an existing directory.
  Stale `worktreeRoots` entries are silently dropped on load with a warning;
  a stale `workspaceRoot` is cleared on load with a warning, prompting the
  first-boot picker on next launch.
- **Persistence is atomic.** `config_store` writes via
  `NamedTempFile::persist`; an aborted write leaves either the old file
  intact or the new file fully on disk — never a truncated half.
- **Restore is idempotent and never aborts.** `restore_all_sessions`
  iterates `tabOrder` order, isolates per-session failure (worktree
  missing, temp-file rematerialise failure, spawn failure → `Error`
  status, persist + emit, continue), and is gated by `frontend_ready` so
  no early output is lost (DESIGN §5.5).

## 3. Frontend module map (`src/`)

| File / dir                                  | Purpose                                                                                          | Maps to                                   |
| ------------------------------------------- | ------------------------------------------------------------------------------------------------ | ----------------------------------------- |
| `main.tsx`                                  | React entrypoint.                                                                                | —                                         |
| `App.tsx`                                   | App shell + boot sequence (see §4).                                                              | DESIGN §5.5                               |
| `components/Sidebar.tsx`, `SidebarTab.tsx`  | Vertical-tab list with full keyboard nav, error indicators, drag-to-reorder via `@dnd-kit`.      | SPEC §5.1                                 |
| `components/NewSessionButton.tsx`           | The `+` button that opens the dialog.                                                            | SPEC S-04                                 |
| `components/NewSessionDialog.tsx`           | Tool → worktree (quick-pick + Browse…) → instruction-set picker; submits `session_create`.       | SPEC §5.2 / §5.5                          |
| `components/MainArea.tsx`, `TerminalView.tsx` | Hosts the active session's xterm.js, error overlay + Restart button on `error` status.         | SPEC T-01..T-05                            |
| `components/CloseConfirmDialog.tsx`         | Confirmation modal before `session_close`.                                                       | SPEC S-08                                 |
| `components/ToolIcon.tsx`                   | Inline Claude / Copilot SVGs with accessible labels.                                             | SPEC NF-06                                |
| `hooks/use-terminal.ts`                     | One `Terminal` per `sessionId` in a module-level `Map` (survives unmounts on tab switch); attach/detach to DOM only; `ResizeObserver` debounced ~50 ms; single app-level `session://output` router demuxes by sessionId. | SPEC T-03, DESIGN §2.2 |
| `lib/tauri-bridge.ts`                       | Typed wrappers for every command / event in DESIGN §6 (single import surface for Tauri).         | DESIGN §6                                 |
| `lib/tauri-bridge.mock.ts`                  | Vitest mock counterpart; structurally enforced via `satisfies typeof realBridge`.                | [`TESTING.md`](./TESTING.md) §2           |
| `lib/session-events.ts`                     | App-level `session://status` subscriber that pushes status updates into the session store.       | DESIGN §5.4 / §5.5                        |
| `store/session-store.ts`                    | Zustand store: `sessions[]`, `activeId`, `pendingClose`. Actions: `hydrate`, `create`, `close`, `focus`, `reorder`, `applyStatus`. **No `applyOutput`** — output bypasses Zustand and goes straight to xterm via `use-terminal`. | DESIGN §2.2 (Phase 8 design)              |
| `store/config-store.ts`                     | Zustand store backed by `config_get` / `config_set`.                                              | DESIGN §3.3                               |
| `store/new-session-dialog-store.ts`         | Modal step / draft state for the New-Session flow.                                                | SPEC §5.2                                 |
| `types/arborist.ts`                            | TypeScript mirrors of every Rust type in `types.rs`. Each interface carries a `// MIRROR:` marker pointing at the canonical definition. | DESIGN §3 |

## 4. Boot sequence (DESIGN §5.5)

`App.tsx` orchestrates startup so that **listeners are attached before
any session spawns**, satisfying SPEC NF-03 and guaranteeing no early
output is lost:

1. **Tauri `setup` (Rust)** — initialise plugins, build the `PtyPool`
   with the production `PortablePtySpawner`, run `cleanup_orphans()`,
   register `AppContext` as managed state, open the window. **No
   sessions spawn yet.**
2. **`App.tsx` mount** —
   1. `useConfigStore.hydrate()` — `config_get`.
   2. `useSessionStore.actions.hydrate()` — `session_list`. Backend
      coerces persisted records out of any stale `Running` into
      `Starting` (the spawn restart will move them to `Running` /
      `Error` shortly).
   3. `initTerminalRouter()` — attaches the global `session://output`
      listener.
   4. `subscribeToStatus()` — attaches the global `session://status`
      listener.
   5. `frontendReady()` — one-shot CAS on the backend, kicks off
      `restore_all_sessions` on a blocking thread. Subsequent calls are
      no-ops.
3. **`restore_all_sessions` (Rust)** — for each persisted session in
   `tabOrder`:
   1. `validate_worktree` — on failure, mark `Error`/`WorktreeMissing`,
      persist + emit, continue.
   2. Materialise any missing `temp_files` — on failure, mark `Error`/
      `InstructionFileMissing`, persist + emit, continue.
   3. `pty_pool::respawn_existing` — re-spawns from the **stored**
      `composedCommand` and `worktreePath` (never re-composes). Spawn
      failures are mapped per-session to `Error` and never abort the
      loop.

A `<BootSplash />` is shown while step 2 runs; an `<ErrorOverlay />` with
a Reload button is shown if any hydrate step throws.

## 5. Capability gating (Tauri v2)

Every `#[tauri::command]` callable from the WebView is gated by an entry
in `src-tauri/capabilities/main.json` referencing a permission file in
`src-tauri/permissions/`. The current capability set:

| Permission              | Gates                                                                  |
| ----------------------- | ---------------------------------------------------------------------- |
| `core:default`          | Built-in core APIs (clipboard, window, etc).                            |
| `allow-ping`            | `ping` (smoke / health command).                                        |
| `allow-config`          | `config_get`, `config_set`.                                             |
| `allow-instructions`    | `instructions_list`.                                                    |
| `allow-session`         | `session_create`, `session_list`, `session_close`, `session_focus`, `session_resize`, `session_input`, `session_restart`. |
| `allow-frontend-ready`  | `frontend_ready`.                                                       |
| `allow-worktrees-list`  | `worktrees_list`.                                                       |
| `dialog:allow-open`     | `tauri-plugin-dialog`'s `open` (used by `pickDirectory` for the New-Session "Browse…" fallback). |

The structural test in `src-tauri/tests/capability_gating.rs` keeps the
capability JSON, the `permissions/*.toml` files, and the registered
commands in lockstep. See [`TESTING.md`](./TESTING.md) §6 for the
checklist when adding a new command.

## 6. Where to look when…

| You want to…                                         | Start here                                                          |
| ---------------------------------------------------- | ------------------------------------------------------------------- |
| Add a new Tauri command                              | [`TESTING.md`](./TESTING.md) §6 checklist + `commands/mod.rs`.       |
| Change the persisted config shape                    | `types.rs` (`AppConfig` + `PartialAppConfig`) → `types/arborist.ts` mirror → bump `CONFIG_VERSION_CURRENT` and add a migration in `config_store.rs`. |
| Tune backpressure or scrollback                      | `pty_pool.rs::OUTPUT_CHANNEL_CAPACITY` (Rust) / `useTerminal` `scrollback` option (Frontend). |
| Add a new instruction-set tool                        | `Tool` enum in `types.rs`, the discovery rules in `config_store.rs`, the per-tool branch in `compose.rs::compose_command`. |
| Modify the New-Session flow                          | `components/NewSessionDialog.tsx` + `store/new-session-dialog-store.ts`. |
| Investigate a restore failure                        | `commands/session.rs::restore_all_sessions` + `tracing` output (`RUST_LOG=arborist_lib=debug`). |
| Reproduce backpressure / leak behaviour              | [`../ai/SMOKE_TEST_RESULTS.md`](../ai/SMOKE_TEST_RESULTS.md) procedures. |
