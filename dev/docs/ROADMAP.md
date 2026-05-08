# Arborist — Gap Roadmap
_Last updated: 2026-04-28_

This document catalogues known gaps between the current implementation and the
intended product. Items are grouped by theme and ordered roughly by priority /
dependency order within each group. "Implemented" means the feature exists in
the current codebase at the level described.

---

## 1. Workspace Model _(new — not yet in SPEC/DESIGN)_

The app currently treats `worktreeRoots` as an open-ended list of repo roots
scanned during session creation. The intended UX is simpler: one active
**workspace** (a single root git repository) that the entire app operates
within.

### 1.1 First-boot workspace selection
- **Gap**: No onboarding screen is shown when the app starts without a
  configured workspace.
- **Needed**: On first launch (no `config.json`, or `workspaceRoot` is absent /
  empty), intercept the boot sequence and display a workspace picker screen
  before the main UI is revealed. The user selects (or types) the path to a
  local git repository root.
- **Data model change**: Add `workspaceRoot: string` (single path) to
  `AppConfig`. The existing `worktreeRoots` array can remain for
  backwards-compatibility but `workspaceRoot` takes precedence when set.
- **Backend needed**: New Tauri command `workspace_validate { path }` — verifies
  the directory exists and is a git repository (`git rev-parse --is-inside-work-tree`
  or equivalent). Returns `{ valid: bool, error?: string }` so the picker can
  show inline validation feedback.
- **Persistence**: `config_set { workspaceRoot: <path> }` saves the selection;
  `config_get` returns it so the frontend can detect the uninitialized state.

### 1.2 Workspace display in the UI
- **Gap**: No header or titlebar area shows which workspace is active.
- **Needed**: A small workspace indicator at the top of the sidebar (repo name +
  branch or just the directory basename) so the user always knows which repo
  they are operating in.

### 1.3 Workspace switching
- **Gap**: No mechanism to switch to a different workspace after initial setup.
- **Needed**: A button near the workspace indicator (e.g. "Change workspace…")
  that re-opens the workspace picker. Switching workspace **parks** all open
  sessions of the old workspace (kills the PTYs but preserves every persisted
  record — `sessions.json`, `last_open_sessions`, `tab_order`,
  `active_session_id`); a later switch back to that workspace re-spawns them
  via the standard `restore_all_sessions` path, with Claude/Copilot
  `--resume` keeping AI conversation context intact (see DESIGN §5.5c
  step 7).

---

## 2. Session Creation — Worktree Step Redesign _(new — not yet in SPEC/DESIGN)_

The current Step 2 of `NewSessionDialog` lists all worktrees discovered from
`worktreeRoots` via `worktrees_list`, plus a "Browse…" OS file picker. The
intended flow is:

> _"Choose an existing worktree **or** create a new one."_

### 2.1 Existing-worktree list sourced from `.worktrees/`
- **Gap**: `worktrees_list` runs `git worktree list --porcelain` and returns all
  worktrees for the given root. There is no specific support for the `.worktrees/`
  subdirectory convention (where all linked worktrees are kept alongside the
  main clone: `<repo>/.worktrees/<branch-name>/`).
- **Needed**: When a workspace is selected, Step 2 should offer a dedicated
  "Existing worktrees" list that reads directly from `<workspaceRoot>/.worktrees/`
  — either by scanning that subdirectory or by filtering the
  `worktrees_list` results to those paths. The existing "Browse…" fallback
  remains for worktrees stored outside `.worktrees/`.

### 2.2 Create-new-worktree option
- **Gap**: There is no way to create a worktree from within the app. The user
  must do it in a terminal first, then come back to Arborist.
