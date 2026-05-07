# Local development

Everything you need to get a working dev environment, understand the inner loop, and debug problems.

> Architecture overview: [dev/docs/ARCHITECTURE.md](../dev/docs/ARCHITECTURE.md)
>
> Config format: [dev/docs/CONFIGURATION.md](../dev/docs/CONFIGURATION.md)
>
> Test guide: [dev/docs/TESTING.md](../dev/docs/TESTING.md)

## Prerequisites

| Tool                 | Version  | Notes                                                                                  |
| -------------------- | -------- | -------------------------------------------------------------------------------------- |
| **Node.js**          | 20 LTS+  | Repo pins Node 24 via `.nvmrc` — `nvm use` picks it up                                 |
| **Rust**             | stable   | Install via [rustup.rs](https://rustup.rs/). Toolchain pinned by `rust-toolchain.toml` |
| **Git**              | 2.30+    | Required at runtime for `git worktree list` discovery                                  |
| **Tauri build deps** | platform | See below                                                                              |

Platform build tools:

- **Windows** — Visual Studio 2022 "Desktop development with C++" workload + Windows 10/11 SDK. WebView2 ships with modern Windows; install the [Evergreen Bootstrapper](https://developer.microsoft.com/microsoft-edge/webview2/) if missing.
- **macOS** — `xcode-select --install`
- **Linux** — `sudo apt install libgtk-3-dev libwebkit2gtk-4.1-dev libsoup-3.0-dev libayatana-appindicator3-dev librsvg2-dev`

The `claude` and `gh copilot` CLIs are **not** required to build, lint, or test. PTY integration tests use the in-tree `arborist-test-child` binary instead.

## First-time setup

```sh
git clone https://github.com/mcaden/arborist.git
cd arborist
nvm use          # optional — picks up .nvmrc → Node 24
pnpm install     # installs JS deps and wires Husky git hooks
```

The first `cargo` invocation downloads the crate index and Tauri's native deps — expect 2–5 minutes on a cold machine.

Confirm Husky hooks are installed:

```sh
ls .husky/pre-commit .husky/pre-push
```

## Common commands

### Run the app

```sh
pnpm run tauri:dev      # Vite + Tauri with HMR (frontend) and hot-recompile (backend)
pnpm run tauri:build    # production bundle → src-tauri/target/release/bundle/
```

### Lint, format, type-check

```sh
pnpm run lint            # eslint + prettier --check
pnpm run lint:fix        # auto-apply fixes
pnpm run dev:typecheck   # tsc --noEmit --watch — run this continuously while coding
cargo fmt --all -- --check
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

### Test

```sh
pnpm test                      # vitest watch mode (inner loop)
pnpm test --run             # vitest once (CI / pre-push)
cargo test --workspace        # unit + integration tests; also builds arborist-test-child
```

Run a specific Rust test by name prefix:

```sh
cargo test --workspace <name>
```

### Acceptance gate

All of the following must be green before a change is mergeable:

```sh
pnpm run lint
pnpm test --run
pnpm run build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Inner-loop watcher setup

Run these continuously in parallel terminals while coding:

```sh
# Frontend
pnpm run dev:typecheck    # tsc --noEmit --watch
pnpm run test:watch       # vitest watch

# Rust (requires cargo-watch: cargo install cargo-watch)
cargo watch -x check -x clippy
cargo watch -x 'test --workspace'
```

Recommended VS Code extensions: `rust-analyzer`, `ESLint`, `Prettier - Code formatter`, `Tailwind CSS IntelliSense`. Set `editor.formatOnSave: true` and `rust-analyzer.check.command = "clippy"`.

## Git hooks

Husky v9 installs two hooks via `pnpm install`:

- **pre-commit** — `lint-staged` runs ESLint + Prettier on staged JS/TS/JSON/CSS/MD; also runs `cargo fmt --check` and `cargo clippy` when any `.rs` file is staged.
- **pre-push** — `pnpm test --run` (Vitest CI mode) + `cargo test --workspace`.

`--no-verify` is allowed on personal WIP branches; never use it on `main`.

## Debugging

### Frontend

- Open Chromium DevTools in the dev WebView: right-click → "Inspect" (or `Ctrl/Cmd + Shift + I`). Vite source maps resolve TS paths directly.
- Inspect Zustand state from the console: `useSessionStore.getState()`, `useConfigStore.getState()`.
- The xterm terminal registry is accessible via the named export `__getTerminalRegistryForTests()` from `src/hooks/use-terminal.ts`. It is not attached to `window`; use it directly in tests or import the module in a dev console snippet.

### Backend

```sh
RUST_LOG=arborist_lib=debug pnpm run tauri:dev   # verbose tracing to stderr
RUST_LOG=trace pnpm run tauri:dev                # everything
```

Run the config-store end-to-end harness without Tauri:

```sh
cargo run -p arborist --example config_smoke
```

Poke the PTY test child interactively (echoes stdin, exits on `quit`):

```sh
cargo run -p arborist --bin arborist-test-child
```

### Persistent state

Config lives in your OS app-data directory. The exact folder name is derived from the Tauri app identifier in `src-tauri/tauri.conf.json` (currently `dev.arborist.desktop`):

| OS      | Path                                                                               |
| ------- | ---------------------------------------------------------------------------------- |
| Windows | `%APPDATA%\dev.arborist.desktop\`                                                  |
| macOS   | `~/Library/Application Support/dev.arborist.desktop/`                              |
| Linux   | `$XDG_DATA_HOME/dev.arborist.desktop/` (or `~/.local/share/dev.arborist.desktop/`) |

To debug persistence issues: stop Arborist, inspect/edit `config.json` or `sessions.json`, restart. If the loader rejects a file it renames it to `*.bad-<unix-timestamp>` and logs `code = "ConfigQuarantined"`.

## Troubleshooting

| Symptom                                                         | Fix                                                                                                                                      |
| --------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `error: linking with cl.exe failed` (Windows)                   | Install the Visual Studio 2022 "Desktop development with C++" workload                                                                   |
| `failed to find tool. Is gtk+-3.0 installed?` (Linux)           | Install GTK / WebKit2GTK dev packages (see Prerequisites)                                                                                |
| `pnpm run tauri:dev` opens a blank window                       | Frontend crashed at boot — open DevTools and check the console                                                                           |
| `cargo test --workspace` fails with `claude: command not found` | A test is calling the real CLI; integration tests must use `arborist-test-child` — file a bug                                            |
| Pre-commit hook does nothing                                    | Re-run `pnpm install` — Husky hooks are set up by the `prepare` script                                                                   |
| `config.json.bad-<timestamp>` keeps appearing                   | The loader is rejecting the file; diff it against the minimum valid example in [dev/docs/CONFIGURATION.md](../dev/docs/CONFIGURATION.md) |
| Sessions don't restore on launch                                | Look for `code = "WorktreeMissing"` or `"InstructionFileMissing"` in `RUST_LOG=debug` output                                             |
| Garbled xterm output after high-throughput burst                | Expected — the PTY pool's drop-newest backpressure prepends `ESC c`; output continues correctly after the reset                          |
