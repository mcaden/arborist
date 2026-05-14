// =============================================================================
// 01-launch.spec.ts — P0 spike / basic launch verification
//
// Verifies that the release-built Arborist AppImage launches under Xvfb,
// the WebView renders, and the core layout elements (sidebar, main area)
// are present. No error overlay should appear.
//
// Maps to product IDs T-01 (terminal viewport), S-01 (sidebar)
// =============================================================================

import { waitForAppReady } from "./helpers/app.js";
import { SIDEBAR, MAIN_AREA } from "./helpers/selectors.js";

describe("App Launch", () => {
  before(async () => {
    await waitForAppReady();
  });

  it("should render the main window body", async () => {
    const body = await $("body");
    expect(await body.isExisting()).toBe(true);
  });

  it("should display the sidebar", async () => {
    const sidebar = await $(SIDEBAR);
    await sidebar.waitForExist({ timeout: 10000 });
    expect(await sidebar.isDisplayed()).toBe(true);
  });

  it("should display the main area", async () => {
    const mainArea = await $(MAIN_AREA);
    await mainArea.waitForExist({ timeout: 10000 });
    expect(await mainArea.isDisplayed()).toBe(true);
  });

  it("should not show an error overlay", async () => {
    // If there's an unhandled error, React error boundaries or the app itself
    // would render an element with role="alert" or a known error testid.
    const errorElements = await $$('[role="alert"]');
    expect(errorElements.length).toBe(0);
  });

  it("should have the correct window title", async () => {
    const title = await browser.getTitle();
    // Window title should contain "Arborist" — exact format depends on whether
    // a workspace is bound and the build branch (see lib.rs window_title fn)
    expect(title).toContain("Arborist");
  });
});
