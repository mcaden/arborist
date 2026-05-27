# Arborist — Copilot Instructions

## Repository status

This repo contains the full Arborist Tauri + React codebase. The active docs live under `docs/`:

- `docs/product.md` — product requirements, scope, and non-functional expectations.
- `docs/architecture.md` — architecture, data model, command/event API, module map, and invariants.
- `docs/runtime-flows.md` — boot, restore, workspace switch, session, worktree, prep, and sub-session flows.
- `docs/configuration.md` — config/store layout and migration/quarantine behavior.
- `docs/worktrees.md` — workspace root and `.arborist/.worktrees/` rules.
- `docs/development.md` and `docs/testing.md` — development workflows.
- `CONTRIBUTING.md` — public contributor workflow.

Read the relevant docs before proposing structural changes. If code behavior changes, update the matching docs in the same PR.

This repo has a remote at `origin` -> `https://github.com/mcaden/arborist.git`. Agents may commit, push feature branches, and open pull requests. Do not push directly to `main` and do not force-push shared branches; land changes through PRs.

**PR title convention.** Always prefix PR titles with the worktree name in square brackets, falling back to the branch name when no worktree name is available: `[<worktree-or-branch>] <summary>`. The worktree name is the basename of the current worktree directory (e.g., a worktree at `.worktrees/pr-prefix` yields `[pr-prefix]`). Apply this to every `gh pr create` invocation.

## Dogfooding safety — don't kill the host

This repo is dogfooded: the user typically runs the **host** `arborist.exe` (or `arborist` on macOS/Linux) and you, the agent, are executing inside one of its PTY sessions. Killing the host crashes the user's editor and every sibling session, including yours. **A previous agent killed the host this way — do not repeat it.**

Hard rules:

- **Never** terminate `arborist.exe` / `arborist`, or its parent dev processes — `cargo run … arborist`, `pnpm dev` / `pnpm tauri:dev`, `tauri dev`, `pnpm vite`, the Vite dev server, or any `node`/`cargo` process you did not personally spawn in this session.
- **Never** use name-based or pattern-based process kills — `Stop-Process -Name`, `taskkill /IM`, `pkill`, `killall`, `Get-Process … | Stop-Process`. They will sweep up the host. Do not use or work around these commands.
- **Even with `Stop-Process -Id <PID>`**, only kill PIDs you captured from a child process you started yourself in this same session. If you didn't record the PID at spawn time, don't kill it.
- If your `cargo build` / `cargo run` is blocked by a "file in use" / target-locked error, **stop and ask the user** — that lock almost always means the host arborist is running. Do not "free" the lock by killing processes.
- Do not run `pnpm dev`, `pnpm tauri:dev`, `pnpm vite`, or `cargo run -p arborist` unless the user explicitly asks during the current session. Use `cargo build`, `cargo test`, `pnpm run build`, or `pnpm test --run` for verification instead.

If a task genuinely requires restarting the host, ask the user to do it — never do it yourself.

## What Arborist is

A cross-platform desktop app (Tauri v2 + React/TS) that manages multiple Claude CLI / GitHub Copilot CLI sessions, each bound to a Git worktree, in a sidebar of vertical tabs with a single visible PTY terminal in the main area.

## Stack (see `docs/architecture.md`)

- **Shell**: Tauri v2 (Rust backend, OS WebView frontend) — *not* Electron
- **Frontend**: React + TypeScript, Vite, Tailwind CSS, Zustand, xterm.js
- **Backend**: Rust, `portable-pty` for cross-platform PTY (ConPTY on Windows), custom JSON persistence via `config_store.rs`
- **Layout**: `src/` (frontend), `src-tauri/` (Rust), `crates/arborist-types/` (wire types), `docs/` (project docs)

## Architectural conventions (read `docs/architecture.md` and `docs/runtime-flows.md` before changing)

