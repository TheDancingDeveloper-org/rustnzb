import '@angular/compiler';

import { describe, expect, it, vi } from 'vitest';
import { Observable, of, throwError } from 'rxjs';

import { ApiService } from '../../core/services/api.service';
import { SettingsViewComponent } from './settings-view.component';

function makeComponent(overrides: Record<string, ReturnType<typeof vi.fn>> = {}) {
  const api = {
    get: vi.fn((path: string) => {
      const values: Record<string, unknown> = {
        '/config/servers': [],
        '/config/servers/stats': [],
        '/config/categories': [],
        '/config/speed-limit': { speed_limit_bps: 123 },
        '/config/max-active-downloads': { max_active_downloads: 4 },
        '/config/history-retention': { retention: 14 },
        '/config/disk-guards': { min_free_space_bytes: 2 * 1024 ** 3, abort_hopeless: false },
        '/config/debug-logging': { enabled: false, effective: 'info', overridden: false },
        '/config/sabnzbd-api-key': { api_key: 'key' },
        '/status': { webdav_available: false, webdav_enabled: false },
        '/config': { general: { data_dir: '/data', incomplete_dir: '/incomplete', complete_dir: '/complete', watch_dir: null } },
      };
      return of(values[path]);
    }),
    post: vi.fn(() => of({ api_key: 'rotated' })),
    put: vi.fn(() => of({})),
    delete: vi.fn(() => of({})),
    ...overrides,
  };
  const snack = { open: vi.fn() };
  const confirm = { confirm: vi.fn(() => of(true)) };
  const theme = { set: vi.fn() };
  return {
    component: new SettingsViewComponent(api as unknown as ApiService, snack as never, confirm as never, theme as never),
    api,
    snack,
    confirm,
    theme,
  };
}

