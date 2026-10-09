/**
 * First Boot & Account Setup — journeys 1.1 through 1.8
 *
 * These tests run serially against the fresh backend (port 9191).
 * The backend starts with NO credentials and accumulates state across tests.
 *
 * Ordering contract:
 *   1.3 creates the testadmin account → all subsequent tests (1.4+) inject the
 *   resulting token via addInitScript rather than relying on browser session state.
 */

import { test, expect, Page } from '@playwright/test';
import { FRESH_URL, TEST_USER, TEST_PASS } from '../helpers/api';

// Shared token captured in test 1.3 and reused by 1.4+
let storedToken = '';

/** Inject a valid access token into localStorage before the page loads. */
async function injectToken(page: Page, token: string): Promise<void> {
  await page.addInitScript((tok: string) => {
    localStorage.setItem('access_token', tok);
    localStorage.setItem('refresh_token', 'dummy');
  }, token);
}

test.describe.serial('First boot', () => {
  // ── 1.1 Fresh visit redirects to /login in setup mode ──────────────────────
  test('1.1 fresh visit redirected to /login with setup UI', async ({ page }) => {
    await page.goto('/');
    await expect(page).toHaveURL(/\/login/);
    await expect(page.getByText('Create your account to get started')).toBeVisible();
    await expect(page.getByRole('button', { name: 'Create Account' })).toBeVisible();
    // Confirm Password field must be visible (setup mode only)
    await expect(page.getByLabel(/confirm password/i)).toBeVisible();
  });

  // ── 1.2 Mismatched passwords show validation error ─────────────────────────
  test('1.2 mismatched passwords shows error', async ({ page }) => {
    await page.goto('/login');
    await expect(page.getByText('Create your account to get started')).toBeVisible();

    await page.getByLabel(/^username/i).fill(TEST_USER);
    await page.getByLabel(/^password$/i).fill(TEST_PASS);
    await page.getByLabel(/confirm password/i).fill('different-password');
    await page.getByRole('button', { name: 'Create Account' }).click();

    await expect(page.getByText('Passwords do not match')).toBeVisible();
    await expect(page).toHaveURL(/\/login/);
  });

  // ── 1.3 Valid account creation → /welcome ──────────────────────────────────
  // IMPORTANT: This test creates credentials on the fresh backend. All subsequent
  // tests that need auth inject the token captured here.
  test('1.3 valid account creation redirects to /welcome', async ({ page }) => {
    // Intercept the setup response so we can capture the token
    let capturedToken = '';
    page.on('response', async (response) => {
      if (response.url().includes('/api/auth/setup') && response.status() === 200) {
        try {
          const body = await response.json();
          if (body.access_token) capturedToken = body.access_token;
        } catch {}
      }
    });

    await page.goto('/login');
    await expect(page.getByText('Create your account to get started')).toBeVisible();

    await page.getByLabel(/^username/i).fill(TEST_USER);
    await page.getByLabel(/^password$/i).fill(TEST_PASS);
    await page.getByLabel(/confirm password/i).fill(TEST_PASS);
    await page.getByRole('button', { name: 'Create Account' }).click();

    await expect(page).toHaveURL(/\/welcome/);
    await expect(page.getByRole('heading', { name: 'Welcome to rustnzb' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Import from SABnzbd' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Set up manually' })).toBeVisible();
    await expect(page.getByText(/skip for now/i)).toBeVisible();

    // If the page-level intercept didn't fire in time, fall back to a direct API call
    if (!capturedToken) {
      const resp = await fetch(`${FRESH_URL}/api/auth/login`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ username: TEST_USER, password: TEST_PASS }),
      });
      if (resp.ok) {
        const body = await resp.json();
        capturedToken = body.access_token ?? '';
      }
    }

    if (!capturedToken) throw new Error('Failed to capture access token after account creation');
    storedToken = capturedToken;
  });

  // ── 1.4 Welcome skip → /downloads ───────────────────────────────────────────
  test('1.4 welcome skip goes to downloads', async ({ page }) => {
    await injectToken(page, storedToken);
    await page.goto('/welcome');

    await expect(page.getByRole('heading', { name: 'Welcome to rustnzb' })).toBeVisible();
    await page.getByText(/skip for now/i).click();

    await expect(page).toHaveURL(/\/downloads/);
    await expect(page.getByText(/no downloads in queue/i)).toBeVisible();
  });

  // ── 1.5 "Set up manually" → /settings ─────────────────────────────────────
  test('1.5 set up manually goes to settings', async ({ page }) => {
    await injectToken(page, storedToken);
    await page.goto('/welcome');

    await expect(page.getByRole('heading', { name: 'Welcome to rustnzb' })).toBeVisible();
    await page.getByRole('button', { name: 'Set up manually' }).click();

    await expect(page).toHaveURL(/\/settings/);
    await expect(page.getByRole('heading', { name: 'News servers' })).toBeVisible();
  });

  // ── 1.6 Welcome shows welcome page when no servers are configured ──────────
  // The fresh backend has no servers, so navigating to /welcome should stay on
  // the welcome page rather than auto-redirecting to /downloads.
  test('1.6 welcome page shown when no servers configured', async ({ page }) => {
    await injectToken(page, storedToken);
    await page.goto('/welcome');

    await expect(page).toHaveURL(/\/welcome/);
    await expect(page.getByRole('heading', { name: 'Welcome to rustnzb' })).toBeVisible();
  });

  // ── 1.7 Import wizard — "Fetch config" with empty URL shows error ──────────
  test('1.7 import wizard empty URL shows validation error', async ({ page }) => {
    await injectToken(page, storedToken);
    await page.goto('/welcome');

    await page.getByRole('button', { name: 'Import from SABnzbd' }).click();

    // Should be on the connect step (Live instance tab is default)
    // Tabs are rendered as plain <button>, not ARIA tab role.
    await expect(page.getByRole('button', { name: /live instance/i })).toBeVisible();

    // Click Fetch config without filling any fields
    await page.getByRole('button', { name: /fetch config/i }).click();

    await expect(page.getByText(/SABnzbd URL is required/i)).toBeVisible();
    // Still on the connect step — Import from SABnzbd wizard still showing
    await expect(page.getByRole('button', { name: /fetch config/i })).toBeVisible();
  });

  // ── 1.8 Unreachable host shows connection error ────────────────────────────
  test('1.8 unreachable host shows connection error', async ({ page }) => {
    await injectToken(page, storedToken);
    await page.goto('/welcome');

    await page.getByRole('button', { name: 'Import from SABnzbd' }).click();
    await expect(page.getByRole('button', { name: /live instance/i })).toBeVisible();

    // A loopback URL. The fetch guard refuses private addresses before any
    // connection attempt, and the page shows that reason (human_readable)
    // rather than the generic fallback.
    const urlField = page.getByPlaceholder(/http.*localhost.*8080/i);
    await urlField.fill('http://localhost:1');

    const apiKeyField = page.getByPlaceholder(/32-character hex key/i);
    await apiKeyField.fill('dummy-api-key-1234');

    await page.getByRole('button', { name: /fetch config/i }).click();

    // Wait for the error — connecting to :1 will fail quickly
    await expect(page.getByText(/private\/reserved address|failed to connect/i)).toBeVisible({ timeout: 15000 });
    // Still on connect step
    await expect(page.getByRole('button', { name: /fetch config/i })).toBeVisible();
  });

  // ── 1.9 Regression — typing in URL/API key inputs retains all characters ───
  // Guards against a zoneless-Angular bug where the form block would tear down
  // and destroy the input after a single keystroke. See commits fixing the
  // welcome component (signals + [value]/(input) bindings, [hidden] for tabs).
  test('1.9 typing multi-character strings into URL + API key preserves value', async ({ page }) => {
    await injectToken(page, storedToken);
    await page.goto('/welcome');

    await page.getByRole('button', { name: 'Import from SABnzbd' }).click();
    await expect(page.getByRole('button', { name: /live instance/i })).toBeVisible();

    const urlValue = 'http://example.sabnzbd.local:8081';
    const apiKeyValue = 'abcdef0123456789abcdef0123456789';

    const urlField = page.getByPlaceholder(/http.*localhost.*8080/i);
    await urlField.fill(urlValue);
    await expect(urlField).toHaveValue(urlValue);

    const apiKeyField = page.getByPlaceholder(/32-character hex key/i);
    await apiKeyField.fill(apiKeyValue);
    await expect(apiKeyField).toHaveValue(apiKeyValue);

    // After typing into the second field, the first must still hold its value.
    await expect(urlField).toHaveValue(urlValue);

    // Type character-by-character to catch per-keystroke teardown regressions.
    await urlField.clear();
    await urlField.pressSequentially('http://a.b:1', { delay: 20 });
    await expect(urlField).toHaveValue('http://a.b:1');
  });

  // ── 1.10 Tab switching keeps both sections' DOM alive ([hidden]) ───────────
  test('1.10 switching between Live instance / .ini tabs preserves form state', async ({ page }) => {
    await injectToken(page, storedToken);
    await page.goto('/welcome');

    await page.getByRole('button', { name: 'Import from SABnzbd' }).click();

    const urlField = page.getByPlaceholder(/http.*localhost.*8080/i);
    await urlField.fill('http://retained.example:9090');

    // Switch to .ini tab, then back to Live instance
    await page.getByRole('button', { name: /config file/i }).click();
    await page.getByRole('button', { name: /live instance/i }).click();

    await expect(urlField).toHaveValue('http://retained.example:9090');
  });
});
