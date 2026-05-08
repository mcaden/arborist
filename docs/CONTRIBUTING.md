# Contributing to Arborist

Thanks for contributing. This guide covers the PR workflow, code conventions, and the checklist for common structural changes.

## Before you start

Read the spec and architecture docs — they are the source of truth:

- `dev/docs/SPEC.md` — product requirements. If code disagrees with the spec, fix the code (or update the spec in the same PR — never let them drift silently).
- `dev/docs/DESIGN.md` — architecture, data model, the full command/event API (§6), and per-tool CLI launch rules (§5.6).
- `.github/copilot-instructions.md` — the detailed engineering conventions this project follows.

## Workflow

- Work on a feature branch; open a PR to `main`. Do not push directly to `main` or force-push shared branches.
- The acceptance gate must pass locally before the PR is ready for review (see below).
- Pre-commit and pre-push hooks enforce a subset of this automatically via Husky.

## Acceptance gate

Every change is mergeable only when **all six** are green:

```sh
pnpm run lint
pnpm test --run
pnpm run build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --features test-helpers -- -D warnings
cargo test --workspace --features test-helpers
```

Additionally:

- New or changed behaviour has direct test coverage that fails without the change.
- The app launches via `pnpm dev` (or its alias `pnpm tauri:dev`) and the touched flow works end-to-end at least once.
- No `// @ts-ignore`, `any`, `.unwrap()`, `.expect()` (outside tests/infallible invariants), `console.log`, or `dbg!()` added without a justifying code comment.
- If a Rust struct in `types.rs` changed, its TypeScript mirror in `src/types/arborist.ts` changed in the same commit.

## Code conventions (abbreviated)

The full conventions are in `.github/copilot-instructions.md`. Key rules:

**Rust**

- No business logic in `#[tauri::command]` handlers — handlers deserialize args, call into the relevant module, map errors.
- All command handlers are `async fn` and return `Result<T, AppError>` where `AppError: serde::Serialize`.
- Never hold a `Mutex` guard across an `.await` you don't control.
- PTY read loops run on `std::thread::spawn` (blocking IO), not tokio tasks.

**TypeScript / React**

- All `invoke()` / `listen()` calls go through `src/lib/tauri-bridge.ts`. No React component imports `@tauri-apps/api` directly.
- Zustand selectors are granular: `useSessionStore(s => s.tabs)`, never `useStore(s => s)`.
- `use-terminal.ts` creates one `Terminal` instance per session and keeps it alive across tab switches — never recreate on tab switch.
- `strict: true` in tsconfig with `noUncheckedIndexedAccess` and `exactOptionalPropertyTypes`. No `// @ts-ignore` without an issue link.

**Cross-boundary**

- Rust types in `types.rs` use `#[serde(rename_all = "camelCase")]`; TS mirrors carry a `// MIRROR: src-tauri/src/types.rs::TypeName` comment.
- Event names use `://` namespacing (e.g., `session://output`); payloads are always named-field structs.

## Adding a new Tauri command

Follow this checklist in order (the structural test in `src-tauri/tests/capability_gating.rs` will fail if any step is skipped):

1. Write the handler logic in `src-tauri/src/commands/session.rs`.
2. Add a thin `#[tauri::command]` wrapper in `src-tauri/src/commands/mod.rs`.
3. Register it in `tauri::generate_handler![…]` in `src-tauri/src/lib.rs`.
4. Create `src-tauri/permissions/allow-<command-name>.toml` declaring `commands.allow = ["<command_name>"]`.
5. Add `"allow-<command-name>"` to the `permissions[]` array in `src-tauri/capabilities/main.json`.
6. Extend `src-tauri/tests/capability_gating.rs` to assert the new permission exists.
7. Add a typed wrapper to `src/lib/tauri-bridge.ts`.
8. Add a matching stub to `src/lib/tauri-bridge.mock.ts` (the `satisfies typeof realBridge` assertion at the bottom catches omissions at compile time).
9. Update the command/event table in `dev/docs/DESIGN.md` §6.

## Changing the persisted config shape

1. Update `AppConfig` / `PartialAppConfig` in `src-tauri/src/types.rs`.
2. Mirror the change in `src/types/arborist.ts`.
3. Bump `CONFIG_VERSION_CURRENT` in `config_store.rs` and add a migration if old data needs transforming.

## Testing

**Write the failing test before the fix** for every bug. **Write the test alongside the feature** for new behaviour. See `dev/docs/TESTING.md` for the full test guide including the seam architecture, the `arborist-test-child` binary, and the end-of-feature smoke-test procedure.

Quick rules:

- Rust filesystem touches use `tempfile::TempDir`.
- Time-sensitive async tests use `#[tokio::test(flavor = "current_thread", start_paused = true)]`.
- Frontend tests mock the tauri-bridge wholesale: `vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'))`.
- No `sleep()`, no real network, no real filesystem outside `tempfile::TempDir`.

## Common pitfalls

- **Recomposing the shell command on restart.** Don't — reuse `Session.composedCommand` verbatim (DESIGN §5.4).
- **Interpolating `worktreePath` into the command string.** Don't — pass as `cwd` to `portable-pty` (DESIGN §8.1).
- **Passing `--instructions` to `copilot`.** Don't — it disables auto-discovery of `.github/copilot-instructions.md` (DESIGN §5.6).
- **Destroying the xterm `Terminal` on tab switch.** Don't — detach from the DOM, keep the instance alive (SPEC T-03).
- **Forgetting to add a new command to `capabilities/main.json`.** The call will be silently rejected at runtime with no compile-time warning.
- **Storing credentials anywhere.** Auth is the CLI tool's job (SPEC NF-05).