describe('SettingsViewComponent', () => {
  it('loads server, category, status, directory, and general settings on initialization', () => {
    const { component, api } = makeComponent();
    component.ngOnInit();

    expect(api.get).toHaveBeenCalledWith('/config/servers');
    expect(api.get).toHaveBeenCalledWith('/config/disk-guards');
    expect(component.speedLimit).toBe(123);
    expect(component.maxActiveDownloads).toBe(4);
    expect(component.minFreeSpaceGB).toBe(2);
    expect(component.abortHopeless).toBe(false);
    expect(component.dirs()?.complete_dir).toBe('/complete');
  });

  it('validates a server host before save and clears blank credentials from API payloads', () => {
    const { component, api, snack } = makeComponent();
    component.ngOnInit();
    component.addServer();
    component.saveServer();
    expect(api.post).not.toHaveBeenCalled();
    expect(snack.open).toHaveBeenCalledWith('Host is required', 'Close', { duration: 3000 });

    component.editingServer = { ...component.editingServer!, host: 'news.example', username: '', password: '' };
    component.saveServer();
    expect(api.post).toHaveBeenCalledWith('/config/servers', expect.objectContaining({ host: 'news.example', username: null, password: null }));
  });

  it('surfaces settings save failures and rotates SAB API keys after confirmation', () => {
    const { component, api, snack, confirm } = makeComponent({
      put: vi.fn(() => throwError(() => new Error('offline'))),
    });
    component.ngOnInit();
    component.speedLimit = 999;
    component.saveSpeedLimit();
    expect(snack.open).toHaveBeenCalledWith('Failed to save speed limit', 'Close', { duration: 5000 });

    component.rotateSabApiKey();
    expect(confirm.confirm).toHaveBeenCalledWith(expect.objectContaining({ title: 'Rotate SABnzbd API key?' }));
    expect(api.post).toHaveBeenCalledWith('/config/sabnzbd-api-key/rotate', {});
    expect(component.sabApiKey).toBe('rotated');
    expect(component.showSabApiKey).toBe(true);
  });

  it('shows the server explanation when a settings save is rejected', () => {
    const { component, snack } = makeComponent({
      put: vi.fn(() =>
        throwError(() => ({
          status: 400,
          error: { error_kind: 'bad_request', human_readable: 'speed_limit_bps must be >= 0', status: 400 },
        })),
      ),
    });
    component.ngOnInit();
    component.saveSpeedLimit();
    expect(snack.open).toHaveBeenCalledWith(
      'Failed to save speed limit: speed_limit_bps must be >= 0',
      'Close',
      { duration: 5000 },
    );
  });

  it('does not save any setting before its load has succeeded', () => {
    const { component, api, confirm } = makeComponent();
    component.speedLimit = 999;
    component.saveSpeedLimit();
    component.saveDiskGuards();
    component.saveDebugLogging();
    component.saveRetention();
    component.saveMaxActive();
    component.saveCategory();
    component.saveServer();
    component.saveDavConfig();
    component.rotateSabApiKey();

    expect(api.put).not.toHaveBeenCalled();
    expect(api.post).not.toHaveBeenCalled();
    expect(confirm.confirm).not.toHaveBeenCalled();
    expect(component.canSave('speedLimit')).toBe(false);
    expect(component.loadFailure('speedLimit')).toBeNull();
  });

  it('shows a load failure and keeps Save blocked, so defaults never overwrite the real config', () => {
    const { component, api, snack } = makeComponent();
    failGet(api, '/config/disk-guards');
    component.ngOnInit();

    expect(component.loadFailure('diskGuards')).toBe('Failed to load disk guards');
    expect(component.canSave('diskGuards')).toBe(false);
    expect(component.canSave('speedLimit')).toBe(true);

    component.saveDiskGuards();
    expect(api.put).not.toHaveBeenCalledWith('/config/disk-guards', expect.anything());
    expect(snack.open).not.toHaveBeenCalled();
  });

  it('unblocks Save once a retry of the failed load succeeds', () => {
    const { component, api } = makeComponent();
    const restore = failGet(api, '/config/disk-guards');
    component.ngOnInit();

    restore();
    component.loadDiskGuards();
    expect(component.loadFailure('diskGuards')).toBeNull();
    expect(component.canSave('diskGuards')).toBe(true);
    expect(component.minFreeSpaceGB).toBe(2);

    component.saveDiskGuards();
    expect(api.put).toHaveBeenCalledWith('/config/disk-guards', {
      min_free_space_bytes: 2 * 1024 ** 3,
      abort_hopeless: false,
    });
  });

  it('does not rotate the SAB API key when its current value failed to load', () => {
    const { component, api, confirm } = makeComponent();
    failGet(api, '/config/sabnzbd-api-key');
    component.ngOnInit();

    expect(component.loadFailure('sabApiKey')).toBe('Failed to load SABnzbd API key');
    component.rotateSabApiKey();
    expect(confirm.confirm).not.toHaveBeenCalled();
    expect(api.post).not.toHaveBeenCalledWith('/config/sabnzbd-api-key/rotate', {});
  });

  it('blocks Media Library saves when the DAV config failed to load', () => {
    const { component, api } = makeComponent();
    const load = api.get.getMockImplementation() as (p: string) => Observable<unknown>;
    api.get.mockImplementation((path: string) => {
      if (path === '/status') return of({ webdav_available: true, webdav_enabled: true });
      if (path === '/config/dav') return throwError(() => new Error('offline'));
      return load(path);
    });
    component.ngOnInit();

    expect(component.loadFailure('dav')).toBe('Failed to load Media Library settings');
    component.saveDavConfig();
    expect(api.put).not.toHaveBeenCalledWith('/config/dav', expect.anything());
  });

  it('surfaces a failed status load as a toast', () => {
    const { component, api, snack } = makeComponent();
    failGet(api, '/status');
    component.ngOnInit();

    expect(snack.open).toHaveBeenCalledWith('Failed to load server status', 'Close', { duration: 5000 });
  });
});

/**
 * Make one GET path fail, leaving the other paths answering as the default fixture does.
 * Returns the function that puts the fixture back.
 */
function failGet(api: { get: ReturnType<typeof vi.fn> }, path: string): () => void {
  const answer = api.get.getMockImplementation() as (p: string) => Observable<unknown>;
  api.get.mockImplementation((p: string) => (p === path ? throwError(() => new Error('offline')) : answer(p)));
  return () => api.get.mockImplementation(answer);
}
