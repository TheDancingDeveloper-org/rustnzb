import { expect, test } from '@playwright/test';
import { readToken } from '../helpers/auth';

// The bindings are listed by the `?` sheet; these journeys press the real
// keys in a real browser so the window handler's guards (typing, modifiers,
// chords) are exercised, not just the pure resolver.
test.describe('Keyboard shortcuts', () => {
  test('? opens the help sheet, Escape closes it', async ({ page }) => {
    await page.goto('/downloads');

    await page.keyboard.press('?');

    const sheet = page.getByRole('dialog', { name: 'Keyboard shortcuts' });
    await expect(sheet).toBeVisible();
    await expect(sheet.getByText('Pause or resume all downloads')).toBeVisible();
    await expect(sheet.getByText('Go to Downloads')).toBeVisible();

    await page.keyboard.press('Escape');
    await expect(sheet).toBeHidden();
  });

  test('g d reaches Downloads, g s reaches Settings and g h shows History', async ({ page }) => {
    await page.goto('/statistics');

    await page.keyboard.press('g');
    await page.keyboard.press('d');
    await expect(page).toHaveURL(/\/downloads$/);

    await page.keyboard.press('g');
    await page.keyboard.press('s');
    await expect(page).toHaveURL(/\/settings$/);

    await page.goto('/downloads');
    // Collapse History first so the journey proves the chord expands it.
    const toggle = page.locator('.history-toggle');
    if ((await toggle.getAttribute('aria-expanded')) === 'true') {
      await toggle.click();
      await expect(toggle).toHaveAttribute('aria-expanded', 'false');
    }
    await page.keyboard.press('g');
    await page.keyboard.press('h');
    await expect(page.locator('.history-toggle')).toHaveAttribute('aria-expanded', 'true');
  });

  test('/ focuses the header search box', async ({ page }) => {
    await page.goto('/downloads');

    await page.keyboard.press('/');

    await expect(page).toHaveURL(/\/groups$/);
    const search = page.getByRole('searchbox', { name: 'Search title, poster or subject' });
    await expect(search).toBeFocused();
  });

  // This puts a real toast in the CDK overlay container before pressing `p`.
  // The shell used to skip every shortcut while any overlay pane existed, so a
  // snackbar (a failed poll raises one) silently disabled all of them.
  test('p still works while a snackbar is on screen', async ({ page, request }) => {
    const token = readToken();
    const headers = { Authorization: `Bearer ${token}` };
    const isPaused = async () =>
      (await (await request.get('/api/status', { headers })).json()).paused as boolean;

    const initial = await isPaused();
    try {
      await page.goto('/downloads');
      const pauseButton = page.locator('nav.topbar .pause-group button.action').first();
      await expect(pauseButton).toHaveText(initial ? 'Resume' : 'Pause');

      // A speed-limit change is a local POST; its success snackbar is enough
      // to leave a pane in the overlay container while the key is pressed.
      await page.locator('.speed-group button.pill-btn').click();
      await page.locator('.speed-menu .pm-item', { hasText: '1 MB/s' }).click();
      await expect(page.locator('.mdc-snackbar__label')).toBeVisible();

      await page.keyboard.press('p');
      await expect.poll(isPaused).toBe(!initial);
      await expect(pauseButton).toHaveText(initial ? 'Pause' : 'Resume');

      await page.keyboard.press('p');
      await expect.poll(isPaused).toBe(initial);
    } finally {
      // Leave the speed limit as it was, whatever the assertions did.
      await request.put('/api/config/speed-limit', { headers, data: { speed_limit_bps: 0 } });
      if ((await isPaused()) !== initial) {
        await request.post(`/api/queue/${initial ? 'pause' : 'resume'}`, { headers });
      }
    }
  });

  test('p toggles the global pause and is ignored inside a field', async ({ page, request }) => {
    // The seeded backend starts globally paused and the rest of the suite relies
    // on that. Wait for each toggle to reach the server instead of racing the
    // two posts, and restore the initial state whatever the test does.
    const token = readToken();
    const headers = { Authorization: `Bearer ${token}` };
    const isPaused = async () => ((await (await request.get('/api/status', { headers })).json()).paused as boolean);
    const setPaused = async (paused: boolean) => {
      await request.post(`/api/queue/${paused ? 'pause' : 'resume'}`, { headers });
    };

    const initial = await isPaused();
    try {
      await page.goto('/downloads');

      // The per-row Resume buttons share the name; the global control is in the top bar.
      const pauseButton = page.locator('nav.topbar .pause-group button.action').first();
      await expect(pauseButton).toHaveText(initial ? 'Resume' : 'Pause');

      await page.keyboard.press('p');
      await expect.poll(isPaused).toBe(!initial);
      await expect(pauseButton).toHaveText(initial ? 'Pause' : 'Resume');

      await page.keyboard.press('p');
      await expect.poll(isPaused).toBe(initial);
      await expect(pauseButton).toHaveText(initial ? 'Resume' : 'Pause');

      // In a text field the key is just a letter.
      await page.goto('/settings');
      await page.getByRole('button', { name: 'General' }).click();
      const field = page.locator('input[type="number"]').first();
      await field.click();
      await page.keyboard.press('p');
      await expect(page).toHaveURL(/\/settings$/);
      expect(await isPaused()).toBe(initial);
    } finally {
      if ((await isPaused()) !== initial) await setPaused(initial);
    }
  });
});
