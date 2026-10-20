import '@angular/compiler';

import { TestBed } from '@angular/core/testing';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { of } from 'rxjs';

import { ApiService } from './api.service';
import { BrowserNotificationsService, outcomeNotices } from './browser-notifications.service';
import { HistoryEntry, StatusResponse } from '../models/queue.model';

function entry(id: string, status: string, extra: Partial<HistoryEntry> = {}): HistoryEntry {
  return { id, name: `${id}.nzb`, status, error_message: null, ...extra } as HistoryEntry;
}

function status(queueSize: number, pause: StatusResponse['pause_reason'] = null): StatusResponse {
  return {
    version: 'test',
    speed_bps: 0,
    speed_limit_bps: 0,
    queue_size: queueSize,
    disk_space_free: 1,
    disk_space_total: 2,
    min_free_space_bytes: 0,
    paused: pause !== null,
    pause_reason: pause,
    pause_remaining_secs: null,
    webdav_available: false,
    webdav_enabled: false,
    nntp_connections: [],
  };
}

/**
 * A fake `Notification` the service can construct. jsdom has none, so the
 * tests install one and assert on what was shown.
 */
function installNotification(permission: NotificationPermission = 'granted') {
  const shown: Array<{ title: string; body?: string; tag?: string }> = [];
  class FakeNotification {
    static permission: NotificationPermission = permission;
    static requestPermission = vi.fn(async () => FakeNotification.permission);
    onclick: (() => void) | null = null;
    constructor(
      readonly title: string,
      readonly options?: { body?: string; tag?: string },
    ) {
      shown.push({ title, body: options?.body, tag: options?.tag });
    }
  }
  vi.stubGlobal('Notification', FakeNotification);
  return { shown, FakeNotification };
}

/**
 * Build a service with the fake `Notification` already installed. The
 * service reads `typeof Notification` when it is constructed, so the stub
 * has to exist first.
 */
function makeService(permission: NotificationPermission = 'granted') {
  const { shown, FakeNotification } = installNotification(permission);
  const api = {
    get: vi.fn((path: string) => {
      if (path === '/history') return of({ entries: [] });
      return of({});
    }),
  };
  TestBed.configureTestingModule({ providers: [{ provide: ApiService, useValue: api }] });
  const service = TestBed.runInInjectionContext(() => new BrowserNotificationsService());
  return { service, api, shown, FakeNotification };
}

