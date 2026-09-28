import '@angular/compiler';

import { signal } from '@angular/core';
import { Subject, of } from 'rxjs';
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
  const app = new App(
    api as never,
    auth as never,
    router as never,
    new AddNzbService(),
    {} as never,
    {} as never,
    pauseState,
  );
  return { app, api, pauseState, postResult };
}

describe('App global pause control', () => {
  it('publishes pause immediately and calls the global endpoint', () => {
    const { app, api, pauseState } = makeApp();

    app.togglePause();

    expect(pauseState.paused()).toBe(true);
    expect(api.post).toHaveBeenCalledWith('/queue/pause');
  });

  it('rolls back the shared state if the global request fails', () => {
    const { app, pauseState, postResult } = makeApp();
    vi.spyOn(app, 'pollStatus').mockImplementation(() => {});

    app.togglePause();
    postResult.error(new Error('request failed'));

    expect(pauseState.paused()).toBe(false);
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
});
