# Roadmap

This document tracks unimplemented features and known issues. Items are grouped by theme and ordered roughly by priority within each group.

For the authoritative product spec see `dev/docs/SPEC.md`. For architecture context see `dev/docs/DESIGN.md`.

---

## Upcoming features

### P1 — High value, no hard blockers

#### Per-worktree pre-launch command overrides in settings
The settings dialog exposes global pre-launch commands but not per-worktree overrides (`worktreePrelaunchCommands`). Those remain config-file–only for now. A future settings section could present them as a keyed list (worktree path → commands).

### P2 — Polish and quality

#### "Starting" spinner on tabs
Tabs show a red dot when `status === 'error'` but nothing when `status === 'starting'`. Slow pre-launch commands leave no indication the session is initialising. A subtle spinner or pulsing dot on the tab during the `starting` state would close this gap.

#### "Exited" indicator on tabs
`TerminalView` shows an overlay for both `error` and `exited` statuses, but the sidebar only renders the red dot for `error`. A clean exit (zero exit code) has no tab indicator. A grey dot for `exited` would make the session state unambiguous.

#### E2E test harness
The current test suite (Rust unit/integration + Vitest + RTL with the tauri-bridge mocked) has no layer that exercises the real Tauri shell + WebView together. Regressions in the frontend ↔ backend bridge — a renamed command, a missing capability entry, a broken event payload — are only caught by manual testing. The leading options are WebDriver via `tauri-driver` or Playwright pointed at the dev server. A decision needs to be made, documented in DESIGN, and scaffolded with a basic `e2e/` directory and CI job.

#### Authoritative session-id mapping via CLI hooks
The token-usage metrics on sidebar tabs use a heuristic cwd+mtime match against Claude's transcript directory. Replace this with hook-driven authoritative mapping: Claude's `--settings` flag can inject a `Stop` hook that delivers the CLI's `session_id` + `transcript_path` on stdin. Copilot lacks a `--hooks-file` equivalent; revisit when that's available. See issue #4.

#### Accessibility audit
- `NewSessionDialog` uses native `<dialog>` with `showModal()` for focus trapping, but explicit focus management on step transitions (moving focus to the first interactive element of each step) needs cross-platform WebView verification.
- All decorative SVG icons need `aria-hidden="true"` confirmed so screen readers don't double-read the tab label alongside the icon's implicit accessible name.

### P3 — Future / v2

- **Instruction-set management UI** — users currently manage `.md` files on disk. An in-app editor is out of scope for v1 (SPEC §7) but is the natural v2 follow-up.
- **Release pipeline** — no automated workflow produces distributable bundles (`.msi`, `.dmg`, `.AppImage`). A GitHub Actions release workflow triggered on a version tag is the target.
- **Remote/SSH worktrees**, **plugin system**, **multi-window support**, **built-in chat UI** — all explicitly out of scope per SPEC §7.

---

## Known issues

### Smoke tests not yet run
Two manual smoke tests that require a real OS WebView and Task Manager-level memory accounting have not been executed. Until a maintainer runs them and fills in the results, the memory and backpressure characteristics of the app are unverified at a system level. Procedures are in `dev/ai/SMOKE_TEST_RESULTS.md`.

- **Tab-switch leak check** — create 10 sessions, cycle tabs rapidly for 30 s, confirm RSS plateaus and the xterm `Terminal` instance count stays constant.
- **Backpressure check** — pump high-throughput output into one session, confirm other sessions stay interactive and the PTY pool's drop-newest policy fires as expected.

### Token metrics can't distinguish two Claude sessions in the same worktree
The sidebar token-usage display (`78% · 12.3k tok`) is sourced by matching Claude's `~/.claude/projects/` transcript files by cwd and mtime. If two Claude sessions share the same worktree, the match is ambiguous and the wrong session may display the wrong metrics. Copilot sessions show no metrics (Copilot writes no token data to disk). Tracked in issue #4; the long-term fix is hook-driven session-id mapping.

### Per-worktree pre-launch overrides require manual config editing
`worktreePrelaunchCommands` (per-worktree overrides for pre-launch commands) is not exposed in the settings dialog. Editing them requires manually modifying `config.json` in the OS app-data directory with the app closed. See `dev/docs/CONFIGURATION.md`.

### Worktree deletion between sessions surfaces a terse error
If a worktree directory is deleted while sessions are persisted, `restore_all_sessions` marks the affected session as `error` with a `WorktreeMissing` code. The terminal overlay does not yet surface the deleted path in a human-readable message — users must check the `RUST_LOG=debug` trace output to see which path is missing.

### `aria-hidden` not confirmed on decorative SVG icons
`ToolIcon` renders SVGs for the Claude and Copilot logos. `SidebarTab` sets `aria-label` on the button, but `aria-hidden="true"` on the SVG itself has not been audited across all placements. Screen readers may read the icon's implicit accessible name alongside the button label.
