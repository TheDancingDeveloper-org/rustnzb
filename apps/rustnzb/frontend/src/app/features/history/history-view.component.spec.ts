import '@angular/compiler';

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { of } from 'rxjs';

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

  it('combines name, status, category, and time filters', () => {
    component.entries.set([
      entry(),
      entry({ id: '2', name: 'Release.Failed', category: 'tv', status: 'failed' }),
      entry({ id: '3', name: 'Ancient.Release', completed_at: '2025-01-01T00:00:00Z' }),
    ]);
    component.nameFilter = 'failed';
    component.filterStatus = 'failed';
    component.filterCategory = 'tv';
    expect(component.filteredEntries().map((item) => item.id)).toEqual(['2']);
  });

  it('derives sorted unique category options', () => {
    component.entries.set([
      entry({ category: 'tv' }),
      entry({ id: '2', category: 'movies' }),
      entry({ id: '3', category: 'tv' }),
      entry({ id: '4', category: '' }),
    ]);
    expect(component.categoryOptions()).toEqual(['movies', 'tv']);
  });

  it('computes success statistics and failure summaries', () => {
    component.entries.set([
      entry(),
      entry({ id: '2', total_bytes: 3072 }),
      entry({ id: '3', status: 'failed', error_message: 'CRC mismatch: bad block' }),
    ]);
    expect(component.statCards()).toMatchObject({
      completed: 2,
      completedBytes: 4096,
      failed: 1,
      failReasons: '1 CRC mismatch',
      successPct: 67,
    });
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
