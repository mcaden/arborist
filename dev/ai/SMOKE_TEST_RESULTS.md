# Arborist — Phase 12 Smoke Test Results

**Status: RESULTS PENDING — requires interactive GUI run by maintainer.**

The Phase 12 acceptance gates require two end-of-feature smoke tests from
`.github/skills/quality-workflow/SKILL.md` §5. These cannot be executed by
the implementing agent because the Tauri WebView and OS-level Task Manager
metrics aren't available in the headless agent environment. The procedures
below are reproducible step-by-step; the maintainer should run them once
locally on Windows (the primary dev platform) and fill in the **RESULTS**
sections.

---

## 1. Tab-switch leak check (SPEC T-03 / NF-04)

**Goal**: Confirm that creating many sessions and rapidly switching between
their tabs does not leak xterm `Terminal` instances, event listeners, or
PTY-output buffers in the WebView. RSS measured at the OS level should
plateau, not grow unboundedly.

### Procedure (Windows)

1. Build a release build (`npm run tauri:build`) and launch the produced
   binary, **or** `npm run tauri:dev` and accept that the dev-server overhead
   inflates the baseline.
2. Open Task Manager → Details tab → enable the **Working set (memory)**
   column. Locate `arborist.exe` (or `arborist-dev.exe` for the dev build). Record
   the steady-state RSS after the window has been idle for ~10 s.
   - **Baseline RSS (idle, 0 sessions): _____ MB**
3. Create **10 sessions** against any 10 worktrees. (For repeatability, point
   `worktreeRoots` at a directory containing 10 throwaway worktrees of the
   same repo. Use the New Session dialog; pick the same instruction set each
   time.) Wait until every session reports `Running` in the sidebar.
   - **RSS after 10 sessions started: _____ MB**
   - **xterm Terminal count (DevTools console: `__getTerminalRegistryForTests().size`): _____**
4. Rapidly cycle the active tab for **30 s** by clicking through the sidebar
   tabs (or via keyboard, if shortcuts are wired up). Aim for ~3-5 switches
   per second; an automated `setInterval` in DevTools is acceptable:

   ```js
   const ids = [...document.querySelectorAll('[role="tab"]')].map(
     (el) => el.getAttribute('data-session-id') ?? el.id,
   );
   let i = 0;
   const t = setInterval(() => {
     document.querySelectorAll('[role="tab"]')[i % ids.length].click();
     i++;
   }, 200);
   setTimeout(() => clearInterval(t), 30_000);
   ```

5. After the 30 s cycle, wait 5 s for any debounced work to settle.
   - **RSS after switching: _____ MB**
   - **Δ vs. step 3 RSS: _____ MB** (expected: small, single-digit MB; any
     monotonic per-switch growth is a regression)
   - **xterm Terminal count: _____** (must equal step 3 — switching must not
     create new terminals)
6. Listener leak check via DevTools:
   - In the Memory panel, take a heap snapshot. Filter for `Terminal` —
     count must equal the number of sessions, not a multiple of it.
   - Run a second snapshot and compare retained size for `Terminal`,
     `FitAddon`, `ResizeObserver`. None should grow.
   - **Leaked listeners: _____** (expected: 0)

### Acceptance

- RSS Δ across the 30 s cycle ≤ 20 MB.
- Terminal count constant across the cycle.
- No new `Terminal`/`FitAddon` instances in the second heap snapshot.

### RESULTS

> _Maintainer: fill in once executed._
>
> - Baseline RSS:
> - RSS after 10 sessions:
> - RSS after 30 s cycle:
> - Terminal count before/after:
> - Verdict (PASS / FAIL):

---

## 2. Backpressure check (Phase 6 backpressure policy)

**Goal**: Confirm that a session firing high-throughput output does not
freeze the WebView or starve other sessions. The PTY pool's bounded channel
should drop the *newest* chunks under sustained pressure (DESIGN / Phase 6),
emit a single `ESC c` reset before the next non-dropped chunk, and warn
once per 256 dropped chunks via `tracing`.

### Procedure (Windows)

1. With the app running and at least 2 sessions open (call them **A** and
   **B**), focus **session A** and run a high-throughput command:

   ```cmd
   cmd /c "for /l %i in (1,1,1000000) do @echo %i"
   ```

   (On macOS / Linux: `seq 1 1000000` or `yes | head -n 1000000`.)
2. While the loop is running, immediately switch to **session B** and:
   - Type into the terminal — input must echo without perceptible delay.
   - Issue a small command (`dir` / `ls`) — output must appear promptly.
3. Switch back to **session A** — the terminal should still be writing
   output (possibly with visible reset markers from `ESC c`); the UI must
   remain responsive (you can still scroll, click sidebar tabs, etc.).
4. In the launching shell / log capture, look for the
   `dropped 256 output chunks` warning from `pty_pool` — its presence
   confirms the backpressure policy fired (absence is also acceptable if
   the loop completed before the channel filled).

### Acceptance

- Session B remains interactive throughout (input echo < 100 ms perceived).
- Sidebar tab switches continue to work during the burst.
- App does not crash or OOM; RSS plateaus rather than growing linearly with
  the per-second output volume.

### RESULTS

> _Maintainer: fill in once executed._
>
> - Session B input latency under load:
> - Sidebar responsive during burst (Y/N):
> - `dropped chunks` warning observed (Y/N):
> - Verdict (PASS / FAIL):

---

## Why these aren't auto-runnable

Both checks need a real OS WebView (xterm renders to a canvas + DOM that
jsdom does not provide) and OS-level memory accounting (`Working set` in
Task Manager / `RSS` in `ps`). The Vitest suite covers the *units* — listener
attachment idempotency, terminal-per-session uniqueness, ResizeObserver
debounce, drop-on-unknown-id behaviour — but cannot validate the steady-state
characteristics those units compose into. These two procedures close that
gap once a maintainer runs them.
