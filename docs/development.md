# Development

This guide covers local setup, common commands, CI, debugging, and troubleshooting. For architecture context, read [architecture](./architecture.md)
and [runtime flows](./runtime-flows.md).

## Prerequisites

| Tool             | Version                       | Notes                                                        |
| ---------------- | ----------------------------- | ------------------------------------------------------------ |
| Node.js          | 20+; repo pins 24 in `.nvmrc` | CI currently uses Node 22. Local `nvm use` follows `.nvmrc`. |
| pnpm             | 10.33.0                       | Declared in `package.json`. Use pnpm, not npm.               |
| Rust             | stable                        | Toolchain is pinned by `rust-toolchain.toml`.                |
| Git              | 2.30+                         | Required at runtime for worktree operations.                 |
| Tauri build deps | platform-specific             | Follow Tauri v2 prerequisites for your OS.                   |

Platform notes:

- Windows: Visual Studio 2022 Desktop development with C++ workload and Windows 10/11 SDK. WebView2 is preinstalled on most modern Windows systems.
- macOS: `xcode-select --install`; install `llvm` with Homebrew so `lld` is available for the configured linker.
- Linux: install `clang`, `lld`, GTK/WebKit2GTK, libsoup, AppIndicator, librsvg, libxdo, OpenSSL, and build essentials.

The real `claude` and `copilot` CLIs are not required to build or test. Integration tests use in-tree helper binaries.

## First-time setup

```sh
git clone https://github.com/mcaden/arborist.git
cd arborist
nvm use
pnpm install
```

`pnpm install` runs the Husky prepare script and installs hooks under `.husky/`.

## Day-to-day commands

### Run and build

```sh
pnpm dev
pnpm vite
pnpm tauri:build
```

`pnpm dev` runs `scripts/tauri-dev.mjs`, which chooses a per-worktree devserver port and launches Tauri. Tauri's `beforeDevCommand` is
`pnpm run vite`; keep `package.json` scripts and `src-tauri/tauri.conf.json` in sync if either changes.

Do not use `pnpm dev` as a verification command inside a running Arborist dogfood session unless you intentionally need to start a second app
instance. Prefer build/test commands.

### Lint, format, and type-check

```sh
pnpm run lint
pnpm run lint:fix
pnpm run dev:typecheck
cargo fmt --all -- --check
cargo fmt --all
cargo clippy --workspace --all-targets --features test-helpers -- -D warnings
```

### Test

```sh
pnpm test
pnpm test --run
cargo test --workspace --features test-helpers
cargo test --workspace --features test-helpers <test-name-prefix>
```

### Dependency audits

```sh
pnpm audit --prod --audit-level=moderate
pnpm audit --audit-level=high || true
cargo install --locked --version 0.19.6 cargo-deny
cargo deny check advisories licenses
```

Policy:

- Production dependency vulnerabilities (`pnpm audit --prod`) at `moderate` or higher are blocking and should be fixed before merge/release.
- Development dependency vulnerabilities (`pnpm audit`) are non-blocking unless they impact shipped artifacts; track remediation through follow-up issues.
- `pnpm audit --audit-level=high` exits non-zero when findings exist. Use `|| true` when you want report-only output during routine checks.
- Rust dependency advisories/licenses are enforced by `cargo deny check advisories licenses`.

### Acceptance gate

Before a PR is ready for review, run the applicable local gate:

```sh
pnpm run lint
pnpm test --run
pnpm run build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --features test-helpers -- -D warnings
cargo test --workspace --features test-helpers
```

Documentation-only changes normally need Markdown formatting and link/path checks, not full app test suites, unless they modify generated docs,
examples, scripts, or code comments in a way that could affect compilation.

## Git hooks

Husky hooks are installed by `pnpm install`.

| Hook       | What it runs                                                                              |
| ---------- | ----------------------------------------------------------------------------------------- |
| pre-commit | `lint-staged` on staged JS/TS/JSON/CSS/MD; Rust format/clippy when Rust files are staged. |
| pre-push   | `pnpm test --run` and `cargo test --workspace --features test-helpers`.                   |

Do not bypass hooks on `main`. If you must use `--no-verify` on a personal WIP branch, run the skipped checks before opening a PR.

## CI

| Workflow               | Trigger                                                             | Purpose                                                                           |
| ---------------------- | ------------------------------------------------------------------- | --------------------------------------------------------------------------------- |
| `ci.yml`               | Pull requests and pushes to `main`                                  | Frontend install, lint, and Vitest run.                                           |
| `dependency-audit.yml` | Weekly schedule, manual dispatch, and dependency-related pushes/PRs | Runs Rust advisory/license checks (`cargo deny`) and production npm audit.        |
| `rust-gate.yml`        | PR review approval                                                  | Dispatches the Rust workflow against the PR head branch for same-repo PRs.        |
| `rust.yml`             | Manual dispatch, or via rust gate                                   | Multi-platform Rust format, clippy, and tests after building the frontend bundle. |
| `release.yml`          | Manual dispatch with an existing tag                                | Builds draft release artifacts and GitHub build attestations.                     |

The Rust workflow is approval-gated because it is heavier and multi-platform. Reviewers should ensure it ran against the head SHA they approved.

Workflow `uses:` dependencies are pinned to full commit SHAs. Each pin keeps an inline comment with the intended upstream action ref, and Dependabot is
configured to open weekly update PRs for GitHub Actions, npm, and Cargo dependencies. Review those PRs like code changes: confirm each update is
expected for the ecosystem/group, keep inline action-ref comments accurate, and run the relevant workflows before merging.

## Debugging

### Frontend

- Open WebView DevTools from the running app where the platform allows it.
- Inspect Zustand stores from the console if imported by the bundle.
- For terminal lifecycle issues, start with `src/hooks/use-terminal.ts` and its tests.
- For workspace switch adoption, start with `src/lib/workspace-switch.ts`.

### Backend

```sh
RUST_LOG=arborist_lib=debug pnpm dev
RUST_LOG=trace pnpm dev
cargo run -p arborist --example config_smoke
cargo run -p arborist --features test-helpers --bin arborist-test-child
```

Do not kill `arborist`, `cargo`, `node`, Vite, or Tauri processes you did not start in the current session. If a Cargo command is blocked by a target
lock or file-in-use error, ask the user to stop the running host app.

### Persistent state

Config and sessions live under the per-branch/per-workspace app-data layout documented in [configuration](./configuration.md). Quarantined files are
renamed to `.bad-<timestamp>` and logged with `ConfigQuarantined`.

## Troubleshooting

| Symptom                                            | Likely fix                                                                   |
| -------------------------------------------------- | ---------------------------------------------------------------------------- |
| `cl.exe` linker failure on Windows                 | Install the Visual Studio C++ workload.                                      |
| GTK/WebKit package error on Linux                  | Install Tauri's Linux build dependencies.                                    |
| Blank dev window                                   | Open DevTools and inspect frontend boot errors.                              |
| Rust test tries to call real `claude` or `copilot` | File a bug; tests should use helper binaries or command overrides.           |
| Hooks do not run                                   | Re-run `pnpm install`.                                                       |
| `config.json.bad-*` appears                        | Fix or delete the quarantined file; see [configuration](./configuration.md). |
| Workspace cannot bind                              | Another Arborist instance may hold the same `(branch, workspace)` lock.      |
| Application sub-tab will not focus on Linux        | Install `wmctrl` for X11. Wayland focus is unsupported by design.            |
