<img src="splash.png" alt="Arborist - Manage Your Git Worktrees" width="160" align="right" />

# Arborist

**Arborist is a cross-platform desktop app for managing AI coding-assistant sessions across Git worktrees.**

It gives each worktree its own persistent terminal-backed context for Claude CLI, GitHub Copilot CLI, and configured custom processes. Worktree tabs
live in a sidebar; the main area shows the active worktree dashboard or terminal while background PTYs keep running.

Website: [https://arborist.tools](https://arborist.tools)

## Status

Arborist is a public open-source desktop app. Expect active changes to docs, workflows, and APIs while the project stabilizes.

## What it does

- **Workspace-first:** one running app binds to one primary Git clone.
- **Worktree tabs:** Arborist-created worktrees live under `<repo>/.arborist/.worktrees/`.
- **Persistent PTYs:** background AI and terminal sessions keep running across tab switches.
- **CLI-native auth:** Claude and Copilot authentication stay with their CLIs; Arborist does not store credentials.
- **Custom processes:** launch shells, editors, file browsers, and other commands from a worktree context.
- **Restore on launch:** open sessions are restored from persisted records.
- **Safe command composition:** worktree paths are passed as process `cwd`, not interpolated into shell commands.

## Install for users

Download signed installers from the [Releases page](https://github.com/mcaden/arborist/releases). Contributors can also use the setup below to run
Arborist locally from source.

Release artifacts are OS-signed and notarized where each platform supports it. Published assets include GitHub build attestations:

```sh
gh attestation verify <downloaded-file> --repo mcaden/arborist
```

Runtime requirements:

- `git` on `PATH`.
- At least one authenticated AI CLI for AI sessions: Claude CLI or GitHub Copilot CLI. Arborist does not handle CLI sign-in.
- Windows WebView2 runtime, if not already present.

Optional runtime dependency:

- Linux X11 application-sub-tab focusing uses `wmctrl`. Wayland focus is unsupported by design; launched applications still run.

## Run from source for contributors

Prerequisites are covered in [docs/development.md](docs/development.md).

```sh
git clone https://github.com/mcaden/arborist.git
cd arborist
nvm use
pnpm install
pnpm dev
```

`pnpm dev` starts a local development build. To create a local desktop bundle instead, run `pnpm tauri:build`.

Common verification commands:

```sh
pnpm run lint
pnpm test --run
pnpm run build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --features test-helpers -- -D warnings
cargo test --workspace --features test-helpers
```

## Documentation

Long-form project docs live under [docs](docs/index.md). GitHub community-health docs use the conventional root filenames so GitHub can discover them.

| Topic                | Link                                           |
| -------------------- | ---------------------------------------------- |
| Mental model         | [docs/overview.md](docs/overview.md)           |
| Product contract     | [docs/product.md](docs/product.md)             |
| Architecture and API | [docs/architecture.md](docs/architecture.md)   |
| Runtime flows        | [docs/runtime-flows.md](docs/runtime-flows.md) |
| Configuration        | [docs/configuration.md](docs/configuration.md) |
| Worktrees            | [docs/worktrees.md](docs/worktrees.md)         |
| Development          | [docs/development.md](docs/development.md)     |
| Testing              | [docs/testing.md](docs/testing.md)             |
| Contributing         | [CONTRIBUTING.md](CONTRIBUTING.md)             |
| Security             | [SECURITY.md](SECURITY.md)                     |
| Support              | [SUPPORT.md](SUPPORT.md)                       |
| Code of conduct      | [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)       |
| Releasing            | [docs/releasing.md](docs/releasing.md)         |
| Roadmap              | [docs/roadmap.md](docs/roadmap.md)             |

## Contributing

Public contribution guidance is in [CONTRIBUTING.md](CONTRIBUTING.md). Use the GitHub issue templates for bugs and feature requests, and read
[SECURITY.md](SECURITY.md) before reporting a vulnerability.

## License

[MIT](LICENSE)
