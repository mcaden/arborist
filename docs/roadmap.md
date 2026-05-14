# Roadmap

This page tracks known gaps and follow-up work. It is not a promise of delivery order.

## Near-term quality

| Area          | Gap                                                                                                                                      |
| ------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| E2E coverage  | Expand the Linux E2E harness beyond launch smoke tests to cover workspace picker, worktree create, session launch, restart, and restore. |
| Accessibility | Audit focus management in dialogs and confirm decorative icons are consistently hidden from screen readers.                              |
| Manual smoke  | Keep release smoke checks documented in release PRs until more OS/WebView coverage is automated.                                         |
| Docs          | Keep command/event tables and config docs updated whenever the wire contract changes.                                                    |

## Product follow-ups

| Area                    | Gap                                                                                                                                       |
| ----------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| Child tab reorder       | Top-level worktree tab order persists; per-worktree child reorder is a future polish item.                                                |
| Stale worktree recovery | Deleted worktrees are detected, but guided recovery actions could be clearer.                                                             |
| Metrics identity        | Claude metrics can be ambiguous when multiple sessions share one worktree; hook-based authoritative mapping remains a future improvement. |
| Application focus       | Launcher wrappers and Wayland limitations make app focus best-effort. Better owner discovery can improve this per platform.               |
| Instruction management  | Users manage instruction files on disk. An in-app editor remains out of scope for v1 but is a natural later feature.                      |

## Public open-source readiness

| Area              | Gap                                                                                                     |
| ----------------- | ------------------------------------------------------------------------------------------------------- |
| Community files   | Root community-health files now follow GitHub naming conventions; keep them linked from the docs index. |
| Issue templates   | Public bug/feature/security templates would make triage easier.                                         |
| Governance        | As contributor volume grows, document maintainer roles and decision process.                            |
| Dependency review | Add automated dependency and supply-chain review appropriate for public PRs.                            |

## Longer-term ideas

- Remote or SSH worktrees.
- Multi-window support.
- Public plugin API.
- Built-in chat UI.
- Automatic updates.
- OS code signing and notarization.
- Package-manager distribution.
