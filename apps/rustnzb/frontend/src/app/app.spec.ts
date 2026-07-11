import '@angular/compiler';

import { Subject, of } from 'rxjs';
import { describe, expect, it, vi } from 'vitest';

import { App } from './app';
import { AddNzbService } from './core/services/add-nzb.service';
import { PauseStateService } from './core/services/pause-state.service';

function makeApp(postResult = new Subject<unknown>()) {
  const api = {
    get: vi.fn(() => of({})),
    post: vi.fn(() => postResult.asObservable()),
  };
  const auth = {
    isLoggedIn: vi.fn(() => false),
    logout: vi.fn(() => of({})),
  };
  const router = { url: '/downloads', navigate: vi.fn(() => Promise.resolve(true)) };
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
