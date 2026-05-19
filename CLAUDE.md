# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What Arborist is

A cross-platform desktop app (Tauri v2 + React/TypeScript) that manages multiple Claude CLI / GitHub Copilot CLI sessions, each bound to a Git worktree. Each session gets its own PTY (persistent across tab switches); the sidebar shows worktree tabs with child sessions and the main area shows a dashboard or one terminal at a time.

Authoritative docs: `docs/product.md` (requirements), `docs/architecture.md` (architecture + data model + command/event API), and `docs/runtime-flows.md` (boot/session/workspace behavior). Engineering conventions live in `.github/copilot-instructions.md`. Read those before proposing structural changes.

## Dogfooding safety — don't kill the host

This repo is dogfooded: the user typically runs the **host** `arborist.exe` (or `arborist` on macOS/Linux) and you, the agent, are executing inside one of its PTY sessions. Killing the host crashes the user's editor and every sibling session, including yours. **A previous agent killed the host this way — do not repeat it.**

Hard rules:

- **Never** terminate `arborist.exe` / `arborist`, or its parent dev processes — `cargo run … arborist`, `pnpm dev` / `pnpm tauri:dev`, `tauri dev`, `pnpm vite`, the Vite dev server, or any `node`/`cargo` process you did not personally spawn in this session. Treat them as the user's running editor.
- **Never** use name-based or pattern-based process kills — `Stop-Process -Name`, `taskkill /IM`, `pkill`, `killall`, `Get-Process … | Stop-Process`. They will sweep up the host.
- **Even with `Stop-Process -Id <PID>`**, only kill PIDs you captured from a child process you started yourself in this same session. If you didn't record the PID at spawn time, don't kill it.
- If `cargo build` / `cargo run` is blocked by a "file in use" / target-locked error, **stop and ask the user** — that lock almost always means the host arborist is running. Do not "free" the lock by killing processes.
- Do not run `pnpm dev`, `pnpm tauri:dev`, `pnpm vite`, or `cargo run -p arborist` "to test changes" unless the user explicitly asks. The user already has it running. Use `cargo build`, `cargo test`, `pnpm run build`, or `pnpm test --run` for verification instead.

If a task genuinely requires restarting the host, ask the user to do it — never do it yourself.

## Stack

- **Frontend**: React + TypeScript, Vite, Tailwind CSS (class dark-mode strategy), Zustand, xterm.js
- **Backend**: Rust, Tauri v2, `portable-pty` (ConPTY on Windows), custom JSON persistence via `config_store.rs`
- **Layout**: `src/` (frontend), `src-tauri/src/` (Rust), `crates/arborist-types/` (wire types), `docs/` (project docs)

## Commands

### Run / build

```sh
pnpm dev                 # Vite + Tauri with HMR (frontend) and hot-recompile (backend); alias: pnpm tauri:dev
pnpm vite                # Vite dev server only (no Tauri shell) — useful for browser-only iteration on UI bits
pnpm tauri:build         # production bundle → target/release/bundle/
```

`pnpm dev` runs `scripts/tauri-dev.mjs`, which picks a per-worktree devserver port, hands it to Vite via env, and tells Tauri to load the matching URL. Tauri's `beforeDevCommand` is `pnpm run vite` so the frontend is started automatically — do not change `pnpm dev` to invoke `vite` directly without also updating `tauri.conf.json` (otherwise Tauri will recurse into itself and the frontend will never come up).

### Lint / format / type-check

```sh
pnpm run lint            # eslint + prettier --check
pnpm run lint:fix        # auto-apply
pnpm run dev:typecheck   # tsc --noEmit --watch (run continuously while coding)
cargo fmt --all -- --check
cargo fmt --all
cargo clippy --workspace --all-targets --features test-helpers -- -D warnings
```

### Test

```sh
pnpm test                         # vitest watch mode (inner loop)
pnpm test --run                # vitest once (CI / pre-push)
cargo test --workspace --features test-helpers  # unit + integration tests; also builds arborist-test-child
cargo test --workspace --features test-helpers <name>  # single Rust test by name prefix
```

### Acceptance gate (all must be green before merge)

```sh
pnpm run lint && pnpm test --run && pnpm run build
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --features test-helpers -- -D warnings && cargo test --workspace --features test-helpers
```

### Debugging helpers