- **One PTY per session, lives in Rust.** xterm.js Terminal instances live in the frontend; only the active session's terminal is attached to the DOM, but all PTYs keep running in the backend.
- **Frontend to backend = Tauri commands + events only.** No direct Rust access from the WebView. Every command must be declared in `src-tauri/capabilities/main.json`. Keep the canonical command/event table in `docs/architecture.md#command-and-event-contract` in sync when you add or rename one.
- **Session shell invocation is composed once and stored.** `Session.composedCommand` is reused verbatim for restart and restore. Don't recompose at restart time.
- **Worktree path is passed as `cwd` to `portable-pty`, never interpolated into the command string.** Same rule for any user-supplied path.
- **Platform shell selection**: macOS/Linux -> `$SHELL -c <cmd>`; Windows -> `%COMSPEC% /c <cmd>`.
- **Duplicate session labels get a numeric suffix** (`"my-feature 2"`, `"my-feature 3"`).

## Tool-specific CLI launch rules (see `docs/architecture.md` and `docs/runtime-flows.md`)

| | Claude | Copilot | Codex |
|---|---|---|---|
| Repo instructions | Auto-loaded from `cwd` (`CLAUDE.md`). Don't pass it. | Auto-loaded from `cwd` (`.github/copilot-instructions.md`). Don't pass `--instructions` — it would disable auto-discovery. | Auto-loaded from `cwd` (`AGENTS.md`). Bare `codex` is correct — no instruction flag. |
| Worktree context | Derived by the CLI from `pwd`/`git` because the PTY pool sets `cwd` to the worktree. | Same. | Same. |
| Temp file | None for new sessions. Legacy restored sessions may still rematerialize persisted `tempFiles`. | None | None |
| Resume syntax | `claude --resume <id>` (flag) | `copilot --session-id <id>` (flag; create-or-resume — copilot-cli >= 1.0.51 split this from `--resume`, which is now strict-resume-only and errors on unknown UUIDs). Arborist uses `--session-id` for first launch, restart, and restore-on-launch. | `codex resume <id>` (**subcommand**, not a flag). The metrics watcher discovers the thread id from the rollout file's `SessionMeta` line. |

## Data model

Defined in Rust with `serde` in `crates/arborist-types/src/lib.rs`, mirrored as TypeScript types in `src/types/arborist.ts`. Keep Rust and TS definitions in lockstep when modifying.

## Persistence rules

- All persistent state goes through the Rust `ConfigStore` (`AppConfig` + session records).
- `lastOpenSessions` and `tabOrder` drive restore-on-launch — update them on session create/close/reorder/focus.
- The app **must not** store credentials. Auth is delegated to the CLI tools.

## Out of scope for v1 (don't implement)

Built-in chat UI, remote/SSH worktrees, public plugin marketplace, multi-window, and in-app instruction-file editing are out of scope for v1.

---

# Development patterns

These are the patterns to follow when writing code in this repo. They are opinionated — deviate only with a documented reason.

## Cross-cutting

- **Product and architecture docs are the source of truth.** If code disagrees with `docs/product.md` or `docs/architecture.md`, fix the code or update the docs in the same change — never let them silently drift.
- **Single source of truth for the API surface.** The command/event table in `docs/architecture.md#command-and-event-contract` is authoritative. When you add, rename, or change a command/event: update the docs, Rust `#[tauri::command]` handler, `tauri::generate_handler![...]`, `src-tauri/capabilities/main.json`, permission file, typed bridge wrapper, bridge mock, and mirrored TS types in the same PR.
- **No `any`, no `unwrap()` on the happy path.** TS `any` and Rust `.unwrap()`/`.expect()` are code smells outside of tests and truly-infallible invariants. Prefer `Result`/typed errors and exhaustive matching.
- **Fail loud at boundaries, recover gracefully inside.** Validate inputs at the Tauri command boundary; once past it, types should make invalid states unrepresentable.

### Code Comments

