# Arborist — developer setup

End-to-end instructions for getting a working dev environment, plus the
inner-loop and verification workflows you should expect to run dozens of
times a day.

> Companion docs: [`ARCHITECTURE.md`](./ARCHITECTURE.md) for the codebase
> tour, [`TESTING.md`](./TESTING.md) for the test architecture,
> [`CONFIGURATION.md`](./CONFIGURATION.md) for the on-disk config format.

## 1. Prerequisites

| Tool                 | Version           | Notes                                                                                |
| -------------------- | ----------------- | ------------------------------------------------------------------------------------ |
| **Node.js**          | 20 LTS or later   | The repo pins **Node 24** via `.nvmrc`; `nvm use` will pick it up.                   |
| **npm**              | bundled with Node | `npm` is used directly — no `pnpm` / `yarn` configuration.                           |
| **Rust**             | stable (rustup)   | Install from <https://rustup.rs/>. Toolchain is pinned by `rust-toolchain.toml`.     |
| **Git**              | 2.30+             | Required at runtime for `git worktree list` discovery (DESIGN §6).                   |
| **Tauri build deps** | platform-specific | See <https://v2.tauri.app/start/prerequisites/>.                                     |

Platform build deps in detail:

- **Windows** — Visual Studio 2022 "Desktop development with C++" workload (or
  the standalone Build Tools), plus the Windows 10/11 SDK. WebView2 ships with
  modern Windows; install the
  [Evergreen Bootstrapper](https://developer.microsoft.com/microsoft-edge/webview2/)
  if missing.
- **macOS** — `xcode-select --install` for the command-line tools.
- **Linux** — GTK 3, WebKit2GTK 4.1, libsoup, librsvg. On Debian/Ubuntu:
  `sudo apt install libgtk-3-dev libwebkit2gtk-4.1-dev libsoup-3.0-dev libayatana-appindicator3-dev librsvg2-dev`.

The `claude` and/or `copilot` CLIs are **not** required to build, lint, or
test Arborist. They're only needed at runtime for the sessions Arborist spawns —
the PTY integration tests use a purpose-built `arborist-test-child` binary
instead (see [`TESTING.md`](./TESTING.md)).

## 2. First-time install

```sh
git clone <repo>
cd arborist
nvm use            # picks up .nvmrc → Node 24
npm install        # installs JS deps and runs `husky` to wire git hooks
```

`npm install` triggers `husky` via the `prepare` script, which installs the
`.husky/pre-commit` and `.husky/pre-push` hooks. Confirm:

```sh
ls .husky/pre-commit .husky/pre-push
```

The first `cargo` invocation will download the Rust crate index and Tauri's
native deps; expect 2–5 minutes on a cold machine.

## 3. Repository layout

```
arborist/
├── Cargo.toml                # workspace root — members = ["src-tauri"]
├── package.json              # frontend + Tauri CLI scripts
├── index.html, vite.config.ts, tsconfig.json, tailwind.config.js, postcss.config.js
├── instructions/             # default instruction-set markdown files
├── src/                      # React + TypeScript frontend
│   ├── main.tsx, App.tsx
│   ├── components/           # Sidebar, NewSessionDialog, TerminalView, …
│   ├── store/                # Zustand stores (session-store, config-store, …)
│   ├── hooks/                # use-terminal (xterm.js lifecycle)
│   ├── lib/                  # tauri-bridge (typed invoke/listen wrappers), session-events
│   ├── types/                # TS mirrors of Rust types
│   └── test/                 # shared vitest helpers
├── src-tauri/                # Rust backend (Tauri v2)
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── build.rs
│   ├── capabilities/main.json   # capability gating for #[tauri::command] handlers
│   ├── permissions/             # plugin permission JSON
│   ├── icons/, gen/             # bundle assets and generated schemas
│   ├── src/
│   │   ├── main.rs, lib.rs
│   │   ├── commands/            # #[tauri::command] handlers (mod.rs + session.rs)
│   │   ├── compose.rs           # pure command composition + label dedup
│   │   ├── config_store.rs      # tauri-plugin-store wrapper, atomic writes, quarantine
│   │   ├── git.rs               # GitRunner trait + git worktree list parser
│   │   ├── pty_pool.rs          # PTY pool, PtySpawner trait, backpressure
│   │   ├── types.rs             # serde types (Session, AppConfig, errors, events)
│   │   └── bin/arborist_test_child.rs   # deterministic test child binary
│   ├── examples/                # `config_smoke` — end-to-end config-store harness
│   └── tests/                   # cargo integration tests (capability_gating, pty_pool, …)
├── dev/
│   ├── docs/                    # this directory
│   └── ai/                      # agent-authored artefacts (plan, review, smoke results)
└── .github/
    ├── copilot-instructions.md  # engineering principles loaded into every agent run
    └── skills/quality-workflow-gate/SKILL.md   # procedural lookup
```

## 4. Day-to-day commands

The full command reference also lives in
[`.github/skills/quality-workflow-gate/SKILL.md`](../../.github/skills/quality-workflow-gate/SKILL.md);
keep both pages in sync.

### Run the app

```sh
npm run tauri:dev      # Vite + Tauri shell with HMR (frontend) and cargo recompile (backend)
npm run tauri:build    # production bundle in src-tauri/target/release/bundle/
```

### Lint, format, type-check

```sh
npm run lint           # eslint . && prettier --check .
npm run lint:fix       # auto-apply fixes
npm run dev:typecheck  # tsc --noEmit --watch (run continuously while coding)
cargo fmt --all -- --check
cargo fmt --all
cargo clippy --workspace --all-targets --features test-helpers -- -D warnings
```

### Test

```sh
npm test               # vitest in watch mode (default for inner loop)
npm test -- --run      # vitest once (CI mode); used by pre-push and CI
npm run build          # tsc --noEmit + vite build
cargo test --workspace --features test-helpers  # unit + integration tests including the PTY pool
```

`cargo test --workspace --features test-helpers` builds the in-tree
`arborist-test-child` binary (gated behind the `test-helpers` feature) and
exposes its path to integration tests via the `CARGO_BIN_EXE_arborist-test-child`
environment variable. Without `--features test-helpers`, the test-helper binaries
are excluded — this is how release builds avoid bundling them.

### Acceptance gate (before declaring "done")

The full set, from the repo root:

```sh
npm run lint
npm test -- --run
npm run build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --features test-helpers -- -D warnings
cargo test --workspace --features test-helpers
```

All six must be green. Anything red is a blocker per
`.github/copilot-instructions.md` § "What 'tested' means before merge".

## 5. Inner-loop watcher setup (one-time per contributor)

Run continuously while coding so type / lint / test feedback lands in
seconds:

```sh
# Two terminals for the frontend
npm run dev:typecheck    # tsc --noEmit --watch
npm run test:watch       # vitest in watch mode

# Two terminals for Rust (cargo-watch is optional but recommended)
cargo install cargo-watch
cargo watch -x 'clippy --all-targets --features test-helpers -- -D warnings'
cargo watch -x 'test --workspace --features test-helpers'
```

Editor recommendations:

- **VS Code** — install `rust-analyzer`, `ESLint`,
  `Prettier - Code formatter`, `Tailwind CSS IntelliSense`. Set
  `editor.formatOnSave: true` and configure
  `rust-analyzer.check.command = "clippy"`.
- **Other editors** — anything with LSP support works; the Rust LSP is
  `rust-analyzer`, the TS LSP is the bundled `typescript-language-server`.

## 6. Git hooks

Husky v9 is wired up by `npm install` (via the `prepare` script).

- **pre-commit** (`.husky/pre-commit`):
  - `lint-staged` runs `eslint --fix` + Prettier on staged JS / TS / JSON /
    CSS / MD.
  - When any `.rs` file is staged, `cargo fmt --all -- --check` and
    `cargo clippy --workspace --all-targets --features test-helpers -- -D warnings` also run.
- **pre-push** (`.husky/pre-push`):
  - `npm test -- --run` (Vitest CI mode).
  - `cargo test --workspace --features test-helpers`.

Bypassing with `--no-verify` is allowed for WIP branches that won't be
merged — never on `main` (per `.github/copilot-instructions.md`).

## 7. Debugging

### Frontend

- The dev WebView exposes Chromium DevTools (right-click → "Inspect" or
  `Ctrl/Cmd + Shift + I` if your platform allows it). Vite's source maps
  mean TS file paths resolve directly.
- Zustand stores can be inspected via `useSessionStore.getState()` from the
  DevTools console.
- The xterm registry is exposed in dev as `__getTerminalRegistryForTests()`
  on `window`; the Phase 12 smoke procedure (see
  [`../ai/SMOKE_TEST_RESULTS.md`](../ai/SMOKE_TEST_RESULTS.md)) uses it to
  prove zero-leak tab switching.

### Backend

- Tracing output goes to stderr. Set `RUST_LOG=arborist_lib=debug` (or
  `RUST_LOG=trace`) before launching `npm run tauri:dev` for verbose logs.
- The standalone `config_smoke` example exercises the full config-store
  lifecycle without spinning up Tauri:
  ```sh
  cargo run -p arborist --example config_smoke
  ```
- For ad-hoc PTY experiments, run the test child directly:
  ```sh
  cargo run -p arborist --features test-helpers --bin arborist-test-child
  ```

### Persistent state

Arborist writes two JSON files to the OS app-data directory (paths and
quarantine behaviour are in [`CONFIGURATION.md`](./CONFIGURATION.md)). When
debugging persistence issues:

1. Stop Arborist.
2. Inspect or hand-edit `config.json` / `sessions.json`.
3. Restart Arborist — the loader logs `code = "ConfigQuarantined"` if the file
   could not be parsed and replaces it with `*.bad-<unix-timestamp>`.

## 8. Packaging a release build

```sh
npm run tauri:build
```

Output lands in `src-tauri/target/release/bundle/` under platform-specific
subdirectories (`msi`, `nsis`, `dmg`, `appimage`, `deb`, …). The bundler
honours the metadata in `src-tauri/tauri.conf.json` (identifier
`com.arborist.app`, product name `Arborist`).

There is no automated release pipeline yet — bundles are produced manually.

## 9. Known v1 gaps

- The two performance / memory smoke tests (tab-switch leak check and
  backpressure check) require a real OS WebView and Task-Manager-level RSS
  metrics; they're documented step-by-step in
  [`../ai/SMOKE_TEST_RESULTS.md`](../ai/SMOKE_TEST_RESULTS.md) and must be
  run by hand on a maintainer machine. The RESULTS section is intentionally
  blank until that happens.
- There is no in-app settings UI in v1; all config is hand-edited per
  [`CONFIGURATION.md`](./CONFIGURATION.md). SPEC §7 lists this as
  out-of-scope.

## 10. Troubleshooting

| Symptom                                                          | Likely cause / fix                                                                                                  |
| ---------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| `error: linking with cl.exe failed` on Windows                   | Visual Studio C++ Build Tools not installed; install the workload above.                                            |
| `failed to find tool. Is gtk+-3.0 installed?` on Linux           | Missing GTK / WebKit2GTK dev packages — see prerequisites.                                                          |
| `npm run tauri:dev` opens a blank window                         | Frontend crashed during boot; open DevTools and check the console for an `ErrorOverlay` reason.                     |
| `cargo test --workspace --features test-helpers` fails with `claude: command not found` | A test path is calling the real CLI — file a bug, the integration tests must use `arborist-test-child`.             |
| Pre-commit hook does nothing                                     | `npm install` wasn't re-run after pulling — Husky hooks are installed by the `prepare` script.                      |
| `config.json.bad-<timestamp>` keeps appearing                    | The loader is rejecting the file. Diff it against the minimum valid example in [`CONFIGURATION.md`](./CONFIGURATION.md). |
| Sessions silently fail to restore on launch                      | Look for `code = "WorktreeMissing"` or `"InstructionFileMissing"` in the trace log; affected sessions stay in the sidebar with `status = error`. |
| xterm renders garbled output after a high-throughput burst       | Expected — the PTY pool's drop-newest backpressure prepends `ESC c` to the next chunk; output continues correctly after the reset. |
