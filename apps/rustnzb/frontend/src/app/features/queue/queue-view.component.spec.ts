import '@angular/compiler';

import { HttpErrorResponse } from '@angular/common/http';
import { Observable, of, Subject, throwError } from 'rxjs';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { convertToParamMap, ParamMap } from '@angular/router';

import { AddNzbService } from '../../core/services/add-nzb.service';
import { PauseStateService } from '../../core/services/pause-state.service';
import { ApiService } from '../../core/services/api.service';
import { NzbJob } from '../../core/models/queue.model';
import { QueueViewComponent } from './queue-view.component';

type ApiStub = {
  get: ReturnType<typeof vi.fn>;
  post: ReturnType<typeof vi.fn>;
  put: ReturnType<typeof vi.fn>;
  delete: ReturnType<typeof vi.fn>;
};
type HttpStub = { post: ReturnType<typeof vi.fn> };
type SnackBarStub = { open: ReturnType<typeof vi.fn> };
type ConfirmStub = { confirm: ReturnType<typeof vi.fn> };
type RouteStub = { data: Observable<Record<string, unknown>>; queryParamMap: Observable<ParamMap> };
type RouterStub = { navigate: ReturnType<typeof vi.fn> };

function installLocalStorageMock(): Storage {
  const store = new Map<string, string>();
  const localStorageMock: Storage = {
    get length() {
      return store.size;
    },
    clear: () => store.clear(),
    getItem: (key: string) => store.get(key) ?? null,
    key: (index: number) => Array.from(store.keys())[index] ?? null,
    removeItem: (key: string) => {
      store.delete(key);
    },
    setItem: (key: string, value: string) => {
      store.set(key, String(value));
    },
  };

  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: localStorageMock,
  });

  return localStorageMock;
}

function makeComponent(
  overrides: Partial<ApiStub> = {},
  snackBar?: SnackBarStub,
): {
  component: QueueViewComponent;
  api: ApiStub;
  http: HttpStub;
  snackBar: SnackBarStub;
  route: RouteStub;
  router: RouterStub;
} {
  const api: ApiStub = {
    get: vi.fn(() => of({})),
    post: vi.fn(() => of({})),
    put: vi.fn(() => of({})),
    delete: vi.fn(() => of({})),
    ...overrides,
  };
  const http: HttpStub = {
    post: vi.fn(() => of({})),
  };
  const snackbarStub = snackBar ?? { open: vi.fn() };
  const route: RouteStub = {
    data: of({}),
    queryParamMap: of(convertToParamMap({})),
  };
  const router: RouterStub = {
    navigate: vi.fn(() => Promise.resolve(true)),
  };
  const confirmSvc: ConfirmStub = {
    confirm: vi.fn(() => of(true)),
  };
  const component = new QueueViewComponent(
    api as unknown as ApiService,
    http as unknown as import('@angular/common/http').HttpClient,
    snackbarStub as never,
    new AddNzbService(),
    route as never,
    router as never,
    confirmSvc as never,
    new PauseStateService(),
  );

  return { component, api, http, snackBar: snackbarStub, route, router };
}

function makeJob(overrides: Partial<NzbJob> = {}): NzbJob {
  return {
    id: 'job-1',
    name: 'Job 1',
    category: 'tv',
    status: 'downloading',
    priority: 1,
    total_bytes: 100,
    downloaded_bytes: 50,
    file_count: 1,
    files_completed: 0,
    article_count: 1,
    articles_downloaded: 0,
    articles_failed: 0,
    added_at: '2026-07-08T00:00:00Z',
    completed_at: null,
    speed_bps: 10,
    error_message: null,
    server_stats: [],
    ...overrides,
  };
}