```sh
RUST_LOG=arborist_lib=debug pnpm dev             # verbose backend tracing
cargo run -p arborist --example config_smoke    # config-store end-to-end without Tauri
cargo run -p arborist --features test-helpers --bin arborist-test-child # poke the PTY test child interactively
```

## Architecture

### Two-process model

Rust backend owns all PTYs and persistent state; the React frontend communicates exclusively through Tauri commands (`invoke`) and events (`listen`). **No React component imports `@tauri-apps/api` directly** — everything goes through `src/lib/tauri-bridge.ts`, which has one typed wrapper per command/event.

### Key backend modules (`src-tauri/src/`)

| Module                | Responsibility                                                                                                                                                   |
| --------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `lib.rs`              | App entry: init tracing, build `AppContext`, register commands, run event loop                                                                                   |
| `types.rs`            | All serde types: `Session`, `AppConfig`, errors, event payloads. **Canonical** — TS mirrors must stay in lockstep                                                |
| `compose.rs`          | Pure functions: `compose_command`, `dedupe_label`, shell quoting, worktree validation                                                                            |
| `config_store.rs`     | Custom JSON store; atomic writes via `NamedTempFile::persist`; config quarantine on parse failure                                                                |
| `pty_pool.rs`         | PTY lifecycle, `PtySpawner` trait (injectable for tests), bounded mpsc backpressure (`OUTPUT_CHANNEL_CAPACITY = 512`), `ESC c` reset on overflow, orphan cleanup |
| `commands/session.rs` | All real handler logic (thin wrappers in `commands/mod.rs` delegate here)                                                                                        |
| `git.rs`              | `GitRunner` trait + `RealGitRunner`; parses `git worktree list --porcelain`                                                                                      |

### Key frontend modules (`src/`)

| Module                     | Responsibility                                                                                                                                 |
| -------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| `lib/tauri-bridge.ts`      | Single import surface for all `invoke`/`listen` calls                                                                                          |
| `lib/tauri-bridge.mock.ts` | Vitest mock; `satisfies typeof realBridge` — adding an export without mirroring is a compile error                                             |
| `hooks/use-terminal.ts`    | One `Terminal` per `sessionId` in a module-level `Map`; attach/detach to DOM on tab switch (never recreate); `ResizeObserver` debounced ~50 ms |
| `store/session-store.ts`   | Zustand session state. **Output bypasses Zustand** — it goes straight from `session://output` events to xterm                                  |
| `lib/session-events.ts`    | App-level `session://status` subscriber → session store                                                                                        |
| `types/arborist.ts`        | TS mirrors of every Rust type in `types.rs` (each carries a `// MIRROR:` comment)                                                              |

### Critical invariants

- **Compose once, reuse forever.** `Session.composedCommand` is built at create time and stored. `session_restart` and `restore_all_sessions` re-use it verbatim — never recompose at restart.
- **`cwd` is a discrete spawn field, never interpolated.** `portable-pty` receives `worktreePath` as `cwd`; `compose.rs` tests assert no `cd "<path>" &&` ever appears.
- **Capability gating.** Every `#[tauri::command]` requires a matching `permissions/*.toml` + entry in `capabilities/main.json`. Missing capability = silent runtime rejection. `tests/capability_gating.rs` is a structural assertion keeping these in sync.
- **Type parity is manual.** When a Rust struct in `types.rs` changes, update the matching TS interface in `types/arborist.ts` in the same commit.

### Adding a new Tauri command (checklist)

1. Handler in `commands/mod.rs` + logic in `commands/session.rs`
2. Register in `tauri::generate_handler![…]` in `lib.rs`
3. Create `permissions/allow-<name>.toml`
4. Add `"allow-<name>"` to `capabilities/main.json`
5. Extend `tests/capability_gating.rs`
6. Add typed wrapper to `tauri-bridge.ts` and stub to `tauri-bridge.mock.ts`
7. Update `docs/architecture.md#command-and-event-contract`

### Test seams

