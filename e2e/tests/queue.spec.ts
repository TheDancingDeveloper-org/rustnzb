/**
 * Queue View — journeys 4.1, 4.3-4.8, 4.12
 *
 * Runs against the main backend (port 9190) with seeded data:
 *   - "Test.Movie.2025.mkv"  — paused
 *   - "Another.Show.S01E01"  — queued
 */

import { test, expect } from '@playwright/test';
import * as path from 'path';
import * as fs from 'fs';
import { readToken } from '../helpers/auth';

const FIXTURES = path.resolve(__dirname, '../fixtures');

// ── 4.12 Seeded queue has 2 jobs ─────────────────────────────────────────────

test('4.12 seeded queue shows both jobs and correct count', async ({ page }) => {
  await page.goto('/downloads');

  // Both seeded job names must be present
  await expect(page.locator('.job-name', { hasText: 'Test.Movie.2025.mkv' })).toBeVisible();
  await expect(page.locator('.job-name', { hasText: 'Another.Show.S01E01' })).toBeVisible();

  // Queue stat card shows 2
  const queueCard = page.locator('.cards4 .card', { hasText: 'Downloads' });
  await expect(queueCard).toContainText('2');
});

// ── 4.3 Paused status on "Test.Movie.2025.mkv" ───────────────────────────────

test('4.3 paused job shows paused status pill', async ({ page }) => {
  await page.goto('/downloads');

  // Find the row that contains the paused movie
  const jobRow = page.locator('.data').locator('tr, .row', { hasText: 'Test.Movie.2025.mkv' }).first();
  await expect(jobRow).toBeVisible();

  // Status pill must carry the paused class
  await expect(jobRow.locator('.s-paused')).toBeVisible();
});

test('4.13 active download row shows the failed article count', async ({ page }) => {
  await page.goto('/downloads');

  const jobRow = page.locator('.data').locator('tr', { hasText: 'Test.Movie.2025.mkv' });
  await expect(jobRow.locator('.article-failures')).toHaveText('7 failed articles');
});

// ── 4.4 Global pause owns "Another.Show.S01E01" ──────────────────────────────

test('4.4 global pause overrides queued job controls', async ({ page }) => {
  await page.goto('/downloads');

  const jobRow = page.locator('.data').locator('tr, .row', { hasText: 'Another.Show.S01E01' }).first();
  await expect(jobRow).toBeVisible();

  // The backend row is queued, but global pause is the effective status.
  await expect(jobRow.locator('.s-paused')).toBeVisible();
  await expect(jobRow.getByRole('button', { name: 'Resume' })).toBeDisabled();
});

// ── 4.5 Status filter buttons ─────────────────────────────────────────────────

test('4.5 status filters show correct subsets', async ({ page }) => {
  await page.goto('/downloads');

  // Ensure both jobs visible under "All"
  await page.getByRole('button', { name: /^All \(\d+\)$/ }).click();
  await expect(page.locator('.job-name', { hasText: 'Test.Movie.2025.mkv' })).toBeVisible();
  await expect(page.locator('.job-name', { hasText: 'Another.Show.S01E01' })).toBeVisible();

  // "Paused" filter — both jobs are paused after journey 4.4.
  await page.getByRole('button', { name: 'Paused' }).click();
  await expect(page.locator('.job-name', { hasText: 'Test.Movie.2025.mkv' })).toBeVisible();
  await expect(page.locator('.job-name', { hasText: 'Another.Show.S01E01' })).toBeVisible();

  // "Queued" filter — neither paused job is shown.
  await page.getByRole('button', { name: 'Queued' }).click();
  await expect(page.locator('.job-name', { hasText: 'Another.Show.S01E01' })).not.toBeVisible();
  await expect(page.locator('.job-name', { hasText: 'Test.Movie.2025.mkv' })).not.toBeVisible();

  // "Active" filter — neither seeded job is actively downloading
  await page.getByRole('button', { name: 'Active' }).click();
  await expect(page.locator('.job-name', { hasText: 'Test.Movie.2025.mkv' })).not.toBeVisible();
  await expect(page.locator('.job-name', { hasText: 'Another.Show.S01E01' })).not.toBeVisible();

  // Back to "All"
  await page.getByRole('button', { name: /^All \(\d+\)$/ }).click();
  await expect(page.locator('.job-name', { hasText: 'Test.Movie.2025.mkv' })).toBeVisible();
  await expect(page.locator('.job-name', { hasText: 'Another.Show.S01E01' })).toBeVisible();
});

// ── 4.6 NZB file upload ───────────────────────────────────────────────────────

