# Arborist

Arborist is a cross-platform desktop application that provides a unified workspace
for managing multiple AI coding-assistant sessions (Claude CLI and GitHub
Copilot CLI) across Git worktrees. Each session runs in its own PTY, bound to a
specific worktree, and is presented as a vertical tab in a sidebar with a
single integrated terminal area for the active session.

## Requirements

- **Node.js 20+** (this repo pins Node 24 via `.nvmrc` — `nvm use` will pick it
  up).
- **Rust toolchain** — install via [`rustup`](https://rustup.rs/). The Tauri
  backend builds with the stable channel.
- **Platform build tools** for Tauri v2 (see the
  [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for your
  OS): on Windows the Visual Studio C++ Build Tools, on macOS the Xcode
  command-line tools, on Linux the GTK / WebKit2GTK development packages.
- **`claude` and/or `copilot` CLIs** on `PATH` — Arborist launches whichever you
  pick when creating a session. They are not required to build or test the app
  itself.

## Optional system dependencies

Arborist's "application" custom-process sub-tabs (right-click a session tab →
Launch… → e.g. _Open Folder_, _VS Code_) attempt to focus the spawned
program's OS window when you click the sub-tab.

- **Linux (X11)**: requires [`wmctrl`](https://sites.google.com/site/tstyblo/wmctrl/)
  on `PATH`. Without it, focusing an application sub-tab is a no-op
  (logged warning, no error). Install via your distro's package manager
  (`apt install wmctrl`, `dnf install wmctrl`, etc.).
- **Linux (Wayland)**: window focus from another process is blocked by the
  protocol; Arborist reports `Unsupported` rather than attempting an
  X11-only call. Sub-tabs still spawn and track the application; only the
  click-to-focus action is unavailable.
- **macOS / Windows**: no extra dependencies. macOS uses `osascript` (always
  present); Windows uses native `user32` FFI.

## Quickstart

```sh
npm install
npm run tauri:dev
```

This boots the Vite dev server and launches the Tauri shell in a desktop
window with hot-reload for the frontend.

## Build (production)

```sh
npm run tauri:build
```

Produces a platform-specific bundle under `src-tauri/target/release/bundle/`.

## Test

The acceptance gate for any change is these six commands, all green:

```sh
npm run lint
npm test -- --run
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
npm run build
```

`npm test -- --run` runs Vitest once (no watch); `npm run test:watch` is the
inner-loop equivalent. `cargo test --workspace` includes the integration tests
under `src-tauri/tests/`, which build the in-tree `arborist-test-child` binary so
PTY tests don't depend on `claude` or `copilot` being installed.

## Architecture

See [`dev/docs/SPEC.md`](dev/docs/SPEC.md) and
[`dev/docs/DESIGN.md`](dev/docs/DESIGN.md) for the full product requirements
and architecture, including the command/event API, the boot/restore sequence,
the PTY pool design, and the on-disk layout of the persisted store.
The skill at `.github/skills/quality-workflow/SKILL.md` documents the exact
build/lint/test workflow and the end-of-feature smoke-test procedure.

## Status

Arborist is at the end of its v1 implementation. Out-of-scope for v1 (per
SPEC §7): a built-in chat UI, remote/SSH worktrees, a plugin system,
multi-window support, and an in-app instruction-set editor.