- **`PtySpawner` trait** — production uses `PortablePtySpawner`; tests inject `FakePtySpawner`
- **`GitRunner` trait** — production uses `RealGitRunner`; tests inject canned porcelain output
- **`tauri-bridge.mock.ts`** — frontend tests mock the entire bridge module via `vi.mock('@/lib/tauri-bridge', ...)`
- **`arborist-test-child` binary** — PTY integration tests use this instead of real `claude`/`copilot`. Tests redirect compose output to it via `pluginSettings.ai[pluginId].settings.launchCommand` (Rust integration tests set the field directly through `PartialAppConfig`; the Linux e2e harness uses the boot CLI flags `--ai-launch-claude=<path>` / `--ai-launch-copilot=<path>`)

### Tool-specific CLI rules (easy to get wrong)

- **Claude**: do **not** pass `--system-prompt` for new sessions. Claude auto-discovers `CLAUDE.md` from the worktree `cwd`.
- **Claude hooks**: when the `arborist-claude-hook` sidecar binary is found next to the running `arborist` (release bundle or `cargo build` artifact), each Claude session also gets a `--settings <temp-file>` pointing at a per-session `claude-settings.json` (same temp dir). The file registers the sidecar against PreToolUse/PostToolUse/PostToolUseFailure/PermissionRequest/UserPromptSubmit/Stop/SessionEnd, plus `Notification` scoped to the `idle_prompt` matcher (Claude is idle waiting on the user — drives the sidebar `attention` cue). The user's own `~/.claude/settings.json` and project `.claude/settings*.json` are merged in at session-create time so user formatters / validators keep running alongside Arborist's — note this is a shallow merge that matches Claude's documented `--settings` precedence (last file wins on every top-level key except `hooks.<EventName>` arrays, which are concatenated; nested objects like `permissions` / `mcpServers` are not deep-merged). If the sidecar isn't locatable (e.g. dev runs where the helper wasn't built, partial installs), `--settings` is silently omitted and Claude launches without hook integration — the sidebar falls back to PTY-byte heuristics. The sidebar's `awaitingPermission`/`runningTool`/`thinking`/`awaiting`/`attention` icons all flow from this hook pipeline — see `src-tauri/src/plugins/ai/claude/hooks.rs` and `src-tauri/src/claude_hook_events.rs`.
- **Claude session id**: pre-allocated at create time (mirrors Copilot). The persisted `composed_command` stays bare; `compose::with_first_launch_session_id` splices `--session-id <uuid>` on the very first spawn (so Claude creates the conversation at our uuid); `compose::with_resume` splices `--resume <uuid>` on every subsequent spawn (restart / restore-on-launch).
- **Copilot**: do **not** pass `--instructions` — it disables auto-discovery of `.github/copilot-instructions.md`.

### Code conventions

- **Line length is 150 characters** for all code and comments, in both Rust and TypeScript. Enforced by `rustfmt.toml` (`max_width = 150`) and Prettier (`.prettierrc.json`, `printWidth: 150`).
- No `any` in TypeScript; no `.unwrap()`/`.expect()` in Rust outside tests.
- Zustand selectors are granular: `useSessionStore(s => s.tabs)`, never `useStore(s => s)`.
- Rust command handlers are always `async fn`, return `Result<T, AppError>` (AppError is `serde::Serialize`), and contain no business logic — delegate to `commands/session.rs`.
- Event names use `://` namespace (e.g., `session://output`); payloads are always named-field structs.
- Path alias `@/*` → `src/*`; no deep relative imports.

## Pull requests

- **PR title prefix.** Always prefix PR titles with the worktree name in square brackets, falling back to the branch name when no worktree is in use: `[<worktree-or-branch>] <summary>`. The worktree name is the basename of the current worktree directory (e.g., `.worktrees/pr-prefix` → `[pr-prefix]`). Apply this to every `gh pr create` invocation.
- Do not push directly to `main` and do not force-push shared branches; land changes through PRs.

## Skills (read on demand)

Claude does not auto-discover skill files. When a task matches one of these, **read the linked file before acting**:

- **Addressing PR review comments / responding to PR feedback** → read `.github/skills/pr-comments/SKILL.md`. Covers the exact `gh api` / GraphQL calls for listing review threads, replying in-thread (with the **mandatory `🤖 AI agent reply (acting for @<gh-user>):` disclaimer prefix**, where `<gh-user>` is replaced literally with the output of `gh api user --jq .login`), and the resolve-only-when-code-changed policy.
- **Build / lint / test command lookup, Husky hooks, test architecture** → read `.github/skills/quality-workflow-gate/SKILL.md`.
