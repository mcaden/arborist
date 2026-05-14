# Contributing

Thanks for contributing to Arborist. This guide is written for public contributors and maintainers.

## Before you start

Read:

- [overview](docs/overview.md) for the mental model.
- [product](docs/product.md) for the product contract and scope.
- [architecture](docs/architecture.md) for the codebase and command/event contract.
- [development](docs/development.md) for setup and commands.
- [testing](docs/testing.md) for test seams and expectations.
- [SECURITY](SECURITY.md) before reporting or fixing vulnerabilities.

## Issue workflow

- Search existing issues before opening a new one.
- Use a bug report for reproducible broken behavior.
- Use a feature request for new behavior or user experience changes.
- Do not post secrets, tokens, private repository paths, logs with credentials, or exploit details in public issues.
- Security issues should follow [SECURITY](SECURITY.md), not the public issue tracker.

Good bug reports include:

- OS and Arborist version or commit.
- Whether the build is a release build or dev/worktree build.
- Steps to reproduce.
- Expected and actual behavior.
- Relevant logs with secrets removed.

## Branch and PR workflow

- Work on a branch and open a PR to `main`.
- Do not push directly to `main`.
- Do not force-push shared branches.
- Keep PRs focused. Split unrelated changes.
- Include tests for new or changed behavior.
- Update docs when behavior, configuration, commands, events, or workflows change.
- For PR titles created by automation in this repository, prefix the worktree or branch name in square brackets.

## Acceptance gate

Run the relevant checks before marking a PR ready:

```sh
pnpm run lint
pnpm test --run
pnpm run build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --features test-helpers -- -D warnings
cargo test --workspace --features test-helpers
```

Docs-only changes should at minimum pass Markdown formatting and stale-link/path searches. Run the full gate when docs edits touch code comments,
scripts, generated examples, or anything that can affect compilation.

## Coding expectations

### Cross-cutting

- Keep behavior aligned with [product](docs/product.md).
- Keep the command/event API aligned with [architecture](docs/architecture.md#command-and-event-contract).
- No credentials in source, logs, examples, fixtures, or docs.
- Prefer small, explicit errors over silent fallbacks.
- Preserve type safety. Avoid TypeScript `any` and Rust `.unwrap()`/`.expect()` outside tests and truly infallible invariants.

### Rust

- Command wrappers stay thin; business logic belongs in modules under `src-tauri/src/commands/` or dedicated helpers.
- Use `Result<T, AppError>` at the Tauri boundary.
- Do not hold locks across uncontrolled `.await` points.
- Blocking PTY reads run on OS threads, not async tasks.
- Keep new serialized types in `crates/arborist-types` and mirror them in `src/types/arborist.ts`.

### TypeScript and React

- Components use typed props and functional components.
- All Tauri calls go through `src/lib/tauri-bridge.ts`.
- Mock the bridge wholesale in tests.
- Zustand selectors should be granular.
- Terminal instances persist for the session lifetime; do not recreate them on tab switch.
- Keep event listener cleanup explicit.

## Adding a Tauri command

Update every layer in one PR:

1. Rust payload/result type in `crates/arborist-types/src/lib.rs` if needed.
2. Command wrapper in `src-tauri/src/commands/mod.rs`.
3. Business logic in the appropriate backend module.
4. Handler registration in `src-tauri/src/lib.rs`.
5. Permission file under `src-tauri/permissions/`.
6. Capability entry in `src-tauri/capabilities/main.json`.
7. Capability-gating test.
8. Typed wrapper and mock in `src/lib/tauri-bridge.ts` and `src/lib/tauri-bridge.mock.ts`.
9. TypeScript mirrors in `src/types/arborist.ts`.
10. Command table in [architecture](docs/architecture.md#command-and-event-contract).

## Changing config or persisted types

1. Update `crates/arborist-types/src/lib.rs`.
2. Update `src/types/arborist.ts`.
3. Add or adjust migrations in `config_store.rs`.
4. Bump `CONFIG_VERSION_CURRENT` if the persisted shape changes.
5. Update [configuration](docs/configuration.md).
6. Add tests for old and new shapes.

## Review expectations

Reviewers should focus on correctness, safety, test coverage, cross-boundary type parity, and whether docs stay aligned with behavior. Style-only
feedback should be limited to cases where it affects maintainability or violates established tooling.

Maintainers may ask contributors to split PRs, add tests, redact logs, or move security-sensitive details to the private vulnerability process.
