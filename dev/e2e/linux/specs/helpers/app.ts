// =============================================================================
// App lifecycle helpers for e2e specs.
// =============================================================================

/**
 * Wait until the app's main window is fully loaded.
 * Uses a generous timeout since the first WebDriver connection can be slow.
 */
export async function waitForAppReady(timeoutMs = 30000): Promise<void> {
  // Wait for the body element to exist — confirms the WebView rendered
  await browser.waitUntil(
    async () => {
      try {
        const body = await $("body");
        return await body.isExisting();
      } catch {
        return false;
      }
    },
    { timeout: timeoutMs, timeoutMsg: "App body did not appear within timeout" }
  );
}
