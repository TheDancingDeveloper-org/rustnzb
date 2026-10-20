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
        '/config/schedule': {
          enabled: false,
          rules: [],
          active_rule_ids: [],
          paused: false,
          speed_limit_bps: null,
          disabled_servers: [],
        },
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
    component.saveSchedule();
    component.rotateSabApiKey();

    expect(api.put).not.toHaveBeenCalled();
    expect(api.post).not.toHaveBeenCalled();
    expect(confirm.confirm).not.toHaveBeenCalled();
    expect(component.canSave('speedLimit')).toBe(false);
    expect(component.canSave('schedule')).toBe(false);
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

  it('loads the schedule and shows which rules are active now', () => {
    const { component, api } = makeComponent({
      get: vi.fn((path: string) => {
        if (path === '/config/schedule') {
          return of({
            enabled: true,
            rules: [
              {
                id: 'quiet',
                name: 'Quiet hours',
                days: ['mon', 'monday', 'tue'],
                start: '22:00',
                end: '06:00',
                action: 'pause',
                speed_limit_bps: 0,
                servers: [],
              },
            ],
            active_rule_ids: ['quiet'],
            paused: true,
            speed_limit_bps: null,
            disabled_servers: [],
          });
        }
        if (path === '/config/servers') return of([]);
        if (path === '/config/categories' || path === '/config/servers/stats') return of([]);
        if (path === '/status') return of({ webdav_available: false, webdav_enabled: false });
        if (path === '/config') return of({ general: {} });
        return of({});
      }),
    });
    component.ngOnInit();

    expect(api.get).toHaveBeenCalledWith('/config/schedule');
    expect(component.schedule.rules[0].days).toEqual(['mon', 'tue']);
    expect(component.scheduleActiveNow()).toBe(true);
    expect(component.activeScheduleRules().map((rule) => rule.name)).toEqual(['Quiet hours']);
    expect(component.actionLabel('pause')).toBe('pause');
    expect(component.unsavedSections()).toEqual([]);
  });

  it('validates a draft schedule rule before saving and names the offending row', () => {
    const { component, api, snack } = makeComponent();
    component.ngOnInit();

    component.addScheduleRule();
    expect(component.canSave('schedule')).toBe(true);
    expect(component.schedule.rules[0].days).toEqual(['mon', 'tue', 'wed', 'thu', 'fri']);
    expect(component.scheduleProblem()).toBeNull();

    component.schedule.rules[0].start = '25:00';
    expect(component.ruleProblem(component.schedule.rules[0])).toContain('Start must be HH:MM');
    expect(component.scheduleProblem()).toBe('Rule 1: Start must be HH:MM (00:00 to 23:59).');
    component.saveSchedule();
    expect(api.put).not.toHaveBeenCalledWith('/config/schedule', expect.anything());
    expect(snack.open).toHaveBeenCalledWith(
      'Rule 1: Start must be HH:MM (00:00 to 23:59).',
      'Close',
      { duration: 4000 },
    );

    component.schedule.rules[0].start = '22:00';
    component.schedule.rules[0].end = '22:00';
    expect(component.ruleProblem(component.schedule.rules[0])).toContain('must differ');

    component.schedule.rules[0].action = 'disable_servers';
    component.schedule.rules[0].end = '06:00';
    expect(component.ruleProblem(component.schedule.rules[0])).toContain('at least one server');
    component.toggleScheduleServer(component.schedule.rules[0], 'primary', {
      target: { checked: true },
    } as unknown as Event);
    expect(component.scheduleProblem()).toBeNull();
  });

  it('saves the schedule and takes the ids and live status from the response', () => {
    const put = vi.fn(() =>
      of({
        enabled: true,
        rules: [
          {
            id: 'assigned',
            name: 'Off peak',
            days: ['sat', 'sun'],
            start: '00:00',
            end: '24:00',
            action: 'speed_limit',
            speed_limit_bps: 1_000_000,
            servers: [],
          },
        ],
        active_rule_ids: ['assigned'],
        paused: false,
        speed_limit_bps: 1_000_000,
        disabled_servers: [],
      }),
    );
    const { component, api, snack } = makeComponent({ put });
    component.ngOnInit();

    component.schedule.enabled = true;
    component.addScheduleRule();
    component.schedule.rules[0].name = '  Off peak  ';
    component.schedule.rules[0].days = ['sat', 'sun'];
    component.schedule.rules[0].start = '00:00';
    component.schedule.rules[0].end = '24:00';
    component.schedule.rules[0].action = 'speed_limit';
    component.schedule.rules[0].speed_limit_bps = 1_000_000;
    component.saveSchedule();

    expect(api.put).toHaveBeenCalledWith('/config/schedule', {
      enabled: true,
      rules: [
        {
          id: '',
          name: 'Off peak',
          days: ['sat', 'sun'],
          start: '00:00',
          end: '24:00',
          action: 'speed_limit',
          speed_limit_bps: 1_000_000,
          servers: [],
        },
      ],
    });
    expect(component.schedule.rules[0].id).toBe('assigned');
    expect(component.scheduleActiveNow()).toBe(true);
    expect(component.formatSpeedValue(component.schedule.speed_limit_bps)).toContain('/s');
    expect(snack.open).toHaveBeenCalledWith('Schedule saved', 'Close', { duration: 2000 });
    expect(component.hasUnsavedChanges()).toBe(false);
  });

  it('shows an unlimited schedule window as unlimited rather than a zero cap', () => {
    const { component } = makeComponent();
    // `0` is an explicit unlimited window; `null` means no scheduled cap.
    // Neither may render as "0 B/s", which reads as a cap that stops
    // every download.
    expect(component.formatSpeedValue(0)).toBe('unlimited');
    expect(component.formatSpeedValue(null)).toBe('unlimited');
    expect(component.formatSpeedValue(2 * 1024 * 1024)).toBe('2.0 MB/s');
  });

  it('keeps schedule edits dirty when the save fails and blocks saving after a failed load', () => {
    const { component, api } = makeComponent({
      put: vi.fn(() => throwError(() => new Error('offline'))),
    });
    component.ngOnInit();
    component.addScheduleRule();
    component.schedule.rules[0].start = '23:00';
    expect(component.unsavedSections()).toEqual(['schedule']);
    component.saveSchedule();
    expect(component.unsavedSections()).toEqual(['schedule']);

    const { component: blocked, api: blockedApi } = makeComponent();
    failGet(blockedApi, '/config/schedule');
    blocked.ngOnInit();
    expect(blocked.loadFailure('schedule')).toBe('Failed to load the schedule');
    blocked.saveSchedule();
    expect(blockedApi.put).not.toHaveBeenCalledWith('/config/schedule', expect.anything());
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
