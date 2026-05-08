# Linux e2e Test Harness

Manual Docker-based harness for verifying the **release-built** Arborist AppImage on Linux.
Runs from a Windows host via Docker Desktop / WSL2.

## Prerequisites

- [Docker Desktop](https://www.docker.com/products/docker-desktop/) with WSL2 backend
- ~5 GB disk for the built image (first build takes 20–40 minutes)

## Quick Start

```bash
# 1. Build the image (compiles the release AppImage inside the container)
npm run e2e:linux:build

# 2. Run the WebdriverIO e2e specs against the AppImage
npm run e2e:linux

# 3. Run Rust tests in a clean Linux environment
npm run e2e:linux:rust

# 4. Run Vitest (frontend) tests in a clean Linux environment
npm run e2e:linux:vitest

# 5. Drop into a bash shell for ad-hoc debugging
npm run e2e:linux:shell
```

## Architecture

### Docker image (multi-stage)

| Stage | Purpose |
|---|---|
| `builder` | Full Rust + Node toolchain. Runs `npm run tauri:build` → AppImage. Also builds `arborist-test-child` and `tauri-driver`. |
| `runtime-tools` | Rust + Node toolchains for `cargo test` / `vitest` modes. Source is bind-mounted. |
| `runtime-e2e` | Minimal runtime: webkit2gtk-4.1, Xvfb, WebKitWebDriver, tauri-driver, Node + WebdriverIO, extracted AppImage at `/opt/arborist/`. |

### Compose services

| Service | Stage | What it does |
|---|---|---|
| `e2e` | runtime-e2e | Starts Xvfb + dbus, launches tauri-driver, runs WebdriverIO specs against the AppImage |
| `rust` | runtime-tools | `cargo test --workspace` with Linux build artifacts isolated in a named volume |
| `vitest` | runtime-tools | `npm ci && npm test -- --run` with node_modules isolated in a named volume |
| `shell` | runtime-e2e | Interactive bash with Xvfb running and the AppImage in place |

### Hermetic CLI testing

No real `claude` or `copilot` CLIs are installed. The env vars `ARBORIST_CLI_OVERRIDE_CLAUDE`
and `ARBORIST_CLI_OVERRIDE_COPILOT` point at `arborist-test-child`, which provides a
deterministic line-based protocol (`ARBORIST-TEST-CHILD READY`, `quit`, `exit N`, `flood K`,
`unicode`, and echo).

### Test specs (bind-mounted)

Specs live in `dev/e2e/linux/specs/` and are bind-mounted into the container at `/specs/`.
You can edit specs and re-run `npm run e2e:linux` without rebuilding the image.

To rebuild only when source code changes (frontend or Rust):
```bash
npm run e2e:linux:build
```

## Debugging a Failing Spec

### Interactive shell

```bash
npm run e2e:linux:shell
```

From the shell inside the container:

```bash
# The AppImage is extracted and ready to run
/opt/arborist/AppRun &

# Or start tauri-driver manually
tauri-driver --port 4444 &

# Then run individual specs
cd /e2e && npx wdio run /specs/wdio.conf.ts --spec /specs/specs/01-launch.spec.ts
```

### Inspecting the X display

From the shell, you can install VNC for visual debugging:

```bash
apt-get update && apt-get install -y x11vnc
x11vnc -display :99 -nopw -forever &
# Connect via VNC client to localhost:5900 (requires port forwarding in docker-compose)
```

## Spec Overview

| Spec | Description | SPEC IDs |
|---|---|---|
| `01-launch` | App boots, sidebar + main area render, no error overlay | T-01, S-01 |
| `02-workspace-bind` | Workspace picker: primary repo accepted, linked worktree rejected | W-01, W-02 |
| `03-session-create` | Create session, terminal shows test-child banner, unicode works | C-01..C-05, P-01 |
| `04-session-restart` | Restart reuses composedCommand; banner re-emits | L-03 |
| `05-session-close` | Close via test-child quit + close button; tab removed | L-04 |
| `06-multi-session-tab-switch` | Two sessions, flood + tab switch, scrollback survives | T-03 |
| `07-restore-on-launch` | Quit + relaunch, sessions restored with same labels | S-02, L-05 |
| `08-config-persistence` | Add instruction set, quit, relaunch, still present | — |

## Troubleshooting

### First build is very slow

The first `npm run e2e:linux:build` compiles the entire Rust crate from scratch inside the
container (~20–40 min depending on hardware). Subsequent builds leverage Docker layer caching
and are much faster if only source files changed.

### Bind-mount performance on Windows

Docker Desktop's bind mounts from Windows into Linux containers go through the 9P filesystem
and can be slow for large directories. This mainly affects `e2e:linux:rust` and `e2e:linux:vitest`.
The e2e service only bind-mounts the small `specs/` directory so performance is fine.

### Named volume management

Named volumes (`cargo-target-linux`, `cargo-cache`, `node-modules-linux`) persist Linux build
artifacts across runs. To clear them:

```bash
docker volume rm arborist_cargo-target-linux arborist_cargo-cache arborist_node-modules-linux
```

### Image size

The full image is ~3–5 GB. To reclaim space:

```bash
docker image prune
docker builder prune
```
