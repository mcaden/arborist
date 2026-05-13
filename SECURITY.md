# Security

Arborist is a local desktop application that launches user-selected CLIs and shell commands in local Git worktrees. It is powerful by design and is
not a sandbox. This policy explains how to report vulnerabilities and what security boundaries the project maintains.

## Supported versions

Until Arborist reaches a stable 1.0 release, security fixes are made on `main` and included in the next release. Public releases are the supported
distribution channel. Old prerelease builds may not receive backports.

## Reporting a vulnerability

Do not open a public issue with exploit details, secrets, tokens, private logs, or a working proof of concept.

Preferred reporting path:

1. Use GitHub private vulnerability reporting for this repository when it is enabled.
2. If private reporting is not available, contact the repository maintainers through a non-public channel listed on the repository owner profile and
   ask where to send details.
3. Publicly file only a minimal placeholder if there is no private channel, and omit exploit details until a maintainer responds.

Please include:

- Affected Arborist version or commit.
- OS and installation method.
- Impact and affected component.
- Reproduction steps or proof of concept, kept private.
- Any known mitigations.

Maintainers should acknowledge reports, triage severity, prepare a fix privately when appropriate, and publish enough detail after release for users to
understand impact and remediation.

## Threat model

Arborist trusts the local user and the workspace they choose, but it still protects against accidental unsafe behavior at app boundaries.

| Area                       | Security expectation                                                                                                           |
| -------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| Credentials                | Arborist must not store CLI credentials, API keys, access tokens, or passwords. Claude/Copilot auth belongs to those tools.    |
| Shell command construction | User paths are canonicalized and worktree paths are passed as `cwd`, not interpolated into command strings.                    |
| Config                     | Config writes are atomic; corrupt config is quarantined rather than partially loaded.                                          |
| Workspace isolation        | Each `(branch, workspace)` store is protected by an advisory lock to avoid concurrent writes from multiple Arborist instances. |
| File opening               | Worktree-prep logs are opened only after containment checks under the app-data log directory.                                  |
| External processes         | Custom processes are user-configured commands. They run with the user's privileges and are not sandboxed.                      |
| Telemetry parsing          | Arborist reads local CLI transcript/telemetry files for metrics. It should not upload this data.                               |

## Out of scope security guarantees

Arborist does not:

- Sandbox Claude, Copilot, shells, or custom processes.
- Prevent a user-configured command from modifying files in its `cwd`.
- Audit or trust third-party CLIs.
- Encrypt local config or sessions.
- Hide information from someone who already controls the user's OS account.
- Provide enterprise policy enforcement.

## Sensitive data guidance

Public contributors should not include secrets or private logs in issues, PRs, tests, fixtures, screenshots, or docs. Redact:

- API keys and tokens.
- Private repository URLs.
- Absolute paths containing user or company names when not needed.
- CLI transcript content.
- Git remotes that reveal private infrastructure.

If sensitive data is accidentally committed, notify maintainers immediately. Do not only delete it in a follow-up commit; assume repository history and
forks may retain it.

## Release trust

Release artifacts are unsigned by OS code-signing systems unless release notes say otherwise. GitHub build attestations are published for release
assets and can be verified with:

```sh
gh attestation verify <downloaded-file> --repo mcaden/arborist
```

Unsigned binaries may trigger Windows SmartScreen or macOS Gatekeeper first-run warnings. See [releasing](docs/releasing.md) for release mechanics.
