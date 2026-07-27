import '@angular/compiler';

import { describe, expect, it, vi } from 'vitest';
import { of, throwError } from 'rxjs';

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
    component.speedLimit = 999;
    component.saveSpeedLimit();
    expect(snack.open).toHaveBeenCalledWith('Failed to save speed limit', 'Close', { duration: 3000 });

    component.rotateSabApiKey();
    expect(confirm.confirm).toHaveBeenCalledWith(expect.objectContaining({ title: 'Generate SABnzbd API key?' }));
    expect(api.post).toHaveBeenCalledWith('/config/sabnzbd-api-key/rotate', {});
    expect(component.sabApiKey).toBe('rotated');
    expect(component.showSabApiKey).toBe(true);
  });
});
