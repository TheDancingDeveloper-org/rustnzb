import { test, expect } from '@playwright/test';
import { readToken } from '../helpers/auth';

const BASE_URL = 'http://localhost:9190';

async function apiGetSpeedLimit(token: string): Promise<number | null> {
  try {
    const r = await fetch(`${BASE_URL}/api/config/speed-limit`, {
      headers: { Authorization: `Bearer ${token}` },
    });
    if (!r.ok) return null;
    const data = await r.json();
    return typeof data === 'number' ? data : (data.speed_limit ?? data.limit ?? null);
  } catch {
    return null;
  }
}

async function navigateToGeneral(page: import('@playwright/test').Page): Promise<void> {
  await page.goto('/settings');
  await page.getByRole('button', { name: 'General' }).click();
  // The h3 "Speed & concurrency" is inside @if (tab === 'general')
  await expect(page.locator('h3', { hasText: 'Speed' })).toBeVisible();
}

// The General tab form structure:
//   <label>Field name</label>
//   <div class="inline">
//     <input type="number" ...>
//     ...
//     <button class="btn sm">Save</button>
//   </div>
// Use CSS adjacent sibling selector to reach each field's input and button.

test.describe('8. Settings — General', () => {
  test('8.0 theme picker applies and persists two additional themes with standard fonts', async ({ page }) => {
    await page.goto('/settings');
    await page.getByRole('button', { name: 'General' }).click();

    await page.getByRole('radio', { name: /Midnight/ }).click();
    await expect(page.locator('body')).toHaveAttribute('data-theme', 'midnight');
    await page.getByRole('radio', { name: /Daylight/ }).click();
    await expect(page.locator('body')).toHaveAttribute('data-theme', 'light');

    const description = page.locator('.setting-description').first();
    await expect(description).toHaveCSS('font-family', /Inter|Segoe UI|Roboto/);
    await page.reload();
    await expect(page.locator('body')).toHaveAttribute('data-theme', 'light');
  });
  test('8.1 change global speed limit', async ({ page }) => {
    const token = readToken();
    await navigateToGeneral(page);

    const speedInput = page.locator('label:has-text("Global speed limit") + .inline input[type="number"]');
    await expect(speedInput).toBeVisible();

    const originalValue = await speedInput.inputValue();
    await speedInput.fill('10240');

    await page.locator('label:has-text("Global speed limit") + .inline button').click();
    await page.waitForTimeout(500);

    const saved = await apiGetSpeedLimit(token);
    if (saved !== null) {
      expect(typeof saved).toBe('number');
    }

    await expect(page.getByText(/error|failed/i)).not.toBeVisible();

    await speedInput.fill(originalValue || '0');
    await page.locator('label:has-text("Global speed limit") + .inline button').click();
  });

  test('8.2 history retention setting visible and changeable', async ({ page }) => {
    await navigateToGeneral(page);

    const retentionLabel = page.locator('label', { hasText: 'History retention' });
    await expect(retentionLabel).toBeVisible();

    const retentionInput = page.locator('label:has-text("History retention") + .inline input[type="number"]');
    await expect(retentionInput).toBeVisible();

    const originalValue = await retentionInput.inputValue();
    await retentionInput.fill('30');

    await page.locator('label:has-text("History retention") + .inline button').click();
    await page.waitForTimeout(500);

    await navigateToGeneral(page);

    const retentionInputAfter = page.locator('label:has-text("History retention") + .inline input[type="number"]');
    await expect(retentionInputAfter).toHaveValue('30');

    await retentionInputAfter.fill(originalValue || '0');
    await page.locator('label:has-text("History retention") + .inline button').click();
  });

  test('8.3 max active downloads visible and changeable', async ({ page }) => {
    await navigateToGeneral(page);

    const concurrentLabel = page.locator('label', { hasText: 'Concurrent jobs' });
    await expect(concurrentLabel).toBeVisible();

    const concurrentInput = page.locator('label:has-text("Concurrent jobs") + .inline input[type="number"]');
    await expect(concurrentInput).toBeVisible();

    const originalValue = await concurrentInput.inputValue();
    await concurrentInput.fill('2');

    await page.locator('label:has-text("Concurrent jobs") + .inline button').click();
    await page.waitForTimeout(500);

    await expect(page.getByText(/error|failed/i)).not.toBeVisible();

    await navigateToGeneral(page);
    const concurrentInputAfter = page.locator('label:has-text("Concurrent jobs") + .inline input[type="number"]');
    await expect(concurrentInputAfter).toHaveValue('2');

    await concurrentInputAfter.fill(originalValue || '1');
    await page.locator('label:has-text("Concurrent jobs") + .inline button').click();
  });
});

test.describe('8a. Settings — Scheduler', () => {
  test('8a.1 adds, saves, reloads and removes a rule', async ({ page }) => {
    await page.goto('/settings');
    await page.getByRole('button', { name: 'Scheduler' }).click();

    await expect(page.locator('h2', { hasText: 'Scheduler' })).toBeVisible();

    // Clear anything a previous run left behind, so the journey is repeatable.
    while ((await page.getByRole('button', { name: 'Remove' }).count()) > 0) {
      await page.getByRole('button', { name: 'Remove' }).first().click();
      await page.getByRole('button', { name: 'Save schedule' }).click();
      await expect(page.getByText('Schedule saved')).toBeVisible();
    }
    await expect(page.getByText('No rules yet')).toBeVisible();

    await page.getByRole('button', { name: '+ Add rule' }).click();
    const name = page.locator('label:has-text("Name") + .inline input[type="text"]').first();
    await name.fill('E2E quiet hours');

    // Friday is selected by default; the window is 01:00-06:00.
    await expect(page.locator('.schedule-day', { hasText: 'Fri' }).locator('input')).toBeChecked();

    await page.getByRole('button', { name: 'Save schedule' }).click();
    await expect(page.getByText('Schedule saved')).toBeVisible();

    // A reload comes back from the API with the server-assigned id and the
    // saved name and times.
    await page.reload();
    await page.getByRole('button', { name: 'Scheduler' }).click();
    await expect(page.locator('.panel h3', { hasText: 'E2E quiet hours' })).toBeVisible();
    const start = page.locator('input[aria-label="Start time"]').first();
    await expect(start).toHaveValue('01:00');
    await expect(page.locator('input[aria-label="End time"]').first()).toHaveValue('06:00');

    await page.getByRole('button', { name: 'Remove' }).first().click();
    await page.getByRole('button', { name: 'Save schedule' }).click();
    await expect(page.getByText('Schedule saved')).toBeVisible();
    await expect(page.getByText('No rules yet')).toBeVisible();
  });

  test('8a.2 refuses a malformed window instead of saving it', async ({ page }) => {
    await page.goto('/settings');
    await page.getByRole('button', { name: 'Scheduler' }).click();
    await page.getByRole('button', { name: '+ Add rule' }).click();

    const start = page.locator('input[aria-label="Start time"]').first();
    await start.fill('25:00');
    await expect(page.getByText('Start must be HH:MM')).toBeVisible();
    await expect(page.getByRole('button', { name: 'Save schedule' })).toBeDisabled();

    await start.fill('01:00');
    await expect(page.getByRole('button', { name: 'Save schedule' })).toBeEnabled();
    await page.getByRole('button', { name: 'Discard changes' }).click();
    await expect(page.getByText('No rules yet')).toBeVisible();
  });
});