- Focus comments on **why**, not **what**. The code already says what it does; comments should explain intent, constraints, trade-offs, or non-obvious design decisions.
- Do not document changes to the spec in code comments — update the spec instead. The spec is the source of truth for design decisions; code comments are secondary annotations.
- Inline comments inside method bodies are appropriate only when the logic would otherwise be opaque — link to tickets, specs, or external constraints when relevant.

### Line length

- **150 characters** is the maximum line length for all code and comments in both Rust and TypeScript.
- Rust: enforced by `rustfmt.toml` (`max_width = 150`).
- TypeScript/JS: enforced by Prettier (`.prettierrc.json`, `printWidth: 150`).

## Rust (src-tauri/)

### Module layout
Match `docs/architecture.md#backend-modules`. One concern per file. `commands/mod.rs` is a thin layer — it deserializes args, calls into the relevant module, maps errors. No business logic in command handlers.

### Error handling
- Define a crate-wide `Error` enum with `thiserror`; convert to a `serde`-friendly shape at the Tauri boundary (commands return `Result<T, AppError>` where `AppError: serde::Serialize`).
- Never panic in a command handler. `?` everywhere; map foreign errors via `From` impls.
- PTY/IO errors get their own variants so the frontend can branch on them (e.g., `PtySpawnFailed`, `WorktreeMissing`).

### Concurrency
- The PTY pool is shared mutable state — wrap it in `Arc<Mutex<PtyPool>>` (or `tokio::sync::Mutex` if held across `.await`) and store it via `app.manage(...)`. Access from commands with `State<'_, Arc<Mutex<PtyPool>>>`.
- **Never hold a lock across an `.await` you don't control.** Lock → copy what you need → drop → await.
- PTY read loops run on dedicated OS threads (not tokio tasks) — `portable-pty` reads are blocking. Use `std::thread::spawn` and forward bytes through a channel or directly via `app_handle.emit()`.
- Use bounded channels (`tokio::sync::mpsc::channel(N)`) for PTY → frontend so a stalled WebView can't OOM the backend. Drop oldest on overflow with a logged warning.

### Tauri commands
- Always `async fn` even if the body is sync — keeps the signature uniform and lets you add `.await`s later without churn.
- Payload structs live in `crates/arborist-types/src/lib.rs`, `#[derive(Deserialize)]`, `#[serde(rename_all = "camelCase")]` to match the TS side.
- Return types are owned (`Session`, not `&Session`) — Tauri serializes them.
- Every new command requires a corresponding entry in `capabilities/main.json`. Missing capability = silent failure in production builds.

### Events
- Event names are namespaced with `://` (e.g., `session://output`) — keep that convention.
- Payloads are always structs with named fields, never bare strings/tuples — gives us forward compatibility.

### Types & serde
- One canonical Rust definition per wire type in `crates/arborist-types/src/lib.rs`. Use `#[serde(rename_all = "camelCase")]` so Rust uses snake_case and the wire/TS uses camelCase.
- Newtype wrappers for IDs (`SessionId(Uuid)`, `WorktreeTabId(Uuid)`) — prevents passing the wrong ID into the wrong function.

### Testing
Rust-specific principles (procedural detail — test layout, fixtures, virtual-time setup — lives in the `quality-workflow-gate` skill):
- Pure logic (label dedup, command composition, path validation) is unit-tested.
- PTY pool gets integration tests that don't depend on `claude`/`copilot` being installed.
- `cargo fmt` + `cargo clippy -- -D warnings` must be clean.

## TypeScript / React (src/)

### Project conventions
- `strict: true` in `tsconfig.json`, plus `noUncheckedIndexedAccess` and `exactOptionalPropertyTypes`. No `// @ts-ignore` without an issue link in the comment.
- Path alias `@/*` → `src/*`. No deep relative imports (`../../../`).
- ESLint with `@typescript-eslint`, `eslint-plugin-react-hooks`, `eslint-plugin-react-refresh`. Prettier for formatting (no style debates in review).
- Functional components only. No class components. No `React.FC` — declare props inline.