- **Needed**:
  - Step 2 gains a toggle/tab: **"Existing"** | **"New"**.
  - In **New** mode: a text input for the worktree name (which doubles as the
    branch name, e.g. `my-feature` → `git worktree add .worktrees/my-feature -b my-feature`).
  - Inline validation that mimics git's branch-naming rules:
    - No spaces.
    - No `..`, `~`, `^`, `:`, `?`, `*`, `[`, `\`.
    - Cannot start or end with `.` or `/`.
    - Cannot end with `.lock`.
    - Cannot be `@` alone.
    - 1–255 characters.
  - The validation runs client-side on every keystroke; a final server-side
    check happens before the worktree is created.
  - New backend Tauri command: `worktree_create { name: string }` — runs
    `git worktree add <workspaceRoot>/.worktrees/<name> -b <name>` in
    `<workspaceRoot>`, returns the new worktree's absolute path on success.
  - DESIGN §6 command table must be updated to include `worktree_create`.
  - Capability entry required in `capabilities/main.json`.

### 2.3 Git branch-name validation utility
- **Gap**: No shared validation function exists on either the Rust or TS side.
- **Needed**: A pure function `validateWorktreeName(name: string): string | null`
  (returns `null` when valid, an error message string when invalid). Lives in
  `src/lib/worktree-validation.ts` and is covered by unit tests. Rust-side
  validation lives in a companion `compose::validate_worktree_name` function used
  before shelling out.

---

## 3. Settings / Configuration UI

Currently all configuration is done by hand-editing `config.json` with the app
closed (see `CONFIGURATION.md`). With the addition of workspace selection (#1),
at least a minimal in-app settings surface is needed.

### 3.1 Workspace settings
- **Gap**: No in-app way to change `workspaceRoot`, `instructionSetsDir`, or
  `worktreePrepCommands`.
- **Needed** (minimal v1 scope): A settings panel (accessible from the sidebar
  footer) that exposes:
  - Workspace root (path picker).
  - Instruction sets directory (path picker).
  - Worktree prep commands (editable ordered list — runs once when a new
    worktree is created; see issue #63).
- Per-worktree prep command overrides are intentionally out of scope for v1
  (the global list applies to every new worktree); a per-worktree config UI
  could reintroduce them later.

### 3.2 Instruction-set management
- **Gap**: No UI to add, rename, or delete instruction sets; users must manage
  `.md` files on disk.
- **Status**: Out of scope for v1 per SPEC §7 ("In-app editing of instruction
  set files"). Listed here as a v2 candidate.

---

## 4. Tab / Session UX Polish

### 4.1 "Starting" status indicator on tabs
- **Gap**: Tabs have an error dot (`session.status === 'error'`) but no visual
  feedback when a session is in the `'starting'` state. Slow pre-launch commands
  leave the user with no indication the session is initialising.
- **Needed**: A subtle spinner or pulsing dot on the tab icon while `status === 'starting'`.

### 4.2 "Exited" status indicator on tabs
- **Gap**: `TerminalView` shows an overlay for both `'error'` and `'exited'`
  statuses, but the sidebar tab only renders the red dot for `'error'`. A session
  that exits cleanly (zero exit code) has no tab indicator.
- **Needed**: Decide and implement a consistent visual convention — e.g. a grey
  dot for `'exited'` so users know the session has ended.

### 4.3 Worktree path validation on session restore
- **Gap**: DESIGN §8.2 documents that stale worktree paths should produce a clear
  error, but the UX path for "worktree was deleted between sessions" is not
  explicitly surfaced to the user at restore time.
- **Needed**: When `restore_all_sessions` fails for a session because the worktree
  directory no longer exists, emit `session://status { status: 'error' }` with an
  annotated message and show a human-readable note in the terminal overlay (e.g.
  "Worktree path no longer exists: /path/to/worktree").

### 4.4 Sidebar token-usage indicators — Claude v1 _(shipped — Issue #3)_
- **Shipped**: Each sidebar tab shows a compact second line `78% · 12.3k tok`
  with the running context-window utilization, sourced from polling Claude's
  `~/.claude/projects/<encoded-cwd>/<sid>.jsonl` transcript files. Heuristic
  cwd+mtime mapping; debounced; cleared on close/restart. Backend module:
  `src-tauri/src/session_metrics.rs`. Event: `session://metrics`.
- **Known limitation**: Cannot distinguish two same-tool Claude sessions sharing
  one worktree; Copilot tabs show no metrics (no token data on disk).

