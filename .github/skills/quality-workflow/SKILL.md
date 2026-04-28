---
name: quality-workflow
description: Arborist's procedural quality reference — exact build/lint/test commands, inner-loop watcher setup, Husky pre-commit/pre-push configuration, test architecture rules, and end-of-feature performance/memory smoke tests. Invoke when scaffolding the repo, configuring git hooks, setting up editor watchers, looking up an exact `npm`/`cargo` command, writing or restructuring tests, or verifying a feature is done before merge. The load-bearing *principles* (test-first, determinism, "what done means", pitfalls) live in `.github/copilot-instructions.md` and are always in context — this skill is the procedural lookup that complements them.
---

# Quality workflow — procedural reference

Companion to the **Shift-left quality** principles in `.github/copilot-instructions.md`. Those principles are always loaded; this file holds the commands and configuration detail you only need when actively setting things up or looking something up.

## 1. Build, run, lint, test — command reference

```
npm install                                     # install JS deps
npm run tauri:dev                               # dev build + HMR
npm run tauri:build                             # production bundle
npm run lint                                    # eslint + prettier --check
npm run lint:fix                                # eslint --fix + prettier --write
npm run dev:typecheck                           # tsc --noEmit --watch
npm test                                        # vitest (watch by default in dev)
npm test -- --run                               # vitest single-shot (CI mode)
npm test -- --run --coverage                    # with coverage report
cargo fmt --all -- --check                      # format check (workspace root)
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo watch -C src-tauri -x check -x clippy    # Rust inner loop
```

These commands are wired up in `package.json` and the workspace
`Cargo.toml`; if you find a discrepancy, treat it as a docs bug and
update this skill in the same PR.

## 2. Inner-loop setup (one-time per contributor)

Run continuously while coding so type/lint/test feedback hits in <5 s:

```
# Frontend watchers (run in two terminals)
npm run dev:typecheck    # tsc --noEmit --watch
npm run test:watch       # vitest

# Rust watchers (run in two terminals)
cargo watch -x check -x clippy            # type + lint feedback (workspace root)
cargo watch -x 'test --workspace'         # tests
```

**Editor configuration**: ESLint + Prettier on save, `rust-analyzer` with `clippy` as the check command. No "I'll lint at the end" — by then it's a wall of changes.

## 3. Pre-commit / pre-push hooks (Husky + lint-staged)

Hooks live under `.husky/`. Bypassing with `--no-verify` is allowed only for WIP branches that won't be merged; never on `main`.

- **pre-commit**:
  - `lint-staged` runs `eslint --fix` + Prettier on staged JS/TS
  - `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` on the Rust workspace if any `.rs` file is staged
- **pre-push**:
  - `npm test -- --run` (vitest in CI mode)
  - `cargo test --workspace`

## 4. Test architecture rules

### Rust
- Unit tests in-module: `#[cfg(test)] mod tests { ... }`
- Integration tests in `src-tauri/tests/`
- Filesystem touches: always `tempfile::TempDir`, never the real FS
- Time-dependent async code: `#[tokio::test(flavor = "current_thread", start_paused = true)]` so virtual time advances deterministically
- PTY pool integration tests use a trivial cross-platform child (`echo` on Unix, `cmd /c echo` on Windows) — never depend on `claude`/`copilot` being installed

### Frontend
- Colocate `Foo.test.tsx` next to `Foo.tsx`
- Shared test helpers in `src/test/`
- Hand-written mock at `src/lib/tauri-bridge.mock.ts` exporting the same shape as `tauri-bridge.ts` with `vi.fn()` defaults; tests override per-case
- Component tests for Sidebar, NewSessionDialog, TerminalView (with a mock terminal); hook tests for `use-terminal`
- E2E (post-v1, optional): Tauri's WebDriver integration — gated behind a separate npm script, not part of `npm test`

### General
- **Coverage** is a smell detector, not a target. No percentage gate, but a file <60% line coverage is a yellow flag worth explaining.
- **Flaky tests are bugs.** Quarantine (`.skip` with a linked issue) within the same day; fix or delete within the week. Never retry to green.

## 5. End-of-feature performance & memory smoke tests

Run these before claiming a feature done:

- **Tab-switch leak check**: open 10 sessions, switch tabs rapidly for 30 s — no leaked terminals, RSS stable (Activity Monitor / Task Manager).
- **Backpressure check**: pipe a high-throughput command (`yes` on Unix, `for /l %i in (1,1,1000000) do @echo %i` on Windows) into one session — the UI in other tabs stays responsive (proves the bounded channel works).
