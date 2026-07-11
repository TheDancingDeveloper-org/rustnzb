import { expect, test } from '@playwright/test';

test.describe('Global statistics', () => {
  test('shows persistent download, speed, server hit and article totals', async ({ page }) => {
    await page.goto('/statistics');
    await page.getByRole('button', { name: 'Lifetime' }).click();

    await expect(page.getByRole('heading', { name: 'Statistics' })).toBeVisible();
    await expect(page.locator('.card', { hasText: 'Downloads' })).toContainText('3');
    await expect(page.locator('.card', { hasText: 'Average speed' })).toContainText('/s');
    await expect(page.locator('.card', { hasText: 'News server hits' })).toContainText('2,600');
    await expect(page.locator('.card', { hasText: 'Articles served' })).toContainText('2,548');
    await expect(page.locator('.card', { hasText: 'Articles missing' })).toContainText('52');
    await expect(page.locator('table.data', { hasText: 'Seed News' })).toBeVisible();
  });
});