describe('BrowserNotificationsService', () => {
  beforeEach(() => localStorage.clear());
  afterEach(() => {
    TestBed.resetTestingModule();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it('turns on only when the browser grants permission', async () => {
    const { service, FakeNotification } = makeService('granted');

    await expect(service.setEnabled(true)).resolves.toBe(true);
    expect(service.enabled()).toBe(true);
    expect(service.permission()).toBe('granted');
    expect(localStorage.getItem('rustnzb-browser-notifications')).toBe('on');

    FakeNotification.permission = 'denied';
    await expect(service.setEnabled(true)).resolves.toBe(false);
    expect(service.enabled()).toBe(false);
  });

  it('treats a rejected permission request as a refusal', async () => {
    const { service, FakeNotification } = makeService('default');
    FakeNotification.requestPermission = vi.fn(async () => {
      throw new Error('permission request requires a user gesture');
    });

    await expect(service.setEnabled(true)).resolves.toBe(false);
    expect(service.enabled()).toBe(false);
  });

  it('drops the switch when permission is revoked in the browser', async () => {
    const { service, FakeNotification } = makeService('granted');
    await service.setEnabled(true);
    expect(service.enabled()).toBe(true);

    FakeNotification.permission = 'denied';
    service.refreshPermission();

    expect(service.permission()).toBe('denied');
    expect(service.enabled()).toBe(false);
    expect(localStorage.getItem('rustnzb-browser-notifications')).toBe('off');
  });

  it('stays enabled when permission is still granted', async () => {
    const { service } = makeService('granted');
    await service.setEnabled(true);

    service.refreshPermission();

    expect(service.enabled()).toBe(true);
  });

  it('records existing history first and then announces only a new outcome', async () => {
    const { service, api, shown } = makeService('granted');
    await service.setEnabled(true);
    // jsdom reports the page as visible; the service only raises alerts
    // while it is not.
    vi.spyOn(document, 'visibilityState', 'get').mockReturnValue('hidden');

    api.get.mockReturnValueOnce(of({ entries: [entry('old', 'completed')] }));
    service.observeStatus(status(1));
    expect(shown).toHaveLength(0);

    api.get.mockReturnValueOnce(
      of({ entries: [entry('new', 'completed'), entry('old', 'completed')] }),
    );
    service.observeStatus(status(0));

    expect(shown.map((notice) => notice.tag)).toEqual(['new']);
    expect(shown[0].title).toBe('Download complete');
  });

  it('announces a failed job with its reason', async () => {
    const { service, api, shown } = makeService('granted');
    await service.setEnabled(true);
    vi.spyOn(document, 'visibilityState', 'get').mockReturnValue('hidden');

    service.observeStatus(status(1));
    api.get.mockReturnValueOnce(
      of({
        entries: [entry('bad', 'failed', { error_message: 'Archive is corrupt' })],
      }),
    );
    service.observeStatus(status(0));

    expect(shown).toHaveLength(1);
    expect(shown[0]).toMatchObject({
      title: 'Download failed',
      body: 'bad.nzb: Archive is corrupt',
    });
  });

  it('does not announce while the tab is in front', async () => {
    const { service, api, shown } = makeService('granted');
    await service.setEnabled(true);
    vi.spyOn(document, 'visibilityState', 'get').mockReturnValue('visible');

    service.observeStatus(status(1));
    api.get.mockReturnValueOnce(of({ entries: [entry('new', 'completed')] }));
    service.observeStatus(status(0));

    expect(shown).toHaveLength(0);
  });

  it('looks for outcomes on a poll that is not a queue shrink', async () => {
    const { service, api } = makeService('granted');
    await service.setEnabled(true);

    service.observeStatus(status(0));
    const callsAfterBaseline = api.get.mock.calls.filter(([path]) => path === '/history').length;

    // Same queue size, no pause: a job finished in the same window another
    // was added, so the shrink signal never fired. The periodic check must.
    vi.useFakeTimers();
    vi.setSystemTime(Date.now() + 61_000);
    service.observeStatus(status(0));

    const calls = api.get.mock.calls.filter(([path]) => path === '/history').length;
    expect(calls).toBe(callsAfterBaseline + 1);
  });

  it('announces a disk-space pause once per hold', async () => {
    const { service, shown } = makeService('granted');
    await service.setEnabled(true);
    vi.spyOn(document, 'visibilityState', 'get').mockReturnValue('hidden');

    service.observeStatus(status(1, 'disk_space'));
    service.observeStatus(status(1, 'disk_space'));

    expect(shown.map((notice) => notice.tag)).toEqual(['disk-space-pause']);
    expect(shown[0].title).toBe('Downloads paused');
  });
});

describe('outcomeNotices', () => {
  it('announces unseen completed and failed rows oldest first', () => {
    // History is newest first.
    const entries = [
      entry('c2', 'completed'),
      entry('f1', 'failed', { error_message: 'Archive is corrupt' }),
      entry('c1', 'completed'),
    ];
    const notices = outcomeNotices(entries, new Set());

    expect(notices.map((n) => n.tag)).toEqual(['c1', 'f1', 'c2']);
    expect(notices[0]).toEqual({ tag: 'c1', title: 'Download complete', body: 'c1.nzb' });
    expect(notices[1]).toEqual({
      tag: 'f1',
      title: 'Download failed',
      body: 'f1.nzb: Archive is corrupt',
    });
  });

  it('does not repeat an outcome on a later check', () => {
    const seen = new Set<string>();
    const entries = [entry('c1', 'completed')];

    expect(outcomeNotices(entries, seen)).toHaveLength(1);
    expect(outcomeNotices(entries, seen)).toHaveLength(0);
    expect(seen.has('c1')).toBe(true);
  });

  it('marks baseline rows as seen without announcing a new one', () => {
    const seen = new Set<string>();
    const notices = outcomeNotices([entry('old', 'completed')], seen);
    expect(notices).toHaveLength(1);

    const next = outcomeNotices([entry('new', 'completed'), entry('old', 'completed')], seen);
    expect(next.map((n) => n.tag)).toEqual(['new']);
  });

  it('ignores rows that are neither completed nor failed', () => {
    const notices = outcomeNotices([entry('x', 'deleted')], new Set());
    expect(notices).toEqual([]);
  });
});
