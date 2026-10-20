import '@angular/compiler';

import { signal } from '@angular/core';
import { Subject, of, throwError } from 'rxjs';
import { describe, expect, it, vi } from 'vitest';

import { App, MENU_WIDTH, documentTitle, isBareRoute, isDemoPath } from './app';
import { AddNzbService } from './core/services/add-nzb.service';
import { PauseStateService } from './core/services/pause-state.service';

function makeApp(postResult = new Subject<unknown>()) {
  const api = {
    get: vi.fn(() => of({})),
    post: vi.fn(() => postResult.asObservable()),
    put: vi.fn(() => of({})),
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
  const addNzb = new AddNzbService();
  const app = new App(
    api as never,
    auth as never,
    router as never,
    addNzb,
    {} as never,
    {} as never,
    pauseState,
    snack as never,
    { observeStatus: vi.fn() } as never,
  );
  return { app, api, auth, pauseState, postResult, snack, router, addNzb };
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

describe('browser tab title', () => {
  const live = { paused: false, speedBps: 12.3 * 1024 * 1024, queueSize: 4 };

  it('shows live speed and queue depth on the downloads page', () => {
    expect(documentTitle('/downloads', live)).toBe('↓ 12.3 MB/s · 4 left – rustnzb');
  });

  it('shows Paused in place of speed while paused', () => {
    expect(documentTitle('/downloads', { ...live, paused: true })).toBe('Paused · 4 left – rustnzb');
  });

  it('names the page after the live state, except on the home page', () => {
    expect(documentTitle('/statistics', live)).toBe('↓ 12.3 MB/s · 4 left · Statistics – rustnzb');
    expect(documentTitle('/statistics', null)).toBe('Statistics – rustnzb');
  });

  it('falls back to the app name when idle or when live state is unknown', () => {
    expect(documentTitle('/downloads', { paused: false, speedBps: 0, queueSize: 0 })).toBe('rustnzb');
    expect(documentTitle('/downloads', null)).toBe('rustnzb');
  });

  it('keeps the legacy queue and history paths on their old tab names', () => {
    expect(documentTitle('/queue', null)).toBe('rustnzb');
    expect(documentTitle('/history?x=1', null)).toBe('History – rustnzb');
  });

  it('is updated from the status poll and drops live values when offline', () => {
    const { app, api, auth } = makeApp();
    auth.authenticated.set(true);
    api.get.mockReturnValueOnce(of({ speed_bps: 2048, paused: false, queue_size: 2, disk_space_free: 0, webdav_enabled: false }));
    app.pollStatus();
    expect(document.title).toBe('↓ 2.0 KB/s · 2 left – rustnzb');

    api.get.mockReturnValueOnce(throwError(() => new Error('down')));
    app.pollStatus();
    expect(document.title).toBe('rustnzb');
  });
});

describe('quick speed limit', () => {
  it('sends the chosen limit and marks it as the current one', () => {
    const { app, api, snack } = makeApp();
    app.speedMenuOpen = true;

    app.setSpeedLimit(5 * 1024 * 1024);

    expect(api.put).toHaveBeenCalledWith('/config/speed-limit', { speed_limit_bps: 5 * 1024 * 1024 });
    expect(app.speedLimit()).toBe(5 * 1024 * 1024);
    expect(app.speedMenuOpen).toBe(false);
    expect(snack.open).toHaveBeenCalledWith('Speed limit set to 5.0 MB/s', 'Close', { duration: 2000 });
  });

  it('converts a custom MB/s value and ignores empty input', () => {
    const { app, api } = makeApp();

    app.customSpeedMb = null;
    app.setCustomSpeedLimit();
    expect(api.put).not.toHaveBeenCalled();

    app.customSpeedMb = 2.5;
    app.setCustomSpeedLimit();
    expect(api.put).toHaveBeenCalledWith('/config/speed-limit', { speed_limit_bps: 2.5 * 1024 * 1024 });
    expect(app.customSpeedMb).toBeNull();
  });

  it('reports a failed save and leaves the limit unchanged', () => {
    const { app, api, snack } = makeApp();
    api.put.mockReturnValueOnce(throwError(() => new Error('nope')));

    app.setSpeedLimit(1024);

    expect(app.speedLimit()).toBe(0);
    expect(snack.open).toHaveBeenCalledWith('Failed to set speed limit', 'Close', expect.anything());
  });
});

describe('first-run server banner', () => {
  it('counts servers only once the list has loaded', () => {
    const { app, api, auth } = makeApp();
    auth.authenticated.set(true);
    expect(app.noServers()).toBe(false);

    api.get.mockReturnValueOnce(of([]));
    app.loadServerCount();
    expect(app.noServers()).toBe(true);

    api.get.mockReturnValueOnce(of([{ id: 'a' }]));
    app.loadServerCount();
    expect(app.noServers()).toBe(false);
  });

  it('shows no banner when the server check fails', () => {
    const { app, api, auth } = makeApp();
    auth.authenticated.set(true);
    api.get.mockReturnValueOnce(throwError(() => new Error('down')));

    app.loadServerCount();

    expect(app.serverCount()).toBeNull();
    expect(app.noServers()).toBe(false);
  });
});

describe('header menus', () => {
  const trigger = (rect: { left: number; right: number; bottom: number }) =>
    ({ getBoundingClientRect: () => ({ ...rect, top: 0, width: 0, height: 0, x: 0, y: 0, toJSON: () => ({}) }) }) as unknown as HTMLElement;

  it('anchors the speed menu just below its trigger and opens it', () => {
    const { app } = makeApp();

    app.toggleSpeedMenu(trigger({ left: 300, right: 360, bottom: 40 }));

    expect(app.speedMenuOpen).toBe(true);
    expect(app.menuAnchor.top).toBe(44);
    expect(app.menuAnchor.left).toBe(300);
  });

  it('keeps the speed menu inside the viewport on narrow screens', () => {
    const { app } = makeApp();
    Object.defineProperty(window, 'innerWidth', { configurable: true, value: 390 });

    try {
      app.toggleSpeedMenu(trigger({ left: 380, right: 420, bottom: 40 }));
      // The whole menu, not just its anchor, must sit inside the viewport.
      const rightEdge = app.menuAnchor.left + MENU_WIDTH;
      expect(app.menuAnchor.left).toBeGreaterThanOrEqual(8);
      expect(rightEdge).toBeLessThanOrEqual(390 - 8);
    } finally {
      Object.defineProperty(window, 'innerWidth', { configurable: true, value: 1024 });
    }
  });

  it('anchors the pause menu from the right edge and closes the other menu', () => {
    const { app } = makeApp();
    app.speedMenuOpen = true;

    app.togglePauseMenu(trigger({ left: 900, right: 940, bottom: 40 }));

    expect(app.pauseMenuOpen).toBe(true);
    expect(app.speedMenuOpen).toBe(false);
    expect(app.menuAnchor.right).toBe(window.innerWidth - 940);
  });

  it('closes both menus on window resize', () => {
    const { app } = makeApp();
    app.togglePauseMenu(trigger({ left: 900, right: 940, bottom: 40 }));

    app.onWindowResize();

    expect(app.pauseMenuOpen).toBe(false);
    expect(app.speedMenuOpen).toBe(false);
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

function fileDrag(type: string, files: File[] = [], defaultPrevented = false) {
  return {
    dataTransfer: { types: ['Files'], files, dropEffect: 'none' },
    defaultPrevented,
    preventDefault: vi.fn(),
    target: document.body,
  } as unknown as DragEvent & { preventDefault: ReturnType<typeof vi.fn>; type: string };
}

describe('drop .nzb files anywhere', () => {
  function signedIn() {
    const ctx = makeApp();
    ctx.auth.authenticated.set(true);
    const prefills: unknown[] = [];
    ctx.addNzb.prefill$.subscribe((p) => prefills.push(p));
    return { ...ctx, prefills };
  }

  it('shows the overlay while files are dragged over the window, through child elements', () => {
    const { app } = signedIn();
    app.onWindowDragEnter(fileDrag('dragenter'));
    app.onWindowDragEnter(fileDrag('dragenter'));
    app.onWindowDragLeave(fileDrag('dragleave'));
    expect(app.fileDragActive()).toBe(true);
    app.onWindowDragLeave(fileDrag('dragleave'));
    expect(app.fileDragActive()).toBe(false);
  });

  it('ignores drags that carry no files (queue row reordering)', () => {
    const { app } = signedIn();
    const e = { dataTransfer: { types: ['text/plain'] }, preventDefault: vi.fn() } as never;
    app.onWindowDragEnter(e);
    app.onWindowDragOver(e);
    expect(app.fileDragActive()).toBe(false);
    expect(
      (e as { preventDefault: ReturnType<typeof vi.fn> }).preventDefault,
    ).not.toHaveBeenCalled();
  });

  it('hands dropped .nzb files to the Add NZB panel', () => {
    const { app, prefills } = signedIn();
    const nzb = new File(['x'], 'a.nzb');
    app.onWindowDragEnter(fileDrag('dragenter'));
    const drop = fileDrag('drop', [nzb, new File(['y'], 'notes.txt')]);
    app.onWindowDrop(drop);
    expect(drop.preventDefault).toHaveBeenCalled();
    expect(app.fileDragActive()).toBe(false);
    expect(prefills).toEqual([{ files: [nzb] }]);
  });

  it('navigates to downloads first when elsewhere', async () => {
    const { app, router, prefills } = signedIn();
    router.url = '/settings';
    app.onWindowDrop(fileDrag('drop', [new File(['x'], 'a.nzb')]));
    expect(router.navigate).toHaveBeenCalledWith(['/downloads']);
    await Promise.resolve();
    expect(prefills).toHaveLength(1);
  });

  it('leaves drops the panel dropzone already handled', () => {
    const { app, prefills } = signedIn();
    app.onWindowDrop(fileDrag('drop', [new File(['x'], 'a.nzb')], true));
    expect(prefills).toEqual([]);
  });

  it('tells the user when nothing dropped is an NZB', () => {
    const { app, prefills, snack } = signedIn();
    app.onWindowDrop(fileDrag('drop', [new File(['x'], 'movie.mkv')]));
    expect(prefills).toEqual([]);
    expect(snack.open).toHaveBeenCalled();
  });

  it('does nothing when signed out', () => {
    const { app, auth, prefills } = signedIn();
    auth.authenticated.set(false);
    app.onWindowDragEnter(fileDrag('dragenter'));
    app.onWindowDrop(fileDrag('drop', [new File(['x'], 'a.nzb')]));
    expect(app.fileDragActive()).toBe(false);
    expect(prefills).toEqual([]);
  });
});

describe('paste a URL anywhere', () => {
  function paste(text: string, target: EventTarget = document.body) {
    return {
      clipboardData: { getData: () => text },
      target,
      preventDefault: vi.fn(),
    } as unknown as ClipboardEvent & { preventDefault: ReturnType<typeof vi.fn> };
  }
  function signedIn() {
    const ctx = makeApp();
    ctx.auth.authenticated.set(true);
    const prefills: unknown[] = [];
    ctx.addNzb.prefill$.subscribe((p) => prefills.push(p));
    return { ...ctx, prefills };
  }

  it('opens the panel with a pasted URL', () => {
    const { app, prefills } = signedIn();
    const e = paste(' https://idx.example/get/1 ');
    app.onWindowPaste(e);
    expect(e.preventDefault).toHaveBeenCalled();
    expect(prefills).toEqual([{ url: 'https://idx.example/get/1' }]);
  });

  it('leaves pastes into form fields and non-URL text alone', () => {
    const { app, prefills } = signedIn();
    app.onWindowPaste(paste('https://idx.example/get/1', document.createElement('input')));
    app.onWindowPaste(paste('just some words'));
    expect(prefills).toEqual([]);
  });
});