describe('QueueViewComponent', () => {
  beforeEach(() => {
    installLocalStorageMock();
    localStorage.clear();
  });

  afterEach(() => {
    vi.restoreAllMocks();
    localStorage.clear();
  });

  it('clamps percent for invalid values', () => {
    const { component } = makeComponent();

    expect(component.percent({ total_bytes: 0, downloaded_bytes: 50 })).toBe(0);
    expect(component.percent({ total_bytes: -100, downloaded_bytes: 50 })).toBe(0);
    expect(component.percent({ total_bytes: 100, downloaded_bytes: -50 })).toBe(0);
    expect(component.percent({ total_bytes: 100, downloaded_bytes: 250 })).toBe(100);
    expect(component.percent({ total_bytes: Number.NaN, downloaded_bytes: 10 })).toBe(0);
  });

  it('returns em dash for invalid eta inputs', () => {
    const { component } = makeComponent();

    expect(component.eta(makeJob({ speed_bps: 0 }))).toBe('—');
    expect(component.eta(makeJob({ speed_bps: -10 }))).toBe('—');
    expect(component.eta(makeJob({ speed_bps: Number.NaN }))).toBe('—');
    expect(component.eta(makeJob({ downloaded_bytes: 150, total_bytes: 100, speed_bps: 10 }))).toBe(
      '—',
    );
  });

  it('formats the live failed article count', () => {
    const { component } = makeComponent();

    expect(component.failedArticlesLabel(1)).toBe('1 failed article');
    expect(component.failedArticlesLabel(37)).toBe('37 failed articles');
  });

  it('formats queue sizes and speeds beyond TB (BUG-120)', () => {
    const { component } = makeComponent();
    expect(component.formatBytes(3 * 1024 ** 5)).toBe('3.0 PB');
    expect(component.formatSpeed(-1)).toBe('0 B/s');
  });

  it('shows the real speed limit on the download speed card (BUG-121)', () => {
    const { component } = makeComponent();
    const base = { speed_bps: 0, paused: false } as never;
    component.status.set({ ...(base as object), speed_limit_bps: 0 } as never);
    expect(component.speedLimitLabel()).toBe('Active · limit off');
    component.status.set({ ...(base as object), speed_limit_bps: 5 * 1024 * 1024 } as never);
    expect(component.speedLimitLabel()).toBe('Active · limit 5.0 MB/s');
    component.paused.set(true);
    expect(component.speedLimitLabel()).toBe('Paused');
  });

  it('holds a just-finished connection count for five seconds', () => {
    const { component } = makeComponent();
    component.status.set({ nntp_connections: [] } as never);

    component.updateConnectionHold([{ server_id: 'backup', connected: 7 }], 1_000);
    component.status.set({ nntp_connections: [{ server_id: 'backup', connected: 0, limit: 10 }] } as never);

    expect(component.displayConnectionCount('backup', 5_999)).toEqual({ count: 7, recent: true });
    expect(component.displayConnectionCount('backup', 6_000)).toEqual({ count: 0, recent: false });
  });

  it('prefers the live connection count over a held value', () => {
    const { component } = makeComponent();
    component.updateConnectionHold([{ server_id: 'backup', connected: 7 }], 1_000);
    component.status.set({ nntp_connections: [{ server_id: 'backup', connected: 3, limit: 10 }] } as never);

    expect(component.displayConnectionCount('backup', 2_000)).toEqual({ count: 3, recent: false });
  });

  it('labels a job held by the disk-space guard', () => {
    const { component } = makeComponent();

    expect(component.statusLabel(makeJob({ status: 'paused', pause_reason: 'disk_space' }))).toBe(
      'paused · low disk',
    );
    expect(component.statusLabel(makeJob({ status: 'paused', pause_reason: 'manual' }))).toBe(
      'paused',
    );
    expect(component.statusLabel(makeJob({ status: 'downloading' }))).toBe('downloading');
  });

  it('shows the disk-space reason for a global pause applied by the guard', () => {
    const { component } = makeComponent({
      get: vi.fn((url: string) =>
        url === '/queue'
          ? of({ jobs: [], total: 0, speed_bps: 0, paused: true, pause_reason: 'disk_space' })
          : of({}),
      ),
    });

    component.loadQueue();

    expect(component.pausedLabel()).toBe('Paused · low disk space');
    // A queued job caught by the global hold before the next refresh.
    expect(component.statusLabel(makeJob({ status: 'queued' }))).toBe('paused · low disk');
  });

  it('keeps the plain paused label for a user pause', () => {
    const { component } = makeComponent({
      get: vi.fn((url: string) =>
        url === '/queue'
          ? of({ jobs: [], total: 0, speed_bps: 0, paused: true, pause_reason: 'global' })
          : of({}),
      ),
    });

    component.loadQueue();

    expect(component.pausedLabel()).toBe('Paused');
  });

  it('ignores a duplicate row action while the first request is pending', () => {
    const action$ = new Subject<unknown>();
    const { component, api } = makeComponent({
      post: vi.fn(() => action$.asObservable()),
    });
    vi.spyOn(component, 'loadQueue').mockImplementation(() => {});

    component.pauseJob('job-1');
    component.pauseJob('job-1');

    expect(api.post).toHaveBeenCalledTimes(1);
    expect(component.isActionPending('job-1')).toBe(true);

    action$.next({});
    action$.complete();

    expect(component.isActionPending('job-1')).toBe(false);
  });

  it('reloads queue after a successful row action', () => {
    const { component, snackBar } = makeComponent({
      post: vi.fn(() => of({ ok: true })),
    });
    const loadQueue = vi.spyOn(component, 'loadQueue').mockImplementation(() => {});

    component.pauseJob('job-1');

    expect(loadQueue).toHaveBeenCalledTimes(1);
    expect(snackBar.open).toHaveBeenCalledWith('Job paused', 'Close', { duration: 2500 });
  });

  describe('queue poll sequencing (BUG-126)', () => {
    /** /queue responses are held until the test flushes them, in any order. */
    function controlledQueue(overrides: Partial<ApiStub> = {}) {
      const pending: Subject<unknown>[] = [];
      const ctx = makeComponent({
        get: vi.fn((path: string) => {
          if (path !== '/queue') return of({});
          const response = new Subject<unknown>();
          pending.push(response);
          return response.asObservable();
        }),
        ...overrides,
      });
      const respond = (index: number, jobs: NzbJob[]) => {
        pending[index].next({ jobs, total: jobs.length, speed_bps: 0, paused: false });
        pending[index].complete();
      };
      return { ...ctx, pending, respond };
    }
    const ids = (component: QueueViewComponent) => component.jobs().map((job) => job.id);

    it('drops a poll that started before a delete finished', () => {
      const { component, respond } = controlledQueue();
      const [one, two] = [makeJob({ id: 'job-1' }), makeJob({ id: 'job-2' })];
      component.jobs.set([one, two]);

      component.loadQueue(); // #0: poll in flight while the delete runs
      component.deleteJob(one); // delete succeeds → reload #1
      respond(1, [two]);
      respond(0, [one, two]); // stale: must not resurrect job-1

      expect(ids(component)).toEqual(['job-2']);
    });

    it('keeps the optimistic order while a reorder is in flight and after it lands', () => {
      const move = new Subject<unknown>();
      const { component, respond } = controlledQueue({ post: vi.fn(() => move.asObservable()) });
      const jobs = ['job-1', 'job-2', 'job-3'].map((id) => makeJob({ id }));
      component.jobs.set(jobs);
      component.draggingJobId.set('job-1');
      component.dropAfterTarget.set(true);

      component.loadQueue(); // #0 started before the drop
      component.onRowDrop({ preventDefault: vi.fn() } as unknown as DragEvent, 'job-2');
      component.loadQueue(); // #1 started while the move is pending
      respond(0, jobs);
      respond(1, jobs);
      expect(ids(component)).toEqual(['job-2', 'job-1', 'job-3']);

      move.next({});
      move.complete(); // → reload #2
      respond(2, [jobs[1], jobs[0], jobs[2]]);
      expect(ids(component)).toEqual(['job-2', 'job-1', 'job-3']);
    });

    it('ignores a poll response that arrives after a newer one', () => {
      const { component, respond } = controlledQueue();
      component.loadQueue();
      component.loadQueue();
      respond(1, [makeJob({ id: 'new' })]);
      respond(0, [makeJob({ id: 'old' })]);
      expect(ids(component)).toEqual(['new']);
    });
  });

  it('shows active download states as paused while global pause is active', () => {
    const { component } = makeComponent();
    component.paused.set(true);

    expect(component.effectiveStatus('downloading')).toBe('paused');
    expect(component.effectiveStatus('queued')).toBe('paused');
    expect(component.effectiveStatus('extracting')).toBe('extracting');
  });

  it('filters globally stopped queued jobs as paused', () => {
    const { component } = makeComponent();
    component.jobs.set([makeJob({ status: 'queued' })]);
    component.paused.set(true);

    component.filterStatus = 'paused';
    expect(component.filteredJobs().map((job) => job.id)).toEqual(['job-1']);
    component.filterStatus = 'queued';
    expect(component.filteredJobs()).toEqual([]);
  });

  it('never sends an individual resume while global pause is active', () => {
    const { component, api } = makeComponent();
    component.paused.set(true);

    component.resumeJob('job-1');

    expect(api.post).not.toHaveBeenCalled();
  });

  it('reloads queue and shows feedback after a failed row action', () => {
    const { component, snackBar } = makeComponent({
      delete: vi.fn(() =>
        throwError(
          () =>
            new HttpErrorResponse({
              status: 409,
              error: { error_kind: 'conflict', human_readable: 'Job is post-processing', status: 409 },
            }),
        ),
      ),
    });
    const loadQueue = vi.spyOn(component, 'loadQueue').mockImplementation(() => {});

    component.deleteJob(makeJob({ id: 'job-1' }));

    expect(loadQueue).toHaveBeenCalledTimes(1);
    expect(snackBar.open).toHaveBeenCalledWith(
      'Failed to remove job: Job is post-processing',
      'Close',
      { duration: 5000 },
    );
  });

  it('reorders jobs before the target row', () => {
    const { component } = makeComponent();
    const jobs = [
      makeJob({ id: 'job-1', name: 'One' }),
      makeJob({ id: 'job-2', name: 'Two' }),
      makeJob({ id: 'job-3', name: 'Three' }),
    ];

    expect(
      component.buildReorderedJobs(jobs, 'job-3', 'job-1', false)?.map((job) => job.id),
    ).toEqual(['job-3', 'job-1', 'job-2']);
  });

  it('reorders jobs after the target row', () => {
    const { component } = makeComponent();
    const jobs = [
      makeJob({ id: 'job-1', name: 'One' }),
      makeJob({ id: 'job-2', name: 'Two' }),
      makeJob({ id: 'job-3', name: 'Three' }),
    ];

    expect(
      component.buildReorderedJobs(jobs, 'job-1', 'job-3', true)?.map((job) => job.id),
    ).toEqual(['job-2', 'job-3', 'job-1']);
  });

  it('optimistically reorders rows and persists the new position', () => {
    const { component, api } = makeComponent({
      post: vi.fn(() => of({ ok: true })),
    });
    component.jobs.set([
      makeJob({ id: 'job-1', name: 'One' }),
      makeJob({ id: 'job-2', name: 'Two' }),
      makeJob({ id: 'job-3', name: 'Three' }),
    ]);
    component.draggingJobId.set('job-1');
    component.dropAfterTarget.set(true);
    const loadQueue = vi.spyOn(component, 'loadQueue').mockImplementation(() => {});

    component.onRowDrop({ preventDefault: vi.fn() } as unknown as DragEvent, 'job-2');

    expect(component.jobs().map((job) => job.id)).toEqual(['job-2', 'job-1', 'job-3']);
    expect(api.post).toHaveBeenCalledWith('/queue/job-1/move', { position: 1 });
    expect(loadQueue).toHaveBeenCalledTimes(1);
  });

  it('canonicalizes the legacy history route and expands history', () => {
    const { component, route, router } = makeComponent();
    route.data = of({ legacyTab: 'history' });
    component.historyCollapsed.set(true);
    vi.spyOn(component as unknown as { loadAll(): void }, 'loadAll').mockImplementation(() => {});

    component.ngOnInit();

    expect(router.navigate).toHaveBeenCalledWith(['/downloads'], {
      replaceUrl: true,
    });
    expect(component.historyCollapsed()).toBe(false);
    component.ngOnDestroy();
  });

  it('persists history panel collapse state', () => {
    const { component } = makeComponent();

    component.toggleHistory();
    expect(component.historyCollapsed()).toBe(true);
    expect(localStorage.getItem(component.HISTORY_KEY)).toBe('true');

    component.toggleHistory();
    expect(component.historyCollapsed()).toBe(false);
    expect(localStorage.getItem(component.HISTORY_KEY)).toBe('false');
  });

  it('reports bulk action failures once with the server message', () => {
    const notFound = new HttpErrorResponse({
      status: 404,
      error: { error_kind: 'job_not_found', human_readable: 'Job not found: job-2', status: 404 },
    });
    const { component, snackBar } = makeComponent({
      post: vi.fn((path: string) => (path.includes('job-2') ? throwError(() => notFound) : of({}))),
    });
    const loadQueue = vi.spyOn(component, 'loadQueue').mockImplementation(() => {});
    component.selectedIds.set(new Set(['job-1', 'job-2']));

    component.bulkPause();

    expect(snackBar.open).toHaveBeenCalledTimes(1);
    expect(snackBar.open).toHaveBeenCalledWith(
      'Failed to pause 1 of 2 job(s): Job not found: job-2',
      'Close',
      { duration: 5000 },
    );
    expect(loadQueue).toHaveBeenCalledTimes(1);
    expect(component.selectedIds().size).toBe(0);
  });

  it('does not toast a 401 that the auth interceptor is handling', () => {
    const { component, snackBar } = makeComponent({
      post: vi.fn(() => throwError(() => new HttpErrorResponse({ status: 401 }))),
    });
    vi.spyOn(component, 'loadQueue').mockImplementation(() => {});

    component.pauseJob('job-1');

    expect(snackBar.open).not.toHaveBeenCalled();
  });
});
