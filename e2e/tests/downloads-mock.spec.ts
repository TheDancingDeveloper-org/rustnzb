import { test, expect } from "@playwright/test";
import * as path from "path";

const FIXTURES = path.resolve(__dirname, "../fixtures");

test.describe("Mock-backed downloads", () => {
  test("uploading the sample NZB completes and lands in history", async ({
    page,
  }) => {
    await page.goto("/downloads");

    await page.getByRole("button", { name: /\+ upload nzb/i }).click();
    await page
      .locator('input[type="file"]')
      .setInputFiles(path.join(FIXTURES, "sample.nzb"));
    await page
      .locator(".add-panel")
      .getByRole("button", { name: "Upload", exact: true })
      .click();

    // Scoped to the snackbar's inner text label, inside the overlay. The
    // shell mirrors this sentence into its sr-only polite live region (an
    // unscoped text match hits both), and Material nests a second
    // .mat-mdc-snack-bar-label around the label, so the mdc class is the one
    // distinct node.
    await expect(
      page.locator(".cdk-overlay-container .mdc-snackbar__label", {
        hasText: /added to queue/i,
      }),
    ).toBeVisible({ timeout: 10000 });

    const history = page.locator(".history-toggle");
    await expect(history).toHaveAttribute("aria-expanded", "true");
    await expect(page).toHaveURL(/\/downloads$/);

    const completedRow = page
      .locator("app-history-view tr", { hasText: /sample/i })
      .first();
    await expect(completedRow).toBeVisible({ timeout: 20000 });
    await expect(completedRow.locator(".s-ok")).toBeVisible();
  });
});
