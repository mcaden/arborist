# Arborist documentation

This directory is the home for Arborist's long-form project docs. They use lowercase filenames and Mermaid diagrams; GitHub community-health docs live
at the repository root using GitHub's conventional filenames.

## Start here

| Document                            | Use it for                                                                                                |
| ----------------------------------- | --------------------------------------------------------------------------------------------------------- |
| [overview](./overview.md)           | The mental model: what Arborist is, how workspaces, worktree tabs, sessions, and terminals fit together.  |
| [product](./product.md)             | The product contract: requirements, supported scope, non-functional expectations, and out-of-scope items. |
| [architecture](./architecture.md)   | The codebase map, data model, command/event contract, plugin model, and invariants.                       |
| [runtime flows](./runtime-flows.md) | Boot, workspace switching, worktree creation, session launch, sub-sessions, metrics, and activity flows.  |

## Contributor and maintainer docs

| Document                        | Use it for                                                                          |
| ------------------------------- | ----------------------------------------------------------------------------------- |
| [development](./development.md) | Local setup, day-to-day commands, CI, debugging, and troubleshooting.               |
| [testing](./testing.md)         | Test layout, seams, deterministic test rules, and the Linux E2E harness.            |
| [releasing](./releasing.md)     | Manual release workflow, artifacts, unsigned-binary expectations, and attestations. |
| [roadmap](./roadmap.md)         | Known gaps, follow-up work, and planned improvements.                               |

## Public project docs

| Document                                           | Use it for                                                                         |
| -------------------------------------------------- | ---------------------------------------------------------------------------------- |
| [CONTRIBUTING](../CONTRIBUTING.md)                 | Branch, PR, review, coding, and acceptance-gate expectations.                      |
| [SECURITY](../SECURITY.md)                         | Responsible disclosure, threat model, security boundaries, and supported versions. |
| [SUPPORT](../SUPPORT.md)                           | Where public users should ask questions, file bugs, and request features.          |
| [CODE_OF_CONDUCT](../CODE_OF_CONDUCT.md)           | Conduct expectations and enforcement process for the public project.               |
| [Issue templates](../.github/ISSUE_TEMPLATE)       | Public bug and feature request forms for GitHub issues.                            |
| [PR template](../.github/pull_request_template.md) | Pull request summary, issue-link, check, and smoke-test checklist.                 |

## Operations and references

| Document                            | Use it for                                                                               |
| ----------------------------------- | ---------------------------------------------------------------------------------------- |
| [configuration](./configuration.md) | App-data layout, config fields, repo overlays, and quarantine recovery.                  |
| [worktrees](./worktrees.md)         | Workspace-root requirements, `.arborist/.worktrees/`, validation, and deletion behavior. |

## Documentation policy

- Keep long-form documentation under `docs/`.
- Use lowercase filenames for project docs.
- Use GitHub's conventional root filenames for community-health docs: `CONTRIBUTING.md`, `SECURITY.md`, `SUPPORT.md`, and `CODE_OF_CONDUCT.md`.
- Keep GitHub issue and pull request templates under `.github/` so the community profile can discover them.
- Use Mermaid for diagrams in active docs.
- Keep source comments brief; link to stable doc anchors for deeper design context.
