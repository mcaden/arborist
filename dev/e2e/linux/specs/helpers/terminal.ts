// =============================================================================
// Terminal output helpers for e2e specs.
//
// xterm.js renders into <div class="xterm-rows">…</div>. These helpers poll
// the rendered text content for expected patterns. Never uses sleep().
// =============================================================================

import { XTERM_ROWS } from "./selectors.js";

/**
 * Wait until the visible terminal output contains text matching `pattern`.
 * Polls the xterm DOM — tolerant of ANSI escape sequences in the rendered text.
 */
export async function waitForTerminalOutput(
  pattern: RegExp | string,
  opts: { timeoutMs?: number } = {}
): Promise<string> {
  const { timeoutMs = 10000 } = opts;
  // Strip `g`/`y` flags — `RegExp.test` with sticky/global is stateful
  // (advances `lastIndex`), which would make repeated polling flaky.
  const regex =
    typeof pattern === "string"
      ? new RegExp(pattern)
      : new RegExp(pattern.source, pattern.flags.replace(/[gy]/g, ""));

  let lastText = "";
  await browser.waitUntil(
    async () => {
      try {
        const rows = await $(XTERM_ROWS);
        if (!(await rows.isExisting())) return false;
        lastText = await rows.getText();
        return regex.test(lastText);
      } catch {
        return false;
      }
    },
    {
      timeout: timeoutMs,
      timeoutMsg: `Terminal output did not match ${regex} within ${timeoutMs}ms. Last text: "${lastText.slice(0, 200)}"`,
    }
  );

  return lastText;
}
