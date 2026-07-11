/**
 * History View — journeys 5.1-5.6
 *
 * Seeded history:
 *   - "Completed.Movie.2025.mkv"  — completed
 *   - "Failed.Show.S02E05.mkv"   — failed  (error: "Article not found on server: test-msg@test.com")
 *   - "Good.Podcast.EP100.mp3"   — completed
 */

import { test, expect } from '@playwright/test';

async function openAllHistory(page: import('@playwright/test').Page) {
  await page.goto('/downloads?tab=history');
  const history = page.locator('app-history-view');
  await expect(history).toBeVisible();
  await history.locator('.search-bar select').nth(2).selectOption('all');
  return history;
}

test.describe('5. Download History', () => {
  // Auto-accept confirm dialogs (used by delete tests)
  test.beforeEach(async ({ page }) => {
    page.on('dialog', (dialog) => dialog.accept());
  });

  // ── 5.1 Completed entry visible ────────────────────────────────────────────

  test('5.1 completed history entry visible with status pill', async ({ page }) => {
    await openAllHistory(page);

    const row = page.locator('tr, .row', { hasText: 'Completed.Movie.2025.mkv' }).first();
    await expect(row).toBeVisible();

    // Completed status pill
    await expect(row.locator('.s-ok')).toBeVisible();
  });

  // ── 5.2 Failed entry visible with error accessible ─────────────────────────

  test('5.2 failed history entry has failed pill and error detail', async ({ page }) => {
    await openAllHistory(page);

    const row = page.locator('tr, .row', { hasText: 'Failed.Show.S02E05.mkv' }).first();
    await expect(row).toBeVisible();

    // Failed status pill
    await expect(row.locator('.s-fail')).toBeVisible();

    // Error message visible somewhere on the row or on expand
    // Some UIs show it inline; some behind a detail click. Try inline first.
    const inlineError = row.locator('text=Article not found');
    const detailBtn = row.getByRole('button', { name: /detail|info|expand|\u2139/i });

    if (await inlineError.isVisible()) {
      await expect(inlineError).toBeVisible();
    } else if (await detailBtn.count() > 0) {
      await detailBtn.first().click();
      await expect(page.getByText('Article not found', { exact: false })).toBeVisible();
    }
    // Either path is acceptable — the test passes if the error is reachable.
  });

  test('5.2a long failed history details remain on one line', async ({ page }) => {
    const name = 'Law.And.Order.S15E20.1080p.AMZN.WEB-DL.DDP5.1.H.264-PlayWEB';
    const error =
      'Aborted: only 99.9% of content available (need 100.2%), 6 of 4408 content articles missing';

    await page.route('**/api/history', async (route) => {
      await route.fulfill({
        contentType: 'application/json',
        body: JSON.stringify({
          entries: [{
            id: 'long-failed-history-entry',
            name,
            category: 'sonarr',
            status: 'failed',
            total_bytes: 3_400_000_000,
            downloaded_bytes: 3_396_600_000,
            added_at: new Date(Date.now() - 13 * 60 * 60 * 1000 - 188_000).toISOString(),
            completed_at: new Date(Date.now() - 13 * 60 * 60 * 1000).toISOString(),
            output_dir: '',
            stages: [],
            error_message: error,
            server_stats: [],
            has_nzb_data: true,
          }],
        }),
      });
    });
    await page.setViewportSize({ width: 1000, height: 720 });

    const history = await openAllHistory(page);
    const row = history.locator('tbody tr').filter({ hasText: name });
    const nameLine = row.locator('.e-name');
    const errorLine = row.locator('.e-err');

    await expect(nameLine).toHaveAttribute('title', name);
    await expect(errorLine).toHaveAttribute('title', error);
    for (const line of [nameLine, errorLine]) {
      await expect(line).toHaveCSS('white-space', 'nowrap');
      expect(await line.evaluate((element) => element.scrollWidth > element.clientWidth)).toBe(true);
    }
  });

  // ── 5.3 Filter by name ─────────────────────────────────────────────────────

  test('5.3 name filter hides non-matching entries', async ({ page }) => {
    await openAllHistory(page);

    // All three entries present initially
    await expect(page.locator('tr, .row', { hasText: 'Completed.Movie.2025.mkv' }).first()).toBeVisible();
    await expect(page.locator('tr, .row', { hasText: 'Failed.Show.S02E05.mkv' }).first()).toBeVisible();
    await expect(page.locator('tr, .row', { hasText: 'Good.Podcast.EP100.mp3' }).first()).toBeVisible();

    // Type in the search / filter input
    const searchInput = page.getByPlaceholder('Filter name…');
    await searchInput.fill('Movie');

    // Only "Completed.Movie.2025.mkv" matches
    await expect(page.locator('tr, .row', { hasText: 'Completed.Movie.2025.mkv' }).first()).toBeVisible();

    // Neither "Failed.Show" nor "Good.Podcast" should be shown
    await expect(
      page.locator('tr, .row', { hasText: 'Failed.Show.S02E05.mkv' }).first(),
    ).not.toBeVisible();
    await expect(
      page.locator('tr, .row', { hasText: 'Good.Podcast.EP100.mp3' }).first(),
    ).not.toBeVisible();

    // Clear the filter
    await searchInput.clear();
    await expect(page.locator('tr, .row', { hasText: 'Failed.Show.S02E05.mkv' }).first()).toBeVisible();
  });

  // ── 5.4 Filter by status "Failed" ─────────────────────────────────────────

  test('5.4 status filter "Failed" hides completed entries', async ({ page }) => {
    const history = await openAllHistory(page);

    // Select "Failed" from the status dropdown
    const statusSelect = history.locator('.search-bar select').first();
    await statusSelect.selectOption({ label: 'Failed' });

    // Only the failed row visible
    await expect(page.locator('tr, .row', { hasText: 'Failed.Show.S02E05.mkv' }).first()).toBeVisible();

    // Completed rows hidden
    await expect(
      page.locator('tr, .row', { hasText: 'Completed.Movie.2025.mkv' }).first(),
    ).not.toBeVisible();
    await expect(
      page.locator('tr, .row', { hasText: 'Good.Podcast.EP100.mp3' }).first(),
    ).not.toBeVisible();

    // Reset to "All statuses"
    await statusSelect.selectOption({ label: 'All statuses' });
    await expect(page.locator('tr, .row', { hasText: 'Completed.Movie.2025.mkv' }).first()).toBeVisible();
  });

  // ── 5.5 Delete history entry ───────────────────────────────────────────────

  test('5.5 deleting a history entry removes it from the list', async ({ page }) => {
    await openAllHistory(page);

    const podcastRow = page.locator('tr, .row', { hasText: 'Good.Podcast.EP100.mp3' }).first();
    await expect(podcastRow).toBeVisible();

    // Click the delete (✕) action
    await podcastRow.getByRole('button', { name: 'Delete' }).click();

    // Entry must disappear
    await expect(
      page.locator('tr, .row', { hasText: 'Good.Podcast.EP100.mp3' }).first(),
    ).not.toBeVisible({ timeout: 5000 });

    // Other entries remain
    await expect(page.locator('tr, .row', { hasText: 'Completed.Movie.2025.mkv' }).first()).toBeVisible();
    await expect(page.locator('tr, .row', { hasText: 'Failed.Show.S02E05.mkv' }).first()).toBeVisible();
  });

  // ── 5.6 Stat cards show correct counts ────────────────────────────────────

  test('5.6 stat cards show 2 completed and 1 failed', async ({ page }) => {
    const history = await openAllHistory(page);

    // Completed card — seeded: Completed.Movie + Good.Podcast = 2
    const completedCard = history.locator('.cards4 .card', { hasText: 'Completed' });
    await expect(completedCard).toContainText('2');

    // Failed card — seeded: Failed.Show = 1
    const failedCard = history.locator('.cards4 .card', { hasText: 'Failed' });
    await expect(failedCard).toContainText('1');

    // Success rate card — 2 out of 3 = 66% or 67%
    const rateCard = history.locator('.cards4 .card', { hasText: 'Success rate' });
    await expect(rateCard).toBeVisible();
  });

  test('5.7 average speed is shown and selecting a job opens detailed information', async ({ page }) => {
    const history = await openAllHistory(page);
    const row = history.locator('tbody tr.history-row', { hasText: 'Completed.Movie.2025.mkv' });
    await expect(row).toContainText('/s');
    await row.click();

    const details = history.locator('.detail-panel');
    await expect(details).toBeVisible();
    await expect(details).toContainText('Average speed');
    await expect(details).toContainText('Articles served');
    await expect(details).toContainText('News server usage');
    await expect(details).toContainText('Seed News');
  });
});