### State management (Zustand)
- One store per concern (`session-store`, `config-store`) — not one mega-store.
- Selectors are colocated and granular: `const tabs = useSessionStore(s => s.tabs)`. Never `useStore(s => s)` — causes re-render storms.
- Actions live inside the store definition, not as free functions that mutate via `setState`.
- Persistence of UI-only state (e.g., active tab) goes through the store middleware; persistent app config goes through Tauri `config_get`/`config_set` (Rust owns the file).

### Tauri bridge (`src/lib/tauri-bridge.ts`)
- All `invoke()` and `listen()` calls go through this module — components never import from `@tauri-apps/api` directly.
- One typed wrapper per command/event, e.g.:
  ```ts
  export const sessionCreate = (args: SessionCreateArgs): Promise<Session> =>
    invoke('session_create', args);

  export const onSessionOutput = (cb: (e: SessionOutputEvent) => void) =>
    listen<SessionOutputEvent>('session://output', e => cb(e.payload));
  ```
- Event listeners return the unlisten function — callers must call it on cleanup. Hooks that subscribe MUST unsubscribe in their effect cleanup.

### xterm.js lifecycle (`use-terminal` hook)
- Terminal instances are created once per session and **persist for the session's lifetime** — do not recreate on tab switch.
- Attach/detach via `term.open(el)` / removing the parent node; the `Terminal` object survives detachment.
- Use a `ResizeObserver` on the container, debounce to ~50ms, then call `fitAddon.fit()` and emit `session_resize`.
- Bind PTY data with `onData` → `session_input`; subscribe to `session://output` → `term.write`. Filter events by `sessionId` in the listener.
- Cleanup on session close: dispose terminal, dispose addons, unlisten events.

### React patterns
- Effects do one thing. If you have two unrelated subscriptions, write two `useEffect`s.
- Derive, don't duplicate. If a value can be computed from existing state, compute it in a selector or `useMemo` — don't store it twice.
- Keys on lists are stable IDs (`session.id`), never array index.
- Suspense/error boundaries around the Terminal Viewport so a single misbehaving session can't blank the whole UI.

### Styling (Tailwind)
- Utility-first; extract a component (not an `@apply` class) when a pattern repeats 3+ times.
- Theme tokens (colors, spacing) go in `tailwind.config.js` — no magic hex values in JSX.
- Dark mode via `class` strategy; the root `<html>` class is set from system preference at boot.

### Testing
Frontend-specific principles (procedural detail in the `quality-workflow-gate` skill):
- Vitest + React Testing Library. Test behavior, not implementation (no shallow rendering, no snapshot-as-assertion).
- Mock the `tauri-bridge` module wholesale — never call real `invoke()` from a unit test.

## Cross-boundary contracts

- **Type parity is enforced manually until we automate it.** Whenever you change a Rust struct in `crates/arborist-types/src/lib.rs`, update the matching TS interface in the same commit. Add a `// MIRROR:` comment on the TS side.
- **Async cancellation:** if the frontend navigates away from an in-flight operation, it must still tolerate the eventual response. Never assume the component is still mounted in a `.then()`.
- **Event ordering is not guaranteed across event names.** Within a single event name (e.g., `session://output`), Tauri preserves order per emitter. Don't rely on cross-stream ordering (status vs. output) — design state machines that are robust to either arriving first.

## Shift-left quality (principles)

**Catch problems on the keystroke, not in CI.** The feedback ladder runs fastest → slowest: editor type/lint on save → unit-test watch → pre-commit hook → pre-push hook → CI. Run the editor and watcher loops continuously while coding; never bypass hooks with `--no-verify` on `main`.

For exact commands, watcher setup, Husky configuration, test layout, and end-of-feature smoke tests, **invoke the `quality-workflow-gate` skill**.

