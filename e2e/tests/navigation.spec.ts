import { test, expect } from "@playwright/test";

// ── Tests ─────────────────────────────────────────────────────────────────────

test.describe("11. Navigation & Shell", () => {
  // ── 11.1 All nav links present when authenticated ─────────────────────────

  test("11.1 all nav tabs present when authenticated", async ({ page }) => {
    await page.goto("/");

    // Should redirect to /downloads
    await expect(page).toHaveURL(/\/downloads/);

    // All primary nav links must be visible
    await expect(page.getByRole("link", { name: "Downloads" })).toBeVisible();
    await expect(
      page.getByRole("link", { name: /Groups|Search/i }),
    ).toBeVisible();
    await expect(page.getByRole("link", { name: "RSS" })).toBeVisible();
    await expect(page.getByRole("link", { name: "Logs" })).toBeVisible();
    await expect(page.getByRole("link", { name: "Settings" })).toBeVisible();
  });

  // ── 11.2 Active tab highlighted ───────────────────────────────────────────

  test("11.2 legacy history route opens history on downloads", async ({
    page,
  }) => {
    await page.goto("/history");

    await expect(page).toHaveURL(/\/downloads$/);

    const downloadsLink = page.getByRole("link", { name: "Downloads" });
    await expect(downloadsLink).toBeVisible();
    await expect(downloadsLink).toHaveClass(/active/);

    await expect(page.locator(".history-toggle")).toHaveAttribute(
      "aria-expanded",
      "true",
    );

    // Settings link should NOT be active
    const settingsLink = page.getByRole("link", { name: "Settings" });
    await expect(settingsLink).not.toHaveClass(/active/);
  });

  // ── 11.3 Status bar visible with connection/daemon info ───────────────────

  test("11.3 status bar shows daemon state pills", async ({ page }) => {
    await page.goto("/downloads");

    // The status bar contains .pill elements
    const pills = page.locator(".pill");
    await expect(pills.first()).toBeVisible();

    // The state pill shows a placeholder until the first /status poll lands,
    // so wait for it to settle on a daemon state rather than reading it once.
    const statePill = page.locator(".status .pill[role=status]");
    await expect(statePill).toHaveText(/Live|Paused/);
  });

  // ── 11.4 Tab links navigate to the correct routes ─────────────────────────

  test("11.4 clicking nav links changes route", async ({ page }) => {
    await page.goto("/downloads");

    // Downloads → Settings
    await page.getByRole("link", { name: "Settings" }).click();
    await expect(page).toHaveURL(/\/settings/);

    // Settings → Downloads
    await page.getByRole("link", { name: "Downloads" }).click();
    await expect(page).toHaveURL(/\/downloads/);
  });
});
