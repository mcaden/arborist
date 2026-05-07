# Arborist documentation

Authoritative documentation for the Arborist desktop app.

## Index

| Doc                                       | Audience          | Read it when…                                                               |
| ----------------------------------------- | ----------------- | --------------------------------------------------------------------------- |
| [`SPEC.md`](./SPEC.md)                    | Everyone          | You need the product contract — functional + non-functional requirements.    |
| [`DESIGN.md`](./DESIGN.md)                | Engineers         | You need the architecture, data model, and Tauri command/event API.          |
| [`ARCHITECTURE.md`](./ARCHITECTURE.md)    | Engineers         | You want a guided tour of the codebase that ties SPEC + DESIGN to real files. |
| [`DEVELOPMENT.md`](./DEVELOPMENT.md)      | Contributors      | You're setting up a dev environment or running build / lint / test.          |
| [`TESTING.md`](./TESTING.md)              | Contributors      | You're writing tests or trying to understand the test seams.                 |
| [`CONFIGURATION.md`](./CONFIGURATION.md)  | Operators / users | You're hand-editing `config.json` or recovering from a bad config.           |

## Reference outside this directory

- Top-level [`README.md`](../../README.md) — install + first-run quickstart.
- [`.github/copilot-instructions.md`](../../.github/copilot-instructions.md) —
  load-bearing engineering principles (test-first, determinism, "what done
  means", common pitfalls).
- [`.github/skills/quality-workflow-gate/SKILL.md`](../../.github/skills/quality-workflow-gate/SKILL.md)
  — exact build / lint / test commands and end-of-feature smoke procedures.
- [`dev/ai/`](../ai/) — agent-authored artefacts: implementation plan,
  smoke-test results, and review reports.
