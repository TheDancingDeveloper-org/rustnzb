import '@angular/compiler';

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Observable, of, throwError } from 'rxjs';
import { HttpHeaders, HttpResponse } from '@angular/common/http';

import { ApiService } from '../../core/services/api.service';
import {
  MAX_BACKUP_BYTES,
  SettingsViewComponent,
  filenameFromDisposition,
} from './settings-view.component';

const BACKUP_REPORT = {
  status: true,
  restored: false,
  backup: {
    backup_version: 1,
    created_at: '2026-10-20T09:00:00Z',
    app_version: '1.7.0',
    secrets_included: false,
  },
  summary: { server_count: 1, category_count: 2, rss_feed_count: 0, secrets_kept: 3, warnings: [] },
  restart_required: [],
};

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
    getText: vi.fn(() =>
      of(
        new HttpResponse({
          body: 'masked = true',
          headers: new HttpHeaders({
            'Content-Disposition': 'attachment; filename="rustnzb-backup-20261020T090000Z.toml"',
          }),
        }),
      ),
    ),
    postText: vi.fn(() => of(BACKUP_REPORT)),
    ...overrides,
  };
  const snack = { open: vi.fn(), success: vi.fn(), info: vi.fn(), error: vi.fn(), undo: vi.fn() };
  const confirm = { confirm: vi.fn(() => of(true)) };
  const theme = { set: vi.fn() };
  const notifications = { enabled: vi.fn(() => false), supported: true, permission: vi.fn(() => 'default'), setEnabled: vi.fn() };
  return {
    component: new SettingsViewComponent(api as unknown as ApiService, snack as never, confirm as never, theme as never, notifications as never),
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
    expect(snack.info).toHaveBeenCalledWith('Host is required');

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

describe('SettingsViewComponent unsaved changes', () => {
  it('is clean after load and dirty after an edit', () => {
    const { component } = makeComponent();
    component.ngOnInit();
    expect(component.hasUnsavedChanges()).toBe(false);

    component.speedLimit = 999;
    expect(component.unsavedSections()).toEqual(['speedLimit']);
    component.speedLimit = 123;
    expect(component.hasUnsavedChanges()).toBe(false);
  });

  it('becomes clean once the save succeeds', () => {
    const { component } = makeComponent();
    component.ngOnInit();
    component.abortHopeless = true;
    expect(component.unsavedSections()).toEqual(['diskGuards']);
    component.saveDiskGuards();
    expect(component.hasUnsavedChanges()).toBe(false);
  });

  it('stays dirty when the save fails', () => {
    const { component } = makeComponent({ put: vi.fn(() => throwError(() => new Error('offline'))) });
    component.ngOnInit();
    component.maxActiveDownloads = 9;
    component.saveMaxActive();
    expect(component.unsavedSections()).toEqual(['maxActive']);
  });

  it('keeps edits made while a save is in flight unsaved', () => {
    let finish!: () => void;
    const put = vi.fn(
      () =>
        new Observable((sub) => {
          finish = () => {
            sub.next({});
            sub.complete();
          };
        }),
    );
    const { component } = makeComponent({ put });
    component.ngOnInit();
    component.historyRetention = 30;
    component.saveRetention();
    component.historyRetention = 60;
    finish();
    expect(component.unsavedSections()).toEqual(['historyRetention']);
  });

  it('does not treat a form whose load failed as dirty', () => {
    const { component } = makeComponent({
      get: vi.fn((path: string) =>
        path === '/config/speed-limit' ? throwError(() => new Error('down')) : of(path.endsWith('servers') || path.endsWith('categories') || path.endsWith('stats') ? [] : {}),
      ),
    });
    component.ngOnInit();
    component.speedLimit = 5;
    expect(component.unsavedSections()).not.toContain('speedLimit');
  });

  it('counts an open server or category editor as unsaved', () => {
    const { component } = makeComponent();
    component.ngOnInit();
    component.addServer();
    component.addCategory();
    expect(component.unsavedSections()).toEqual(['server', 'category']);
    component.cancelServerEdit();
    component.cancelCategoryEdit();
    expect(component.hasUnsavedChanges()).toBe(false);
  });

  it('asks the browser to confirm unload only when dirty', () => {
    const { component } = makeComponent();
    component.ngOnInit();
    const clean = { preventDefault: vi.fn() } as unknown as BeforeUnloadEvent;
    component.onBeforeUnload(clean);
    expect(clean.preventDefault).not.toHaveBeenCalled();

    component.speedLimit = 1;
    const dirty = { preventDefault: vi.fn() } as unknown as BeforeUnloadEvent;
    component.onBeforeUnload(dirty);
    expect(dirty.preventDefault).toHaveBeenCalled();
  });
});

describe('SettingsViewComponent backup & restore', () => {
  let saved: { filename: string; text: string } | null;
  let click: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    saved = null;
    click = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(function (
      this: HTMLAnchorElement,
    ) {
      saved = { filename: this.download, text: String((this as unknown as { __text?: string }).__text ?? '') };
    });
    URL.createObjectURL = vi.fn(() => 'blob:backup');
    URL.revokeObjectURL = vi.fn();
  });

  afterEach(() => {
    click.mockRestore();
  });

  it('downloads a masked backup by default and never asks for secrets', () => {
    const { component, api, snack } = makeComponent();
    component.ngOnInit();

    component.downloadBackup();

    expect(api.getText).toHaveBeenCalledWith('/config/backup', undefined);
    expect(snack.info).toHaveBeenCalledWith('Backup downloaded');
    // The name comes from the server, not from a guess on the client.
    expect(saved?.filename).toBe('rustnzb-backup-20261020T090000Z.toml');
  });

  it('asks for secrets only when the operator opts in', () => {
    const { component, api } = makeComponent();
    component.ngOnInit();
    component.backupIncludeSecrets = true;

    component.downloadBackup();

    expect(api.getText).toHaveBeenCalledWith('/config/backup', { secrets: 'true' });
  });

  it('surfaces a failed download', () => {
    const { component, snack } = makeComponent({
      getText: vi.fn(() => throwError(() => new Error('offline'))),
    });
    component.ngOnInit();

    component.downloadBackup();

    expect(snack.open).toHaveBeenCalledWith('Failed to download backup', 'Close', { duration: 5000 });
    expect(component.backupBusy).toBe(false);
  });

  it('will not enable Restore until a preview of the current file has succeeded', () => {
    const { component } = makeComponent();
    component.ngOnInit();
    expect(component.backupPreviewMatchesFile).toBe(false);

    component.backupText = '[backup]\nbackup_version = 1\n[config]\n';
    expect(component.backupPreviewMatchesFile).toBe(false);

    component.previewBackup();
    expect(component.backupPreviewMatchesFile).toBe(true);
    expect(component.backupPreview?.summary.secrets_kept).toBe(3);

    // A different file invalidates the preview, so a stale one cannot be restored.
    component.backupText = '[backup]\nbackup_version = 1\n[config]\n# edited\n';
    expect(component.backupPreviewMatchesFile).toBe(false);
  });

  it('reports the server’s refusal without enabling restore', () => {
    const { component, api, snack } = makeComponent({
      postText: vi.fn(() =>
        throwError(() => ({
          status: 400,
          error: { human_readable: 'general.port must be 1-65535' },
        })),
      ),
    });
    component.ngOnInit();
    component.backupText = '[backup]\nbackup_version = 1\n[config]\n';

    component.previewBackup();

    expect(component.backupPreview).toBeNull();
    expect(component.backupPreviewMatchesFile).toBe(false);
    expect(component.backupError).toBe('This backup was refused: general.port must be 1-65535');
    expect(snack.open).toHaveBeenCalledWith(
      'This backup was refused: general.port must be 1-65535',
      'Close',
      { duration: 5000 },
    );
  });

  it('restores only after confirmation, then refetches the settings it replaced', () => {
    const restored = {
      ...BACKUP_REPORT,
      restored: true,
      restart_required: ['general.port'],
    };
    const { component, api, confirm, snack } = makeComponent({
      postText: vi.fn((path: string) => of(path.endsWith('/restore') ? restored : BACKUP_REPORT)),
    });
    component.ngOnInit();
    component.backupText = '[backup]\nbackup_version = 1\n[config]\n';
    component.previewBackup();
    const getsBefore = api.get.mock.calls.length;

    component.restoreBackup();

    expect(confirm.confirm).toHaveBeenCalledWith(
      expect.objectContaining({ title: 'Restore this configuration?', danger: true }),
    );
    expect(api.postText).toHaveBeenCalledWith('/config/backup/restore', component.backupText);
    expect(api.get.mock.calls.length).toBeGreaterThan(getsBefore);
    expect(snack.info).toHaveBeenCalledWith(
      'Configuration restored. Restart to apply: general.port',
      6000,
    );
  });

  it('does not restore when the confirmation is declined', () => {
    const { component, api, confirm } = makeComponent({
      postText: vi.fn(() => of(BACKUP_REPORT)),
    });
    confirm.confirm = vi.fn(() => of(false));
    component.ngOnInit();
    component.backupText = '[backup]\nbackup_version = 1\n[config]\n';
    component.previewBackup();

    component.restoreBackup();

    expect(api.postText).toHaveBeenCalledTimes(1);
    expect(api.postText).toHaveBeenCalledWith('/config/backup/preview', component.backupText);
  });

  it('does not restore a file it has not previewed', () => {
    const { component, api, confirm } = makeComponent();
    component.ngOnInit();
    component.backupText = '[backup]\nbackup_version = 1\n';

    component.restoreBackup();

    expect(confirm.confirm).not.toHaveBeenCalled();
    expect(api.postText).not.toHaveBeenCalled();
  });

  it('refuses an oversized file before sending it', () => {
    const { component, api } = makeComponent();
    component.ngOnInit();
    const file = new File(['x'], 'huge.toml');
    Object.defineProperty(file, 'size', { value: MAX_BACKUP_BYTES + 1 });

    component.chooseBackupFile({ target: { files: [file] } } as unknown as Event);

    expect(component.backupError).toContain('larger than');
    expect(component.backupText).toBe('');
    expect(api.postText).not.toHaveBeenCalled();
  });

  it('surfaces a failed restore and keeps the preview so it can be retried', () => {
    const { component, snack } = makeComponent({
      postText: vi.fn((path: string) =>
        path.endsWith('/restore') ? throwError(() => new Error('offline')) : of(BACKUP_REPORT),
      ),
    });
    component.ngOnInit();
    component.backupText = '[backup]\nbackup_version = 1\n[config]\n';
    component.previewBackup();

    component.restoreBackup();

    expect(component.backupPreview).not.toBeNull();
    expect(component.backupError).toBe('Restore failed');
    expect(snack.open).toHaveBeenCalledWith('Restore failed', 'Close', { duration: 5000 });
  });
});

describe('filenameFromDisposition', () => {
  it('reads quoted and bare names', () => {
    expect(filenameFromDisposition('attachment; filename="backup.toml"')).toBe('backup.toml');
    expect(filenameFromDisposition('attachment; filename=backup.toml')).toBe('backup.toml');
    expect(filenameFromDisposition(null)).toBeNull();
    expect(filenameFromDisposition('inline')).toBeNull();
  });
});