### 4.5 Authoritative session-id mapping via CLI hooks _(follow-up — Issue #4)_
- **Gap**: The v1 metrics watcher (4.4) uses a heuristic cwd+mtime match against
  Claude's transcript directory. Two same-tool sessions in one worktree are
  indistinguishable, and Copilot is unsupported because no token data is written
  to disk.
- **Needed**: Replace the heuristic with hook-driven authoritative mapping.
  Claude supports `--settings <json>` to inject a `Stop` hook that delivers the
  CLI's `session_id` + `transcript_path` on stdin. Copilot supports hooks only
  via `<cwd>/.github/hooks/hooks.json` (would pollute the user's repo) — revisit
  when the Copilot CLI gains a `--hooks-file` (or equivalent) flag.

---

## 5. Data Model Gaps

### 5.1 `AppConfig` missing `workspaceRoot`
- **Gap**: `AppConfig` (both Rust `types.rs` and TS mirror `arborist.ts`) has no
  `workspaceRoot` field.
- **Needed**: Add the field to both sides in the same commit; bump
  `CONFIG_VERSION_CURRENT` if adding as a required field (or make it
  `Option<String>` / `string | null` so existing configs don't break on load).

### 5.2 `worktree_create` not in DESIGN §6
- **Gap**: The command table in DESIGN §6 does not include `worktree_create`.
- **Needed**: Add the command definition (payload, return type, description)
  before implementation begins.

---

## 6. CI / Automation

### 6.1 Multi-platform CI pipeline
- **Gap**: No `.github/workflows/` directory exists. There is no automated build,
  lint, or test run on push or pull request.
- **Needed**: A GitHub Actions workflow that runs on push/PR targeting `main`,
  with jobs for:
  - **Lint & type-check** (`npm run lint`, `npm run build`, `cargo fmt --check`,
    `cargo clippy -D warnings`) on `ubuntu-latest`.
  - **Frontend tests** (`npm test -- --run`) on `ubuntu-latest`.
  - **Rust tests** (`cargo test --workspace`) on `ubuntu-latest`,
    `windows-latest`, and `macos-latest`.
  - (Optional) Tauri build smoke-test on all three platforms.

### 6.2 Release / bundle pipeline
- **Gap**: No automated release workflow to produce distributable bundles
  (`.msi`, `.dmg`, `.AppImage`).
- **Needed** (v2 candidate): A GitHub Actions release workflow triggered on a
  version tag that runs `npm run tauri:build` on all three platform runners and
  uploads bundle artefacts as release assets.

---

## 7. End-to-End Testing

The current test suite covers Rust unit/integration tests (`cargo test`) and
frontend component tests (Vitest + RTL with `tauri-bridge` mocked). There is no
test layer that exercises the real Tauri shell + WebView together against actual
PTY processes.

### 7.1 E2E test harness
- **Gap**: No end-to-end test framework is wired up. Regressions in the
  frontend ↔ backend bridge (e.g. a renamed command, a missing capability entry,
  a broken event payload) are only caught manually via `npm run tauri dev`.
- **Needed**: Adopt a Tauri-compatible E2E framework — the leading options are
  WebDriver via [`tauri-driver`](https://v2.tauri.app/develop/tests/webdriver/)
  driving the built app binary, or Playwright pointed at the dev server with the
  Rust backend running. Decide one, document the choice in DESIGN, and add the
  scaffolding (`e2e/` directory, runner config, CI job).

### 7.2 Critical-path E2E scenarios
- **Gap**: No automated coverage for the user journeys that span both processes.
- **Needed** (initial scenario set, expand over time):
  - First-boot workspace picker → select repo → main UI renders.
  - Create new session (existing worktree) → terminal attaches → input echoes
    back from a stub shell command.
  - Create new session (new worktree via `worktree_create`) → directory exists
    on disk → session launches in it.
  - Close session → PTY process exits → tab disappears → `lastOpenSessions`
    updated.
  - Restart app → `restore_all_sessions` re-creates tabs in the previous order
    and focuses the previously active session.
  - Stale worktree path on restore surfaces the error overlay (covers gap 4.3).

### 7.3 E2E in CI
- **Gap**: Even once a harness exists, CI (#6.1) doesn't run it.
- **Needed**: Add an `e2e` job to the GitHub Actions workflow that runs on
  `ubuntu-latest` at minimum (with `xvfb-run` for the WebView). Windows/macOS
  E2E runs are a v2 stretch goal — they're slow and flaky on hosted runners.
- **Determinism**: E2E tests must use a stub CLI binary (not real `claude` /
  `copilot`) and a tempdir workspace so they're hermetic and don't depend on
  the developer's machine state.

---

## 8. Accessibility

### 8.1 Tab keyboard navigation completeness
- **Status**: Roving `tabIndex` pattern is implemented in `Sidebar.tsx` /
  `SidebarTab.tsx`. Arrow-key navigation between tabs is covered.
- **Gap**: The `NewSessionDialog` uses native `<dialog>` with `showModal()`,
  which provides some focus trapping, but explicit focus management on step
  transitions (moving focus to the first interactive element of each step)
  needs verification across all three platforms' WebView implementations.

### 8.2 Screen-reader labels on tool icons
- **Status**: `ToolIcon` renders an SVG; `SidebarTab` sets `aria-label` on the
  button. Need to confirm the SVG itself is `aria-hidden` so the label is not
  read twice.
- **Gap**: Audit and add `aria-hidden="true"` to all decorative SVG icons.

---

## 9. Documentation Gaps

### 9.1 SPEC §5.5 / DESIGN §5.1 reference `worktreeRoots` (plural)
- Several spec and design sections describe worktree discovery in terms of a
  list of `worktreeRoots`. Once the single-workspace model (#1) is adopted,
  these sections need updating.

### 9.2 DESIGN §6 command table is the authoritative API surface
- Any new commands (`workspace_validate`, `worktree_create`) must be added to
  the table before their implementation PRs are merged.

### 9.3 `.worktrees/` convention not documented
- The `.worktrees/` subdirectory layout convention (all linked worktrees live
  under `<repo>/.worktrees/<name>/`) is not currently documented anywhere.
- **Needed**: A brief section in `CONFIGURATION.md` or a new `WORKTREES.md`
  explaining the convention and why it is the default.

---

## Summary table

| # | Area | Gap | Priority |
|---|------|-----|----------|
| 1.1 | Workspace | First-boot workspace selection | P0 |
| 2.1 | Session creation | Existing worktrees listed from `.worktrees/` | P0 |
| 2.2 | Session creation | Create-new-worktree option in dialog | P0 |
| 2.3 | Session creation | Git branch-name validation utility | P0 (blocks 2.2) |
| 1.2 | Workspace | Active workspace indicator in sidebar | P1 |
| 5.1 | Data model | `workspaceRoot` field in `AppConfig` | P1 (blocks 1.x) |
| 5.2 | Data model | `worktree_create` in DESIGN §6 | P1 (blocks 2.2) |
| 3.1 | Settings | Minimal settings panel (workspace, instructions dir, worktree prep) | P1 |
| 4.1 | Session UX | "Starting" spinner on tabs | P2 |
| 4.2 | Session UX | "Exited" indicator on tabs | P2 |
| 4.3 | Session UX | Stale worktree path error UX at restore | P2 |
| 1.3 | Workspace | Workspace switching | P2 |
| 6.1 | CI | Multi-platform CI pipeline | P2 |
| 7.1 | E2E | E2E test harness (tauri-driver or Playwright) | P2 |
| 7.2 | E2E | Critical-path E2E scenarios | P2 (blocked by 7.1) |
| 7.3 | E2E | Run E2E suite in CI | P2 (blocked by 7.1, 6.1) |
| 8.1 | a11y | Focus management in NewSessionDialog steps | P2 |
| 8.2 | a11y | `aria-hidden` on decorative SVGs | P2 |
| 9.1–9.3 | Docs | SPEC/DESIGN update for workspace model + `.worktrees/` convention | P2 |
| 3.2 | Settings | Instruction-set management UI | P3 (v2) |
| 6.2 | CI | Release / bundle pipeline | P3 (v2) |
