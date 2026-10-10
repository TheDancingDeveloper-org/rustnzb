import '@angular/compiler';

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { firstValueFrom, of } from 'rxjs';

import { ApiService } from '../../core/services/api.service';
import { HistoryEntry } from '../../core/models/queue.model';
import { HistoryViewComponent } from './history-view.component';

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
    component = new HistoryViewComponent(
      api as unknown as ApiService,
      snack as never,
      confirm as never,
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
});
