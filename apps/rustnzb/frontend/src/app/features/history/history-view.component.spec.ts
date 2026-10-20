import '@angular/compiler';

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { firstValueFrom, of, Subject, throwError } from 'rxjs';

import { ApiService, POLL_TIMEOUT_MS } from '../../core/services/api.service';
import { HistoryEntry } from '../../core/models/queue.model';
import { HistoryViewComponent } from './history-view.component';
import { AnnouncerService } from '../../core/services/announcer.service';

function entry(overrides: Partial<HistoryEntry> = {}): HistoryEntry {
  return {
    id: 'history-1',
    name: 'Release.One',
    category: 'movies',
    status: 'completed',
    total_bytes: 1024,
    downloaded_bytes: 1024,
    added_at: '2026-07-09T10:00:00Z',
    completed_at: '2026-07-09T10:01:30Z',
    output_dir: '/downloads/Release.One',
    stages: [],
    error_message: null,
    server_stats: [],
    has_nzb_data: true,
    ...overrides,
  };
}

describe('HistoryViewComponent', () => {
  let api: { get: ReturnType<typeof vi.fn>; post: ReturnType<typeof vi.fn>; delete: ReturnType<typeof vi.fn> };
  let snack: { open: ReturnType<typeof vi.fn> };
  let confirm: { confirm: ReturnType<typeof vi.fn> };
  let announcer: AnnouncerService;
  let component: HistoryViewComponent;

  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-07-10T00:00:00Z'));
    api = {
      get: vi.fn((path: string) =>
        of(path === '/status' ? { webdav_enabled: true } : { entries: [] }),
      ),
      post: vi.fn(() => of({})),
      delete: vi.fn(() => of({})),
    };
    snack = { open: vi.fn() };
    confirm = { confirm: vi.fn(() => of(true)) };
    announcer = new AnnouncerService();
    component = new HistoryViewComponent(
      api as unknown as ApiService,
      snack as never,
      confirm as never,
      announcer,
    );
  });

  afterEach(() => vi.useRealTimers());

  it('loads history and WebDAV capability during initialization', () => {
    api.get.mockImplementation((path: string) =>
      of(path === '/status' ? { webdav_enabled: true } : { entries: [entry()] }),
    );
    component.ngOnInit();
    expect(component.entries()).toHaveLength(1);
    expect(component.webdavEnabled()).toBe(true);
    expect(component.loading()).toBe(false);
    component.ngOnDestroy();
  });

  it('skips background polls while the tab is hidden', () => {
    component.ngOnInit();
    const hidden = vi.spyOn(document, 'hidden', 'get').mockReturnValue(true);
    vi.advanceTimersByTime(15000);
    expect(historyCalls()).toHaveLength(1);
    hidden.mockReturnValue(false);
    vi.advanceTimersByTime(5000);
    expect(historyCalls()).toHaveLength(2);
    hidden.mockRestore();
    component.ngOnDestroy();
  });

  it('does not stack polls behind a load that is still running', () => {
    const pending = new Subject<unknown>();
    api.get.mockImplementation((path: string) =>
      path === '/status' ? of({}) : pending,
    );
    component.ngOnInit();
    vi.advanceTimersByTime(10000);
    expect(historyCalls()).toHaveLength(1);
    pending.next({ entries: [] });
    vi.advanceTimersByTime(5000);
    expect(historyCalls()).toHaveLength(2);
    component.ngOnDestroy();
  });

  it('gives up on a load that never answers so polling resumes', () => {
    const never = new Subject<unknown>();
    api.get.mockImplementation((path: string) =>
      path === '/status' ? of({}) : historyCalls().length === 1 ? never : of({ entries: [] }),
    );
    component.ngOnInit();
    vi.advanceTimersByTime(POLL_TIMEOUT_MS - 1);
    expect(historyCalls()).toHaveLength(1);
    vi.advanceTimersByTime(5000);
    expect(historyCalls()).toHaveLength(2);
    component.ngOnDestroy();
  });

  function historyCalls() {
    return api.get.mock.calls.filter(([path]) => path === '/history');
  }

  it('requests one page with the default 7-day window', () => {
    component.load();
    expect(api.get).toHaveBeenCalledWith('/history', { offset: '0', limit: '50', days: '7' });
  });

  it('sends name, status, category, and time filters to the server and resets to page one', () => {
    component.offset.set(100);
    component.nameFilter = ' failed ';
    component.filterStatus = 'failed';
    component.filterCategory = 'tv';
    component.filterTime = 'all';
    component.onFiltersChanged();
    expect(component.offset()).toBe(0);
    expect(historyCalls().at(-1)?.[1]).toEqual({
      offset: '0',
      limit: '50',
      status: 'failed',
      category: 'tv',
      search: 'failed',
    });
  });

  it('pages through the full history using the server total', () => {
    api.get.mockImplementation((path: string) =>
      of(path === '/history' ? { entries: [entry()], total: 120, offset: 0, limit: 50 } : {}),
    );
    component.load();
    expect(component.total()).toBe(120);
    expect(component.rangeLabel()).toBe('1–50 of 120');
    expect(component.hasPrev()).toBe(false);
    expect(component.hasNext()).toBe(true);

    component.nextPage();
    expect(component.offset()).toBe(50);
    expect(historyCalls().at(-1)?.[1]).toMatchObject({ offset: '50', limit: '50' });
    component.nextPage();
    expect(component.offset()).toBe(100);
    expect(component.rangeLabel()).toBe('101–120 of 120');
    expect(component.hasNext()).toBe(false);
    component.nextPage();
    expect(component.offset()).toBe(100);

    component.prevPage();
    expect(component.offset()).toBe(50);
  });

  it('steps back to the last page when the current page disappears', () => {
    api.get.mockImplementation((path: string, params?: Record<string, string>) =>
      of(path === '/history'
        ? { entries: params?.['offset'] === '50' ? [entry()] : [], total: 51 }
        : {}),
    );
    component.offset.set(100);
    component.load();
    expect(component.offset()).toBe(50);
    expect(component.entries()).toHaveLength(1);
  });

  it('takes category options from the server rather than the visible page', () => {
    api.get.mockImplementation((path: string) =>
      of(path === '/history'
        ? { entries: [entry({ category: 'tv' })], total: 1, categories: ['movies', 'tv'] }
        : {}),
    );
    component.load();
    expect(component.categoryOptions()).toEqual(['movies', 'tv']);
  });

  it('shows server-computed statistics for the whole window', () => {
    api.get.mockImplementation((path: string) =>
      of(path === '/history'
        ? {
            entries: [entry()],
            total: 300,
            stats: {
              completed: 200,
              completed_bytes: 4096,
              failed: 100,
              success_pct: 67,
              avg_duration_secs: 90,
              fail_reasons: [{ reason: 'CRC mismatch', count: 60 }, { reason: 'Aborted', count: 40 }],
            },
          }
        : {}),
    );
    component.load();
    expect(component.statCards()).toMatchObject({
      completed: 200,
      completedBytes: 4096,
      failed: 100,
      failReasons: '60 CRC mismatch · 40 Aborted',
      successPct: 67,
      avgDurationLabel: '1m 30s',
    });
  });

  it('tolerates a server response without totals or stats', () => {
    api.get.mockImplementation((path: string) =>
      of(path === '/history' ? { entries: [entry(), entry({ id: '2', category: 'tv' })] } : {}),
    );
    component.load();
    expect(component.total()).toBe(2);
    expect(component.categoryOptions()).toEqual(['movies', 'tv']);
    expect(component.statCards()).toMatchObject({ completed: 0, failed: 0, failReasons: 'none' });
  });

  it('exports every matching entry, not just the visible page', async () => {
    const all = Array.from({ length: 450 }, (_, i) => entry({ id: `e${i}`, name: `Release.${i}` }));
    api.get.mockImplementation((path: string, params?: Record<string, string>) => {
      const offset = Number(params?.['offset'] ?? 0);
      const limit = Number(params?.['limit'] ?? 50);
      return of(path === '/history'
        ? { entries: all.slice(offset, offset + limit), total: all.length }
        : {});
    });
    component.filterStatus = 'completed';
    const exported = await firstValueFrom(component.fetchAllMatching());
    expect(exported).toHaveLength(450);
    expect(historyCalls().every(([, params]) => params.status === 'completed')).toBe(true);
    expect(historyCalls().length).toBeLessThanOrEqual(3);
  });

  it('retries, removes, and queues media through the expected routes', () => {
    const load = vi.spyOn(component, 'load').mockImplementation(() => {});
    component.retry('id one');
    component.remove('id two');
    component.addToMedia('id three');
    expect(api.post).toHaveBeenCalledWith('/history/id one/retry');
    expect(api.delete).toHaveBeenCalledWith('/history/id two');
    expect(api.post).toHaveBeenCalledWith('/dav/add?id=id three');
    expect(load).toHaveBeenCalledTimes(2);
  });

  it('requires confirmation before clearing all history', () => {
    const load = vi.spyOn(component, 'load').mockImplementation(() => {});
    component.clearAll();
    expect(confirm.confirm).toHaveBeenCalledWith(expect.objectContaining({ danger: true }));
    expect(api.delete).toHaveBeenCalledWith('/history');
    expect(load).toHaveBeenCalledTimes(1);
  });

  it('formats byte, duration, and relative-time boundaries', () => {
    expect(component.formatBytes(0)).toBe('0 B');
    expect(component.formatBytes(1536)).toBe('1.5 KB');
    expect(component.formatDuration('2026-07-09T10:00:00Z', '2026-07-09T10:01:30Z')).toBe(
      '1m 30s',
    );
    expect(component.relativeTime('2026-07-09T23:59:30Z')).toBe('just now');
    expect(component.relativeTime('2026-07-09T23:00:00Z')).toBe('1 h ago');
  });

  it('calculates average speed and article availability for history details', () => {
    const detailed = entry({
      downloaded_bytes: 9_000,
      added_at: '2026-07-09T10:00:00Z',
      completed_at: '2026-07-09T10:00:10Z',
      server_stats: [{
        server_id: 'primary',
        server_name: 'Primary',
        articles_downloaded: 9,
        articles_failed: 1,
        bytes_downloaded: 9_000,
      }],
    });
    expect(component.averageSpeed(detailed)).toBe(900);
    expect(component.articleServed(detailed)).toBe(9);
    expect(component.articleMissing(detailed)).toBe(1);
    expect(component.availability(detailed)).toBe('90.00%');
  });

  it('loads and closes selected history details', () => {
    const detailed = entry({ id: 'detail-id', average_speed_bps: 1234 });
    api.get.mockImplementation(() => of(detailed));
    component.selectEntry(detailed);
    expect(api.get).toHaveBeenCalledWith('/history/detail-id');
    expect(component.selectedEntry()?.average_speed_bps).toBe(1234);
    expect(component.detailLoading()).toBe(false);
    component.closeDetails();
    expect(component.selectedId()).toBeNull();
  });

  it('shows a retryable error instead of the empty state when the fetch fails', () => {
    const failure = { status: 503, error: { human_readable: 'Database busy' } };
    api.get.mockImplementation((path: string) =>
      path === '/status' ? of({ webdav_enabled: true }) : throwError(() => failure),
    );
    component.load();
    expect(component.loading()).toBe(false);
    expect(component.loadError()).toBe("Couldn't load download history: Database busy");
    expect(component.entries()).toEqual([]);

    api.get.mockImplementation((path: string) =>
      path === '/status' ? of({}) : of({ entries: [entry()], total: 1 }),
    );
    component.load();
    expect(component.loadError()).toBeNull();
    expect(component.entries()).toHaveLength(1);
  });

  it('ignores a failure from a request superseded by a newer load', () => {
    const slow = new Subject<unknown>();
    api.get.mockImplementation((path: string) => (path === '/status' ? of({}) : slow.asObservable()));
    component.load();
    api.get.mockImplementation((path: string) =>
      path === '/status' ? of({}) : of({ entries: [entry()], total: 1 }),
    );
    component.load();
    slow.error({ status: 500 });
    expect(component.loadError()).toBeNull();
    expect(component.entries()).toHaveLength(1);
  });

  it('reports a failed detail fetch as a toast and clears the loading flag', () => {
    api.get.mockImplementation(() => throwError(() => ({ status: 404 })));
    component.selectEntry(entry({ id: 'gone' }));
    expect(component.detailLoading()).toBe(false);
    expect(snack.open).toHaveBeenCalledWith('Failed to load download details', 'Close', { duration: 5000 });
  });

  it('selects rows, reports the count, and clears the selection', () => {
    component.toggleSelected('history-1');
    component.toggleSelected('history-2');
    expect([...component.selectedIds()]).toEqual(['history-1', 'history-2']);
    expect(announcer.polite()).toContain('2 selected');
    component.toggleSelected('history-1');
    expect([...component.selectedIds()]).toEqual(['history-2']);
    component.clearSelection();
    expect(component.selectedIds().size).toBe(0);
    expect(announcer.polite()).toContain('Selection cleared');
  });

  it('selects every entry on the page and deselects them again', () => {
    api.get.mockImplementation((path: string) =>
      of(path === '/history'
        ? { entries: [entry(), entry({ id: 'history-2' })], total: 2 }
        : {}),
    );
    component.load();
    expect(component.allPageSelected()).toBe(false);
    component.toggleSelectAll({ target: { checked: true } } as unknown as Event);
    expect(component.allPageSelected()).toBe(true);
    component.toggleSelectAll({ target: { checked: false } } as unknown as Event);
    expect(component.selectedIds().size).toBe(0);
  });

  it('drops selected ids that a reload no longer returns', () => {
    api.get.mockImplementation((path: string) =>
      of(path === '/history' ? { entries: [entry(), entry({ id: 'history-2' })], total: 2 } : {}),
    );
    component.load();
    component.toggleSelected('history-2');
    api.get.mockImplementation((path: string) =>
      of(path === '/history' ? { entries: [entry()], total: 1 } : {}),
    );
    component.load();
    expect(component.selectedIds().size).toBe(0);
  });

  it('bulk-deletes the selection through one request after confirmation', () => {
    const load = vi.spyOn(component, 'load').mockImplementation(() => {});
    api.post.mockReturnValue(of({ status: true, succeeded: 2, failed: 0, failures: [] }));
    component.toggleSelected('history-1');
    component.toggleSelected('history-2');
    component.bulkDelete();
    expect(confirm.confirm).toHaveBeenCalledWith(expect.objectContaining({ danger: true }));
    expect(api.post).toHaveBeenCalledWith('/history/bulk', {
      action: 'delete',
      ids: ['history-1', 'history-2'],
    });
    expect(component.selectedIds().size).toBe(0);
    expect(snack.open).toHaveBeenCalledWith('Deleted 2 entries', 'Close', { duration: 2000 });
    expect(load).toHaveBeenCalled();
  });

  it('bulk-retries without asking, and reports a partial failure', () => {
    const load = vi.spyOn(component, 'load').mockImplementation(() => {});
    api.post.mockReturnValue(
      of({
        status: false,
        succeeded: 1,
        failed: 1,
        failures: [
          {
            id: 'history-2',
            error: { status: 409, error_kind: 'conflict', human_readable: 'No NZB data stored for this entry' },
          },
        ],
      }),
    );
    component.toggleSelected('history-1');
    component.toggleSelected('history-2');
    component.bulkRetry();
    expect(api.post).toHaveBeenCalledWith('/history/bulk', {
      action: 'retry',
      ids: ['history-1', 'history-2'],
    });
    expect(confirm.confirm).not.toHaveBeenCalled();
    expect(snack.open).toHaveBeenCalledWith(
      'Failed to retry 1 of 2 entries: No NZB data stored for this entry',
      'Close',
      { duration: 5000 },
    );
    expect(load).toHaveBeenCalled();
    expect(component.bulkBusy()).toBe(false);
  });

  it('clears a status with the active filters and reports the count removed', () => {
    api.get.mockImplementation((path: string) =>
      of(path === '/history'
        ? {
            entries: [entry({ status: 'failed' })],
            total: 1,
            stats: { completed: 4, completed_bytes: 0, failed: 3, success_pct: 57, avg_duration_secs: null, fail_reasons: [] },
          }
        : {}),
    );
    component.load();
    const load = vi.spyOn(component, 'load').mockImplementation(() => {});
    component.filterTime = '30d';
    component.filterCategory = 'tv';
    component.nameFilter = '  release ';
    api.post.mockReturnValue(of({ status: true, succeeded: 3, failed: 0, failures: [] }));
    component.clearByStatus('failed');
    expect(confirm.confirm).toHaveBeenCalledWith(
      expect.objectContaining({ title: 'Clear 3 failed entries?', danger: true }),
    );
    expect(api.post).toHaveBeenCalledWith('/history/bulk', {
      action: 'delete',
      status: 'failed',
      ids: [],
      days: 30,
      category: 'tv',
      search: 'release',
    });
    expect(snack.open).toHaveBeenCalledWith('Cleared 3 failed entries', 'Close', { duration: 2000 });
    expect(load).toHaveBeenCalled();
    expect(component.clearingStatus()).toBeNull();
  });

  it('sends no time window when clearing all time', () => {
    const load = vi.spyOn(component, 'load').mockImplementation(() => {});
    component.filterTime = 'all';
    api.post.mockReturnValue(of({ status: true, succeeded: 1, failed: 0, failures: [] }));
    component.clearByStatus('completed');
    expect(api.post).toHaveBeenCalledWith('/history/bulk', {
      action: 'delete',
      status: 'completed',
      ids: [],
    });
    expect(load).toHaveBeenCalled();
  });

  it('does not delete an empty selection', () => {
    component.bulkDelete();
    component.bulkRetry();
    expect(confirm.confirm).not.toHaveBeenCalled();
    expect(api.post).not.toHaveBeenCalled();
  });
});