## Addressing PR review comments

When the user asks you to address PR review feedback, **invoke the `pr-comments` skill**. It has the exact `gh api` / GraphQL invocations for listing review threads, replying in-thread, and resolving threads.

Two non-negotiable rules from that skill, restated here so they're always in context:

- **Every reply the agent posts on behalf of the user must start with the disclaimer prefix `🤖 AI agent reply (acting for @<gh-user>):` followed by a blank line and the body.** Replace `<gh-user>` literally with the output of `gh api user --jq .login`. No exceptions, including for one-line "done" replies. The comment is attributed to the user's GitHub account; the disclaimer makes AI authorship unambiguous to other reviewers.
- **Resolve a review thread only when the agent actually changed code in response to it.** Questions, declines, deferrals, and "already-done" replies are left open for the human to resolve.

### Test-first defaults
- **Write the failing test before the fix** for every bug. The regression test is the proof the bug existed; without it you've only proven the symptom went away today.
- **Write the test alongside the feature** for new behavior. PRs that add behavior without tests need an explicit waiver.
- **One assertion concept per test.** If you find yourself writing "and also", split the test.
- **Test the seam, not the plumbing** — public function/component/command, not private helpers.
- **Determinism is non-negotiable.** No `sleep()`, no real time, no real network, no real filesystem outside a `tempdir`. Inject clocks/IO.

### What "tested" means before merge
A change is mergeable when **all** of these hold:
1. New/changed behavior has direct test coverage that fails without the change.
2. `pnpm run lint`, `pnpm test`, `cargo clippy -D warnings`, `cargo test` all pass locally.
3. No `// @ts-ignore`, `any`, `.unwrap()`, `.expect()`, `console.log`, or `dbg!()` added without justification in a code comment.
4. If a Rust struct in `crates/arborist-types/src/lib.rs` changed, its TS mirror changed in the same commit.

## Common pitfalls (don't repeat)

- Recomposing the shell command at restart instead of reusing `Session.composedCommand`. **Don't.**
- Interpolating `worktreePath` into the command string. **Don't** — pass as `cwd`.
- Passing `--instructions` to `copilot`. **Don't** — it disables auto-discovery of `.github/copilot-instructions.md`.
- Destroying the xterm Terminal on tab switch. **Don't** — detach from DOM, keep the instance.
- Forgetting to add a new command to `capabilities/main.json` — the call will be rejected at runtime with no compile-time warning.
- Holding a `Mutex` guard across `.await` — deadlocks under load.
- Storing credentials anywhere — auth is the CLI tool's job.
- Accepting a linked git worktree as a workspace root. **Don't** — a workspace root must be a primary clone (`<root>/.git` is a *directory*). Linked worktrees have `.git` as a *file* containing `gitdir: ...` and cannot host their own worktrees, so binding one breaks every session-creation flow downstream. Both `crate::boot::validate_repo_root` and `crate::commands::workspace_validate_impl` enforce this — keep the two in sync. See `docs/worktrees.md` and `workspace_validate` in `docs/architecture.md#command-and-event-contract`.
- Killing the host `arborist` process or its dev-server parents to "clean up" or break a target lock — see "Dogfooding safety". A previous agent crashed the user's editor doing this.
- Putting test-only `[[bin]]` source files in `src-tauri/src/bin/`. **Don't** — Tauri's CLI does an unconditional `read_dir` of `src/bin/` and adds every file there to the bundle binary list using the file basename as the name, ignoring the matching `[[bin]]`'s `required-features = ["test-helpers"]` filter. The result is a `tauri build` that tries to copy a binary that wasn't built (the underscore-named file basename, not the hyphen-named `[[bin]] name`) and fails. Keep test-helper sources under `src-tauri/src/test_bin/` and point the `[[bin]] path` there. See the comment block on the test-helper `[[bin]]` entries in `src-tauri/Cargo.toml`.
