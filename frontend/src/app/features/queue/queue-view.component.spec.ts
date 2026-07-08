import { of, Subject, throwError } from 'rxjs';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { AddNzbService } from '../../core/services/add-nzb.service';
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

function makeComponent(
  overrides: Partial<ApiStub> = {},
  snackBar?: SnackBarStub,
): {
  component: QueueViewComponent;
  api: ApiStub;
  http: HttpStub;
  snackBar: SnackBarStub;
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
  const component = new QueueViewComponent(
    api as unknown as ApiService,
    http as unknown as import('@angular/common/http').HttpClient,
    snackbarStub as never,
    new AddNzbService(),
  );

  return { component, api, http, snackBar: snackbarStub };
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
    expect(component.eta(makeJob({ downloaded_bytes: 150, total_bytes: 100, speed_bps: 10 }))).toBe('—');
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

  it('reloads queue and shows feedback after a failed row action', () => {
    const { component, snackBar } = makeComponent({
      delete: vi.fn(() => throwError(() => new Error('Delete failed'))),
    });
    const loadQueue = vi.spyOn(component, 'loadQueue').mockImplementation(() => {});

    component.deleteJob('job-1');

    expect(loadQueue).toHaveBeenCalledTimes(1);
    expect(snackBar.open).toHaveBeenCalledWith('Delete failed', 'Close', { duration: 4000 });
  });
});
