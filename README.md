<img src="splash.png" alt="Arborist — Manage Your Git Worktrees" width="160" align="right" />

# Arborist

**Arborist is a cross-platform desktop app for managing multiple AI coding-assistant sessions across Git worktrees.**

It gives each worktree its own persistent terminal session — Claude CLI or GitHub Copilot CLI — all reachable from a single sidebar. Switch branches, switch contexts, no context lost.

---

## What it does

- **One sidebar, many sessions.** Each session is a vertical tab. Click a tab to bring that terminal to the front; background sessions keep running.
- **Worktree-native.** Sessions are tied to Git worktrees. Arborist creates and discovers worktrees under `<repo>/.worktrees/` and passes the path to the CLI as `cwd` — never baked into the command.
- **Instruction sets.** Drop Markdown files into a configured directory; Arborist injects them into Claude sessions at launch as a system prompt. Copilot sessions continue to use their own `.github/copilot-instructions.md` auto-discovery.
- **Restore on launch.** Every session you had open is re-spawned automatically when you restart the app, in the same tab order.
- **Error recovery.** If a session's process exits unexpectedly, the tab shows an error indicator and a **Restart** button that re-runs the original invocation verbatim.

## Install

Pre-built installers for Windows, macOS, and Linux are published on the
[Releases page](https://github.com/mcaden/arborist/releases). Download the
asset for your platform and follow the first-run notes below — the binaries
are **unsigned**, so each OS will show a one-time warning.

- **Windows** (`Arborist_<version>_x64-setup.exe` or `Arborist_<version>_x64_en-US.msi`)
  — double-click to install. Windows SmartScreen will say "Windows protected
  your PC"; click **More info → Run anyway**.
- **macOS** (`Arborist_<version>_universal.dmg`) — open the DMG and drag
  Arborist to Applications. The first launch must be **right-click → Open**
  (not double-click), then confirm in the Gatekeeper dialog. The build is a
  universal binary and runs natively on both Apple Silicon and Intel.
- **Linux — AppImage** (`arborist_<version>_amd64.AppImage`) — `chmod +x` the
  file and run it. Works on most modern x86_64 distros.
- **Linux — Debian/Ubuntu** (`arborist_<version>_amd64.deb`) —
  `sudo apt install ./arborist_<version>_amd64.deb`.

Arborist requires `git` on `PATH`, plus at least one of `claude` or `gh
copilot` for actual session work. On Windows, the installer uses Tauri's
default WebView2 bootstrapper, which downloads the WebView2 runtime at
install time if it isn't already present (preinstalled on Windows 10/11).

Updates are manual: re-download the latest release when a new version ships.

## Built with

| Layer         | Technology                                                           |
| ------------- | -------------------------------------------------------------------- |
| Desktop shell | [Tauri v2](https://v2.tauri.app/) (Rust + OS WebView — not Electron) |
| Frontend      | React 18 + TypeScript, Vite, Tailwind CSS, Zustand, xterm.js         |
| Backend       | Rust, `portable-pty` (ConPTY on Windows), `tauri-plugin-store`       |

## Getting started

### Prerequisites

| Tool                             | Notes                                                                           |
| -------------------------------- | ------------------------------------------------------------------------------- |
| **Node.js 20+**                  | Repo pins Node 24 via `.nvmrc` — `nvm use` picks it up                          |
| **Rust (stable)**                | Install via [rustup.rs](https://rustup.rs/)                                     |
| **Platform build tools**         | [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/) for your OS |
| **`claude` and/or `gh copilot`** | Only needed at runtime — not required to build or test                          |

Platform specifics:

- **Windows** — Visual Studio 2022 "Desktop development with C++" workload + Windows 10/11 SDK
- **macOS** — `xcode-select --install`
- **Linux** — `sudo apt install libgtk-3-dev libwebkit2gtk-4.1-dev libsoup-3.0-dev libayatana-appindicator3-dev librsvg2-dev`

### Install and run

```sh
git clone https://github.com/mcaden/arborist.git
cd arborist
nvm use          # optional — picks up .nvmrc
npm install
npm run tauri:dev
```

This starts the Vite dev server and opens Arborist in a desktop window with hot-reload.

### First-time configuration

Arborist stores its config in your OS app-data directory (path derived from the Tauri app identifier — currently `%APPDATA%\dev.arborist.desktop\` on Windows, `~/Library/Application Support/dev.arborist.desktop/` on macOS). On first launch it will prompt you to choose a workspace root — point it at your primary Git repository.

See [dev/docs/CONFIGURATION.md](dev/docs/CONFIGURATION.md) for the full config reference and [docs/LOCAL_DEVELOPMENT.md](docs/LOCAL_DEVELOPMENT.md) for setup and troubleshooting.

## How sessions work

```
New session dialog
  1. Pick tool:        Claude  /  Copilot
  2. Pick worktree:    quick-pick from <repo>/.worktrees/  or  Browse…
  3. Launch
```

Arborist composes one shell string — `[prelaunchCmds &&] <cli>` — stores it, and reuses it verbatim on every restart and restore. The worktree path is always passed as the process `cwd`, never embedded in the command.

## Contributing

See [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md) for the full contribution guide including the acceptance gate, code conventions, and the checklist for adding new Tauri commands.

## Documentation

| Doc                                                    | What's in it                                                                      |
| ------------------------------------------------------ | --------------------------------------------------------------------------------- |
| [docs/LOCAL_DEVELOPMENT.md](docs/LOCAL_DEVELOPMENT.md) | Full dev-environment setup, inner-loop watcher config, debugging, troubleshooting |
| [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md)           | Contribution rules, PR workflow, acceptance gate, architecture conventions        |
| [docs/ROADMAP.md](docs/ROADMAP.md)                     | Upcoming features by priority, known issues                                       |
| [dev/docs/ARCHITECTURE.md](dev/docs/ARCHITECTURE.md)   | Deep codebase tour — module map, boot sequence, capability gating                 |
| [dev/docs/SPEC.md](dev/docs/SPEC.md)                   | Product requirements (the spec is the source of truth)                            |
| [dev/docs/DESIGN.md](dev/docs/DESIGN.md)               | Architecture, data model, command/event API                                       |
| [dev/docs/CONFIGURATION.md](dev/docs/CONFIGURATION.md) | On-disk config format, fields, quarantine behaviour                               |
| [dev/docs/TESTING.md](dev/docs/TESTING.md)             | Test layout, seams, smoke-test procedure                                          |

## Status

Arborist is at the end of its v1 implementation. Out of scope for v1: built-in chat UI, remote/SSH worktrees, plugin system, multi-window support, in-app instruction-set editor.

## License

[MIT](LICENSE)
