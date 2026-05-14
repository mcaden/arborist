# Support

Arborist is an open-source project maintained on a best-effort basis.

## Where to get help

| Need                    | Where to go                                                          |
| ----------------------- | -------------------------------------------------------------------- |
| Bug report              | Open a GitHub issue with reproduction steps and environment details. |
| Feature request         | Open a GitHub issue describing the workflow and expected behavior.   |
| Security vulnerability  | Follow [SECURITY](SECURITY.md); do not post details publicly.        |
| Contribution question   | Comment on the issue or PR you are working from.                     |
| Release/install problem | Open an issue with OS, artifact name, and first-run error text.      |

If GitHub Discussions are enabled in the future, general usage questions may move there. Until then, issues are the public support channel.

## What maintainers need

For bugs, include:

- Arborist version or commit.
- OS and architecture.
- Whether you are using a release build or dev build.
- Workspace shape: primary clone path pattern, managed worktree path, or manual worktree path.
- Tool involved: Claude, Copilot, terminal custom process, application custom process, or Git/worktree operation.
- Logs or screenshots with secrets removed.

For feature requests, include:

- The user workflow.
- Why current behavior is insufficient.
- Any safety, persistence, or cross-platform concerns.
- Whether the change affects the command/event API, config shape, or docs.

## Boundaries

Maintainers may close or redirect:

- Requests for help with third-party CLI authentication.
- Issues caused by unsupported external tools or broken local Git repositories.
- Reports without enough reproduction detail.
- Security reports posted publicly with exploit detail.
- Requests outside the project scope in [product](docs/product.md#out-of-scope-for-v1).

Support is best-effort. A maintainer response is not guaranteed within a specific time window.
