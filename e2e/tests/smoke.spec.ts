import { test, expect } from '@playwright/test';
import * as path from 'path';
import * as fs from 'fs';

function getAuthToken(): string {
  const statePath = path.resolve(__dirname, '../auth-state.json');
  const state = JSON.parse(fs.readFileSync(statePath, 'utf8'));
  return state.origins?.[0]?.localStorage?.find((e: { name: string }) => e.name === 'access_token')?.value ?? '';
}

test.describe('Smoke Tests', () => {
  test('health endpoint', async ({ request }) => {
    const r = await request.get('/api/health');
    expect(r.status()).toBe(200);
  });

  test('app loads with tabs', async ({ page }) => {
    await page.goto('/');
    await expect(page).toHaveURL(/\/downloads/);
    await expect(page.locator('.brand')).toBeVisible();
    // All tabs visible
    await expect(page.getByRole('link', { name: 'Downloads' })).toBeVisible();
    await expect(page.getByRole('link', { name: 'Search' })).toBeVisible();
    await expect(page.getByRole('link', { name: 'Settings' })).toBeVisible();
    await expect(page.getByRole('link', { name: 'Logs' })).toBeVisible();
  });

  test('queue tab shows seeded jobs', async ({ page }) => {
    await page.goto('/downloads');
    await expect(page.getByText('Test.Movie.2025.mkv')).toBeVisible();
    await expect(page.getByText('Another.Show.S01E01')).toBeVisible();
  });

  test('groups tab shows subscribed groups', async ({ page }) => {
    await page.goto('/groups');
    await expect(page.locator('.g', { hasText: 'alt.test' })).toBeVisible();
    await expect(page.locator('.g', { hasText: 'alt.binaries.test' })).toBeVisible();
  });

  test('clicking a group loads headers', async ({ page }) => {
    await page.goto('/groups');
    await page.locator('.g', { hasText: 'alt.test' }).click();
    await expect(page.locator('h3').nth(1)).toContainText('alt.test');
    await expect(page.getByText('Test Post Alpha', { exact: true })).toBeVisible();
    await expect(page.getByText('Binary File [1/3]')).toBeVisible();
  });

  test('header search filters results', async ({ page }) => {
    await page.goto('/groups');
    await page.locator('.g', { hasText: 'alt.test' }).click();
    await expect(page.getByText('Test Post Alpha', { exact: true })).toBeVisible();

    await page.locator('.search-bar input').first().fill('Binary');
    await page.locator('.search-bar input').first().press('Enter');

    await expect(page.locator('table.data tbody tr')).toHaveCount(3);
    await expect(page.getByText('Test Post Alpha', { exact: true })).not.toBeVisible();
  });

  test('checkbox selection shows download bar', async ({ page }) => {
    await page.goto('/groups');
    await page.locator('.g', { hasText: 'alt.test' }).click();
    await expect(page.getByText('Binary File [1/3]')).toBeVisible();

    // Select first checkbox
    await page.locator('table.data tbody tr').nth(0).locator('input[type="checkbox"]').check();
    await expect(page.locator('.download-bar')).toBeVisible();
    await expect(page.getByText('1 selected')).toBeVisible();

    // Select all
    await page.locator('table.data thead input[type="checkbox"]').check();
    await expect(page.getByText('5 selected')).toBeVisible();

    // Download Selected button visible
    await expect(page.getByText('↓ Download selected')).toBeVisible();
  });

  test('settings tab shows servers', async ({ page }) => {
    await page.goto('/settings');
    await expect(page.getByRole('heading', { name: 'News servers' })).toBeVisible();
  });

  test('history tab loads', async ({ page }) => {
    await page.goto('/downloads?tab=history');
    await expect(page.locator('h3', { hasText: 'History' }).first()).toBeVisible();
  });

  test('logs tab loads', async ({ page }) => {
    await page.goto('/logs');
    // Should have at least some startup logs
    await page.waitForTimeout(3000);
    // Just verify the page rendered
    await expect(page.locator('.logs')).toBeVisible();
  });

  test('tabs navigate correctly', async ({ page }) => {
    await page.goto('/downloads');
    await page.getByRole('link', { name: 'Search' }).click();
    await expect(page).toHaveURL(/\/groups/);
    await page.getByRole('link', { name: 'Settings' }).click();
    await expect(page).toHaveURL(/\/settings/);
    await page.getByRole('link', { name: 'Downloads' }).click();
    await expect(page).toHaveURL(/\/downloads/);
  });

  test('status bar shows info', async ({ page }) => {
    await page.goto('/');
    // Status pills: the daemon state ("Live" or "Paused"), speed, queue count
    // and disk space. The state pill is a placeholder until the first /status
    // poll lands, so retry rather than reading it once.
    await expect(page.locator('.pill').first()).toBeVisible();
    await expect(page.locator('.status .pill[role="status"]')).toHaveText(/Live|Paused/);
  });

  test('groups API returns seeded data', async ({ request }) => {
    const token = getAuthToken();
    const r = await request.get('/api/groups?subscribed=true', {
      headers: { Authorization: `Bearer ${token}` },
    });
    const data = await r.json();
    expect(data.total).toBe(2);
    expect(data.groups[0].name).toBeTruthy();
  });

  test('headers API returns seeded data', async ({ request }) => {
    const token = getAuthToken();
    const r = await request.get('/api/groups/1/headers', {
      headers: { Authorization: `Bearer ${token}` },
    });
    const data = await r.json();
    expect(data.total).toBe(5);
    expect(data.headers.length).toBe(5);
  });

  test('group status API works', async ({ request }) => {
    const token = getAuthToken();
    const r = await request.get('/api/groups/1/status', {
      headers: { Authorization: `Bearer ${token}` },
    });
    const data = await r.json();
    expect(data.group_id).toBe(1);
    expect(data.new_available).toBe(50);
  });
});
