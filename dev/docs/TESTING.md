# Arborist — testing guide

How tests are organised, what the seams are, how to write new ones, and
what the smoke procedure looks like.

> Companion: [`DEVELOPMENT.md`](./DEVELOPMENT.md) for setup,
> [`ARCHITECTURE.md`](./ARCHITECTURE.md) for the module map, and
> [`.github/copilot-instructions.md`](../../.github/copilot-instructions.md)
> for the load-bearing principles (test-first, determinism, "what done
> means").

## 1. Layout

### Rust

| Location                                    | What lives there                                                                       |
| ------------------------------------------- | -------------------------------------------------------------------------------------- |
| `src-tauri/src/<module>.rs::tests`          | Unit tests for pure logic — composition, label dedup, validation, type round-trips.    |
| `src-tauri/tests/*.rs`                      | Cargo integration tests against the public crate surface.                              |
| `src-tauri/src/test_bin/arborist_test_child.rs`     | Deterministic child binary used by `pty_pool` integration tests. Lives outside `src/bin/` so Tauri's CLI doesn't pick it up as a bundle binary — see the comment on the matching `[[bin]]` in `src-tauri/Cargo.toml`. |
| `src-tauri/examples/config_smoke.rs`        | End-to-end harness for the config store — useful for manual debugging, runnable via `cargo run --example config_smoke`. |

The integration tests in `src-tauri/tests/` are:

| File                                | What it covers                                                                                       |
| ----------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `pty_pool.rs`                       | PTY lifecycle: spawn → echo → resize → exit; backpressure with drop-newest + `ESC c` reset; UTF-8 split-character safety; orphan cleanup; wait-thread persistence. |
| `session_lifecycle_fake.rs`         | Full command surface against a `FakePtySpawner` — no real CLI, no real PTY. Restore-on-launch, restart byte-identical to stored `composedCommand`, dead-worktree handling, idempotent `frontend_ready`. |
| `session_lifecycle_real.rs`         | One happy-path round-trip through `PortablePtySpawner` + the `arborist-test-child` binary, proving end-to-end wiring. |
| `worktrees_command.rs`              | `git worktree list --porcelain` parsing via the `GitRunner` seam; missing-git, non-repo, and permission-error paths.    |
| `capability_gating.rs`              | Structural assertion that every `#[tauri::command]` has a matching `permissions/*.toml` referenced from `capabilities/main.json`. See the file's header for why this is structural rather than a runtime negative test. |

### Frontend

| Location                              | What lives there                                                                       |
| ------------------------------------- | -------------------------------------------------------------------------------------- |
| `src/**/Foo.test.tsx`                 | Component / hook / store tests colocated next to their subject.                        |
| `src/test/setup.ts`                   | Global Vitest setup — `@testing-library/jest-dom` matchers, etc.                       |
| `src/lib/tauri-bridge.mock.ts`        | Hand-written mock module structurally substitutable for `tauri-bridge.ts`.             |
| `src/types/fixtures/`                 | JSON fixtures shared between Rust serde snapshots and TS `satisfies` checks.           |

## 2. Seams (how tests avoid touching the OS)

Arborist takes determinism seriously. Three injectable seams keep tests off
the real filesystem, the real PTY, and the real `git`.

### `PtySpawner` (Rust — `src-tauri/src/pty_pool.rs`)

```rust
pub trait PtySpawner: Send + Sync { /* spawn(cmd, cwd, size) */ }
pub struct PortablePtySpawner; // production
```

`PtyPool::new(spawner: Arc<dyn PtySpawner>)` takes the spawner by injection.
Production wires `PortablePtySpawner` in `lib.rs::run`; integration tests
use `FakePtySpawner` (defined inline in `tests/session_lifecycle_fake.rs`)
that records calls and lets the test drive child exit codes deterministically.

### `GitRunner` (Rust — `src-tauri/src/git.rs`)

```rust
pub trait GitRunner: Send + Sync { /* run("worktree list --porcelain", cwd) */ }
pub struct RealGitRunner; // production
```

`worktrees_list_impl` consults the runner so unit tests can return canned
porcelain output without a real `git` binary.

### `tauri-bridge.mock.ts` (Frontend)

Structurally typed against `tauri-bridge.ts` via a `satisfies typeof realBridge`
assertion at the bottom of the file — adding a new bridge export without
mirroring it here is a TypeScript compile error. Tests opt in with:

```ts
vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));
```

Then per-test, override `vi.fn` return values on the imported module. Every
default mock rejects with `not implemented` so a forgotten
`mockResolvedValue` surfaces loudly.

## 3. The `arborist-test-child` binary

`src-tauri/src/test_bin/arborist_test_child.rs` is a tiny purpose-built child
process used by the PTY-pool integration tests so cross-platform tests
don't depend on `claude` / `copilot` being installed.

Behaviour:

- prints a banner on startup,
- echoes stdin to stdout line-by-line,
- exits 0 on `quit\n`,
- exits with code N on `exit N\n`.

Cargo builds the binary when the `test-helpers` feature is enabled and exposes
its full path to integration tests via the `CARGO_BIN_EXE_arborist-test-child`
environment variable:

```sh
cargo test --workspace --features test-helpers
```

To poke at it manually:

```sh
cargo run -p arborist --features test-helpers --bin arborist-test-child
```

## 4. Test-only seam: CLI program override

`compose::cli_program_for_tool` honours the user-facing
`AppConfig.ai_launch_commands` field as a verbatim shell-snippet override of
the bare `claude` / `copilot` program token. Tests use the same plumbing real
users get via the Settings dialog — there is no environment-variable seam.

| Surface                                             | Mechanism                                                                                |
| --------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| `session_lifecycle_real.rs` (Rust integration test) | `store.save_config({ ai_launch_commands: { claude: <test-child path> } })` before create |
| Linux e2e (`AppImage` under tauri-driver)           | `--ai-launch-claude=<path>` / `--ai-launch-copilot=<path>` CLI flags (see `boot.rs`)     |

The override path is encoded verbatim into the persisted `composedCommand`;
restarting still spawns the literal path (it does **not** fall back to
`claude` / `copilot`). This is documented in `DESIGN.md` §6 too.

## 5. Test-writing rules

Quoting `.github/copilot-instructions.md`:

- **Write the failing test before the fix** for every bug.
- **Write the test alongside the feature** for new behaviour.
- **One assertion concept per test.**
- **Test the seam, not the plumbing** — public function / component /
  command, not private helpers.
- **Determinism is non-negotiable.** No `sleep()`, no real time, no real
  network, no real filesystem outside a `tempfile::TempDir`. Inject
  clocks / IO.

Practical:

- Rust filesystem touches use `tempfile::TempDir` exclusively.
- Time-sensitive async code runs under
  `#[tokio::test(flavor = "current_thread", start_paused = true)]` so
  virtual time advances deterministically.
- React component tests favour `@testing-library/react` queries
  (`getByRole`, `getByLabelText`) over implementation details (no shallow
  rendering, no snapshot-as-assertion).
- Coverage is a smell detector, not a target. A file under 60% line
  coverage is a yellow flag worth explaining; there is no percentage gate.
- Flaky tests are bugs. Quarantine within the day, fix or delete within
  the week. Never retry to green.

## 6. Capability gating

Tauri v2 rejects `invoke()` for any command that lacks a permission entry
in `src-tauri/capabilities/main.json` referencing a file in
`src-tauri/permissions/`. Adding a `#[tauri::command]` without that
plumbing produces no compile-time error — it only fails at runtime.

`tests/capability_gating.rs` defends against that by structurally
asserting the checked-in capability JSON contains every expected
permission and that the corresponding `permissions/*.toml` files exist
and declare the right `commands.allow` list. The header comment explains
why this is structural (Tauri 2.x doesn't expose runtime capability
override yet) and tracks the upgrade path.

When you add a new `#[tauri::command]`:

1. Define the handler in `src-tauri/src/commands/mod.rs`.
2. Register it in the `tauri::generate_handler![…]` list in
   `src-tauri/src/lib.rs`.
3. Create `src-tauri/permissions/allow-<name>.toml` declaring
   `commands.allow = ["<name>"]`.
4. Add `"allow-<name>"` to `permissions[]` in
   `src-tauri/capabilities/main.json`.
5. Extend `tests/capability_gating.rs` to check for the new permission.
6. Add the typed wrapper to `src/lib/tauri-bridge.ts` and the matching
   stub to `src/lib/tauri-bridge.mock.ts`.
7. Update DESIGN §6's command table.

## 7. End-of-feature smoke tests

Two manual smoke tests close the gap that unit + integration tests can't
cover (real OS WebView + Task Manager-level RSS):

- **Tab-switch leak check** — open 10 sessions, switch tabs rapidly for
  30 s, RSS plateaus, no leaked xterm `Terminal` instances.
- **Backpressure check** — pump high-throughput output into one session
  (e.g. `for /l %i in (1,1,1000000) do @echo %i` on Windows), confirm
  other sessions stay interactive; the PTY pool's drop-newest policy
  fires at 256-drop intervals via `tracing::warn!`.

Step-by-step procedures, RSS columns to record, and DevTools snippets are
in [`../ai/SMOKE_TEST_RESULTS.md`](../ai/SMOKE_TEST_RESULTS.md). A
maintainer must run them on a real OS and fill in the RESULTS section
before declaring a release-quality build.
