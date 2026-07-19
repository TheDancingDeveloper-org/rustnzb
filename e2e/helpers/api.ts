/**
 * API helpers for test setup and teardown.
 * These use raw fetch so they work in globalSetup (no Playwright context needed).
 */

export const MAIN_URL = 'http://localhost:9190';
export const FRESH_URL = 'http://localhost:9191';
export const MOCK_URL = 'http://localhost:9192';

export const TEST_USER = 'testadmin';
export const TEST_PASS = 'testpassword123';

/** Call POST /api/auth/setup and return the tokens. */
export async function setupAuth(baseUrl: string, username = TEST_USER, password = TEST_PASS) {
  const r = await fetch(`${baseUrl}/api/auth/setup`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username, password }),
  });
  if (!r.ok) throw new Error(`Auth setup failed: ${r.status} ${await r.text()}`);
  return r.json() as Promise<{ access_token: string; refresh_token: string }>;
}

/** Call POST /api/auth/login and return tokens. */
export async function login(baseUrl: string, username = TEST_USER, password = TEST_PASS) {
  const r = await fetch(`${baseUrl}/api/auth/login`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username, password }),
  });
  if (!r.ok) throw new Error(`Login failed: ${r.status}`);
  return r.json() as Promise<{ access_token: string; refresh_token: string }>;
}

/** Build a Playwright storageState object from auth tokens. */
export function buildStorageState(baseUrl: string, tokens: { access_token: string; refresh_token: string }) {
  return {
    cookies: [],
    origins: [{
      origin: baseUrl,
      localStorage: [
        { name: 'access_token', value: tokens.access_token },
        { name: 'refresh_token', value: tokens.refresh_token },
      ],
    }],
  };
}

/** Add a server via API. Returns the created server id. */
export async function apiAddServer(baseUrl: string, token: string, server?: Partial<ServerConfig>): Promise<void> {
  const body: ServerConfig = {
    id: '',
    name: server?.name ?? 'API Test Server',
    host: server?.host ?? 'news.api-test.com',
    port: server?.port ?? 563,
    ssl: server?.ssl ?? true,
    ssl_verify: server?.ssl_verify ?? false,
    username: server?.username ?? null,
    password: server?.password ?? null,
    connections: server?.connections ?? 4,
    priority: server?.priority ?? 0,
    enabled: server?.enabled ?? true,
    retention: server?.retention ?? 0,
    pipelining: server?.pipelining ?? 16,
    optional: server?.optional ?? false,
    compress: server?.compress ?? false,
    ramp_up_delay_ms: server?.ramp_up_delay_ms ?? 50,
    proxy_url: null,
    trusted_fingerprint: null,
  };
  const r = await fetch(`${baseUrl}/api/config/servers`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
    body: JSON.stringify(body),
  });
  if (!r.ok) throw new Error(`addServer failed: ${r.status} ${await r.text()}`);
}

/** Delete a server by name. */
export async function apiDeleteServer(baseUrl: string, token: string, id: string): Promise<void> {
  const r = await fetch(`${baseUrl}/api/config/servers/${id}`, {
    method: 'DELETE',
    headers: { Authorization: `Bearer ${token}` },
  });
  if (!r.ok && r.status !== 404) throw new Error(`deleteServer failed: ${r.status}`);
}

/** List servers. */
export async function apiListServers(baseUrl: string, token: string): Promise<ServerConfig[]> {
  const r = await fetch(`${baseUrl}/api/config/servers`, {
    headers: { Authorization: `Bearer ${token}` },
  });
  return r.json();
}

/** Add a category via API. */
export async function apiAddCategory(baseUrl: string, token: string, name: string, outputDir?: string): Promise<void> {
  const r = await fetch(`${baseUrl}/api/config/categories`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
    body: JSON.stringify({ name, output_dir: outputDir ?? null, post_processing: 3 }),
  });
  if (!r.ok) throw new Error(`addCategory failed: ${r.status} ${await r.text()}`);
}

/** Delete a category by name. */
export async function apiDeleteCategory(baseUrl: string, token: string, name: string): Promise<void> {
  const r = await fetch(`${baseUrl}/api/config/categories/${encodeURIComponent(name)}`, {
    method: 'DELETE',
    headers: { Authorization: `Bearer ${token}` },
  });
  if (!r.ok && r.status !== 404) throw new Error(`deleteCategory failed: ${r.status}`);
}

/** Add an RSS feed via API. */
export async function apiAddFeed(baseUrl: string, token: string, name: string, url: string): Promise<void> {
  const r = await fetch(`${baseUrl}/api/config/rss-feeds`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
    body: JSON.stringify({ name, url, poll_interval_secs: 900, enabled: true, auto_download: false }),
  });
  if (!r.ok) throw new Error(`addFeed failed: ${r.status} ${await r.text()}`);
}

/** Delete an RSS feed by name. */
export async function apiDeleteFeed(baseUrl: string, token: string, name: string): Promise<void> {
  const r = await fetch(`${baseUrl}/api/config/rss-feeds/${encodeURIComponent(name)}`, {
    method: 'DELETE',
    headers: { Authorization: `Bearer ${token}` },
  });
  if (!r.ok && r.status !== 404) throw new Error(`deleteFeed failed: ${r.status}`);
}

// ── Types ──────────────────────────────────────────────────────────────────────

interface ServerConfig {
  id: string;
  name: string;
  host: string;
  port: number;
  ssl: boolean;
  ssl_verify: boolean;
  username: string | null;
  password: string | null;
  connections: number;
  priority: number;
  enabled: boolean;
  retention: number;
  pipelining: number;
  optional: boolean;
  compress: boolean;
  ramp_up_delay_ms: number;
  proxy_url: string | null;
  trusted_fingerprint?: string | null;
}
