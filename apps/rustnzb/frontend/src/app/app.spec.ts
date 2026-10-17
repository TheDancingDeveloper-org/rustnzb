import '@angular/compiler';

import { signal } from '@angular/core';
import { Subject, of, throwError } from 'rxjs';
import { describe, expect, it, vi } from 'vitest';

import { App, isBareRoute, isDemoPath } from './app';
import { AddNzbService } from './core/services/add-nzb.service';
import { PauseStateService } from './core/services/pause-state.service';

function makeApp(postResult = new Subject<unknown>()) {
  const api = {
    get: vi.fn(() => of({})),
    post: vi.fn(() => postResult.asObservable()),
  };
  const auth = {
    authenticated: signal(false),
    logout: vi.fn(() => of({})),
  };
  const router = {
    url: '/downloads',
    events: new Subject<unknown>(),
    navigate: vi.fn(() => Promise.resolve(true)),
  };
  const pauseState = new PauseStateService();
  const snack = { open: vi.fn() };
  const app = new App(
    api as never,
    auth as never,
    router as never,
    new AddNzbService(),
    {} as never,
    {} as never,
    pauseState,
    snack as never,
    { observeStatus: vi.fn() } as never,
  );
  return { app, api, auth, pauseState, postResult, snack };
}

describe('App global pause control', () => {
  it('publishes pause immediately and calls the global endpoint', () => {
    const { app, api, pauseState } = makeApp();

    app.togglePause();

    expect(pauseState.paused()).toBe(true);
    expect(api.post).toHaveBeenCalledWith('/queue/pause');
  });

  it('rolls back the shared state if the global request fails', () => {
    const { app, pauseState, postResult, snack } = makeApp();
    vi.spyOn(app, 'pollStatus').mockImplementation(() => {});

    app.togglePause();
    postResult.error(new Error('request failed'));

    expect(pauseState.paused()).toBe(false);
    expect(snack.open).toHaveBeenCalledWith('Failed to pause downloads', 'Close', {
      duration: 5000,
    });
  });
});

describe('status pill loading and offline states', () => {
  const status = {
    speed_bps: 1024,
    paused: false,
    queue_size: 3,
    disk_space_free: 2048,
    webdav_enabled: false,
  };

  it('shows dashes and no data before the first successful poll', () => {
    const { app } = makeApp();

    expect(app.loaded()).toBe(false);
    expect(app.offline()).toBe(false);
  });

  it('marks the status loaded after a successful poll', () => {
    const { app, api, auth } = makeApp();
    auth.authenticated.set(true);
    api.get.mockReturnValue(of(status));

    app.pollStatus();

    expect(app.loaded()).toBe(true);
    expect(app.offline()).toBe(false);
    expect(app.queueCount()).toBe(3);
  });

  it('goes offline when the poll fails and keeps the last known values', () => {
    const { app, api, auth } = makeApp();
    auth.authenticated.set(true);
    api.get.mockReturnValueOnce(of(status));
    app.pollStatus();

    api.get.mockReturnValueOnce(throwError(() => new Error('down')));
    app.pollStatus();

    expect(app.offline()).toBe(true);
    expect(app.loaded()).toBe(true);
    expect(app.queueCount()).toBe(3);
  });

  it('recovers from offline on the next successful poll', () => {
    const { app, api, auth } = makeApp();
    auth.authenticated.set(true);
    api.get.mockReturnValueOnce(throwError(() => new Error('down')));
    app.pollStatus();
    expect(app.offline()).toBe(true);

    api.get.mockReturnValueOnce(of(status));
    app.pollStatus();

    expect(app.offline()).toBe(false);
    expect(app.loaded()).toBe(true);
  });
});

describe('demo path detection', () => {
  it('recognizes the demo root and nested demo routes', () => {
    expect(isDemoPath('/demo')).toBe(true);
    expect(isDemoPath('/demo/')).toBe(true);
    expect(isDemoPath('/demo/statistics')).toBe(true);
  });

  it('does not show demo chrome in a normal installation', () => {
    expect(isDemoPath('/')).toBe(false);
    expect(isDemoPath('/downloads')).toBe(false);
    expect(isDemoPath('/demonstration')).toBe(false);
  });
});

describe('bare route detection', () => {
  it('keeps login and welcome full-screen', () => {
    expect(isBareRoute('/login')).toBe(true);
    expect(isBareRoute('/welcome')).toBe(true);
    expect(isBareRoute('/login?returnUrl=%2Fsettings')).toBe(true);
  });

  it('shows chrome on application pages', () => {
    expect(isBareRoute('/downloads')).toBe(false);
    expect(isBareRoute('/settings')).toBe(false);
    expect(isBareRoute('/welcomes')).toBe(false);
  });

  it('formats sizes and speeds beyond TB without "undefined" (BUG-120)', () => {
    const { app } = makeApp();
    expect(app.formatBytes(1.1 * 1024 ** 5)).toBe('1.1 PB');
    expect(app.formatSpeed(2 * 1024 ** 4)).toBe('2.0 TB/s');
  });
});