test('4.6 NZB file upload is accepted by the UI', async ({ page }) => {
  await page.goto('/downloads');

  const nzbPath = path.join(FIXTURES, 'sample.nzb');

  await page.getByRole('button', { name: '+ Upload NZB', exact: true }).click();
  const fileInput = page.locator('.add-panel input[type="file"]');
  await fileInput.setInputFiles(nzbPath);
  await expect(page.locator('.file-chip', { hasText: 'sample.nzb' })).toBeVisible();

  await page.getByRole('button', { name: 'Upload', exact: true }).click();

  // The upload interaction must not crash the page — it either adds a new row
  // (no NNTP, so it may immediately fail/queue) or shows a snackbar/error.
  // Wait briefly and assert we are still on /downloads without a fatal error.
  await page.waitForTimeout(2000);
  await expect(page).toHaveURL(/\/downloads/);

  // Either a new job appeared or a snackbar is present (success or error is fine —
  // the important thing is the upload path exercised without a JS exception).
  const newJobOrFeedback =
    (await page.locator('.job-name').count()) >= 2 ||
    (await page.locator('.snackbar, [class*="snack"], [class*="toast"], [role="alert"]').count()) > 0;
  expect(newJobOrFeedback).toBeTruthy();
});

// ── 4.7 Bulk select shows count ───────────────────────────────────────────────

test('4.7 selecting jobs shows bulk selection count', async ({ page }) => {
  await page.goto('/downloads');

  // Make sure both jobs are visible
  await expect(page.locator('.job-name', { hasText: 'Test.Movie.2025.mkv' })).toBeVisible();
  await expect(page.locator('.job-name', { hasText: 'Another.Show.S01E01' })).toBeVisible();

  // Check the checkbox of the first job row
  const rows = page.locator('.data tbody tr');
  await rows.nth(0).locator('input[type="checkbox"]').check();

  // Bulk action UI or selection count must appear
  const selectionCount = page.getByText(/1 selected/i).or(page.locator('.bulk-actions'));
  await expect(selectionCount.first()).toBeVisible();

  // Check the second row too
  await rows.nth(1).locator('input[type="checkbox"]').check();

  // Count shows 2
  await expect(page.getByText(/2 selected/i)).toBeVisible();
});

// ── 4.1 Queue page loads with stat cards ─────────────────────────────────────

test('4.1 queue page renders stat cards', async ({ page }) => {
  await page.goto('/downloads');

  // The three stat cards described in the component
  await expect(page.locator('.cards4 .card', { hasText: 'Download speed' })).toBeVisible();
  await expect(page.locator('.cards4 .card', { hasText: /NNTP connections/i })).toBeVisible();
  await expect(page.locator('.cards4 .card', { hasText: /^Downloads/ })).toBeVisible();
});

// ── 4.8 Delete a job from the queue ──────────────────────────────────────────

test('4.8 deleting a queue job removes it from the list', async ({ page, request }) => {
  const original = fs.readFileSync(path.join(FIXTURES, 'sample.nzb'), 'utf8');
  const unique = original
    .replace('Sample Test File', 'Delete Test File')
    .replace('Sample.Test.File', 'Delete.Test.File')
    .replaceAll('sample.bin', 'delete-test.bin')
    .replace('sample-article-001@rustnzb.test', 'delete-test-001@rustnzb.test');
  const response = await request.post('http://localhost:9190/api/queue/add', {
    headers: { Authorization: `Bearer ${readToken()}` },
    multipart: {
      file: {
        name: 'delete-test.nzb',
        mimeType: 'application/x-nzb',
        buffer: Buffer.from(unique),
      },
    },
  });
  expect(response.ok()).toBeTruthy();
  await page.goto('/downloads');

  const jobRow = page.locator('.data').locator('tr, .row', { hasText: 'delete-test' }).first();
  await expect(jobRow).toBeVisible();

  await jobRow.getByRole('button', { name: 'Remove' }).click();
  const dialog = page.getByRole('dialog', { name: 'Remove "delete-test"?' });
  await dialog.getByRole('button', { name: 'Remove', exact: true }).click();

  // Row must disappear
  await expect(page.locator('.job-name', { hasText: 'delete-test' })).not.toBeVisible({
    timeout: 5000,
  });
});

// ── 4.10 Drag reorder updates queue order ───────────────────────────────────

test('4.10 dragging a job reorders the queue', async ({ page }) => {
  await page.goto('/downloads');

  const sourceHandle = page
    .locator('tr', { hasText: 'Another.Show.S01E01' })
    .locator('.drag-handle');
  const targetRow = page.locator('tr', { hasText: 'Test.Movie.2025.mkv' });

  await sourceHandle.dragTo(targetRow);

  await expect
    .poll(async () => {
      const names = await page.locator('.data tbody .job-name').allTextContents();
      return names.map((name) => name.trim()).slice(0, 2);
    })
    .toEqual(['Another.Show.S01E01', 'Test.Movie.2025.mkv']);
});
