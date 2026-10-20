import { test, expect } from '@playwright/test';
import { readToken } from '../helpers/auth';

const BASE_URL = 'http://localhost:9190';

// Unique per run and test, so a leftover category from an earlier or retried
// run cannot make the add fail with 409 "already exists".
const uniqueName = (base: string): string =>
  `${base}-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;

async function apiAddCategory(token: string, name: string, outputDir?: string): Promise<void> {
  const r = await fetch(`${BASE_URL}/api/config/categories`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
    body: JSON.stringify({ name, output_dir: outputDir ?? null, post_processing: 3 }),
  });
  if (!r.ok) throw new Error(`apiAddCategory failed: ${r.status} ${await r.text()}`);
}

async function apiDeleteCategory(token: string, name: string): Promise<void> {
  const r = await fetch(`${BASE_URL}/api/config/categories/${encodeURIComponent(name)}`, {
    method: 'DELETE',
    headers: { Authorization: `Bearer ${token}` },
  });
  if (!r.ok && r.status !== 404) throw new Error(`apiDeleteCategory failed: ${r.status}`);
}

async function navigateToCategories(page: import('@playwright/test').Page): Promise<void> {
  await page.goto('/settings');
  await page.getByRole('button', { name: 'Categories' }).click();
  // Wait for the categories section to be active
  await expect(page.getByRole('button', { name: '+ Add category' })).toBeVisible();
}

// ── Tests ─────────────────────────────────────────────────────────────────────

test.describe('3. Category Management', () => {
  test.beforeEach(async ({ page }) => {
    page.on('dialog', (dialog) => dialog.accept());
  });

  // ── 3.1 Add a category ────────────────────────────────────────────────────

  test('3.1 add a category', async ({ page }) => {
    const catName = uniqueName('e2e-cat');

    await navigateToCategories(page);

    // Open the add-category form
    await page.getByRole('button', { name: '+ Add category' }).click();

    // Fill name
    await page.getByPlaceholder('movies').fill(catName);

    // Save
    await page.getByRole('button', { name: 'Save' }).click();

    // Row appears
    await expect(page.getByText(catName, { exact: true })).toBeVisible();

    // Snackbar
    await expect(page.getByText('Category added', { exact: false })).toBeVisible();

    // Cleanup
    const token = readToken();
    await apiDeleteCategory(token, catName);
  });

  // ── 3.2 Edit category ─────────────────────────────────────────────────────
  // Add "e2e-cat" in beforeEach (via API), edit it, verify snackbar.

  test('3.2 edit category output directory', async ({ page }) => {
    const catName = uniqueName('e2e-cat');
    const token = readToken();
    await apiAddCategory(token, catName);

    await navigateToCategories(page);

    // Find the row for e2e-cat and click its edit button
    const catRow = page.locator('tr, .cat-row, li', { hasText: catName }).first();
    await expect(catRow).toBeVisible();
    await catRow.getByRole('button', { name: /edit/i }).click();

    // Set an output_dir value — find an input that looks like a path field
    // The name placeholder is "movies"; output_dir has no specific placeholder documented,
    // so we target the second text input in the form.
    const inputs = page.locator('form input[type="text"], form input:not([type])');
    const count = await inputs.count();
    // Fill the last visible text input (output_dir is typically the second field)
    if (count >= 2) {
      await inputs.nth(1).fill('/tmp/e2e-output');
    }

    await page.getByRole('button', { name: 'Save' }).click();

    // Snackbar
    await expect(page.getByText('Category updated', { exact: false })).toBeVisible();

    // Cleanup
    await apiDeleteCategory(token, catName);
  });

  // ── 3.3 Delete category ───────────────────────────────────────────────────

  test('3.3 delete category with confirm', async ({ page }) => {
    const catName = uniqueName('e2e-del');
    const token = readToken();
    await apiAddCategory(token, catName);

    await navigateToCategories(page);

    // Confirm row is visible
    const catRow = page.locator('tr, .cat-row, li', { hasText: catName }).first();
    await expect(catRow).toBeVisible();

    // Click the row action, then confirm in the application's modal.
    await catRow.getByRole('button', { name: /del/i }).click();
    const dialog = page.getByRole('dialog', { name: `Delete category "${catName}"?` });
    await expect(dialog).toBeVisible();
    await dialog.getByRole('button', { name: 'Delete', exact: true }).click();

    // Row should be gone
    await expect(page.getByText(catName, { exact: true })).not.toBeVisible();

    // Snackbar
    await expect(page.getByText('Category removed', { exact: false })).toBeVisible();
  });
});
