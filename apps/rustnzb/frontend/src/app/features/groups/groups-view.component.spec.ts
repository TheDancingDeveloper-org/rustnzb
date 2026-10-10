import '@angular/compiler';

import { afterEach, describe, expect, it, vi } from 'vitest';
import { Subject, of, throwError } from 'rxjs';

import { GroupService } from '../../core/services/group.service';
import { GroupRow, HeaderRow } from '../../core/models/group.model';
import { GroupsViewComponent } from './groups-view.component';

const group: GroupRow = {
  id: 7, name: 'alt.binaries.tv', description: null, subscribed: true, article_count: 1,
  first_article: 1, last_article: 1, last_scanned: 0, last_updated: null, created_at: '2026-01-01', unread_count: 2,
};
const header: HeaderRow = {
  id: 1, group_id: 7, article_num: 1, subject: 'Episode', author: 'poster', date: 'today',
  message_id: '<one>', references_: '', bytes: 2048, lines: 1, read: false, downloaded_at: '',
};

function makeComponent(overrides: Record<string, ReturnType<typeof vi.fn>> = {}) {
  const service = {
    list: vi.fn(() => of({ groups: [group], total: 1, limit: 500, offset: 0 })),
    listHeaders: vi.fn(() => of({ headers: [header], total: 1, limit: 100, offset: 0 })),
    getStatus: vi.fn(() => of({ new_available: 3 })),
    getArticle: vi.fn(() => of({ body: 'article body' })),
    downloadSelected: vi.fn(() => of({ status: true, job_id: 'job', message: 'Queued' })),
    fetchHeaders: vi.fn(() => of({ status: true, message: 'Fetching' })),
    markAllRead: vi.fn(() => of({ marked: 1 })),
    ...overrides,
  };
  const snack = { open: vi.fn() };
  const dialog = { open: vi.fn() };
  return { component: new GroupsViewComponent(service as unknown as GroupService, snack as never, dialog as never), service, snack };
}

describe('GroupsViewComponent', () => {
  it('loads subscriptions, headers, and availability for a selected group', () => {
    const { component, service } = makeComponent();
    component.ngOnInit();
    component.selectGroup(group);

    expect(component.groups()).toEqual([group]);
    expect(service.listHeaders).toHaveBeenCalledWith(7, { search: undefined, limit: 100, offset: 0 });
    expect(component.headers()).toEqual([header]);
    expect(component.newAvailable()).toBe(3);
  });

  it('loads article previews, updates unread state, and preserves a useful API error state', () => {
    const { component } = makeComponent();
    component.selectGroup(group);
    component.selectArticle(header);
    expect(component.articleBody()).toBe('article body');
    expect(component.headers()[0].read).toBe(true);

    const { component: failed } = makeComponent({ getArticle: vi.fn(() => throwError(() => new Error('gone'))) });
    failed.selectGroup(group);
    failed.selectArticle(header);
    expect(failed.articleBody()).toBe('(Failed to load)');
    expect(failed.articleLoading()).toBe(false);
  });

  it('selects all headers and reports download failures', () => {
    const { component, service, snack } = makeComponent({ downloadSelected: vi.fn(() => throwError(() => new Error('queue unavailable'))) });
    component.selectGroup(group);
    component.toggleSelectAll();
    expect(component.selectedIds()).toEqual(['<one>']);
    expect(component.selectedBytes()).toBe(2048);
    component.downloadSelected();
    expect(service.downloadSelected).toHaveBeenCalledWith(7, ['<one>']);
    expect(snack.open).toHaveBeenCalledWith('Download failed', 'Close', { duration: 5000 });
  });

  it('recomputes the subscribed-group list when the name filter changes (BUG-116)', () => {
    const other: GroupRow = { ...group, id: 8, name: 'alt.binaries.movies' };
    const { component } = makeComponent({
      list: vi.fn(() => of({ groups: [group, other], total: 2, limit: 500, offset: 0 })),
    });
    component.ngOnInit();
    expect(component.filteredGroups().map((g) => g.id)).toEqual([7, 8]);
    component.groupNameFilter.set('MOVIES');
    expect(component.filteredGroups().map((g) => g.id)).toEqual([8]);
    component.groupNameFilter.set('');
    expect(component.filteredGroups().map((g) => g.id)).toEqual([7, 8]);
  });

  it('keeps earlier pages when headers are reloaded after "Load more" (BUG-117)', () => {
    const page = (start: number, n: number): HeaderRow[] =>
      Array.from({ length: n }, (_, i) => ({ ...header, id: start + i, message_id: `<${start + i}>` }));
    const listHeaders = vi.fn((_id: number, q: { limit: number; offset: number }) =>
      of({ headers: page(q.offset, Math.min(q.limit, 250 - q.offset)), total: 250, limit: q.limit, offset: q.offset }),
    );
    const { component } = makeComponent({ listHeaders });
    component.selectGroup(group);
    expect(component.headers().length).toBe(100);
    component.loadMore();
    expect(component.headers().length).toBe(200);
    // A later refresh (fetch poll, mark-all-read) must not collapse the list
    // to just the most recent page.
    component.loadHeaders();
    expect(component.headers().length).toBe(200);
    expect(component.headers()[0].message_id).toBe('<0>');
    expect(new Set(component.headers().map((h) => h.message_id)).size).toBe(200);
    component.markAllRead();
    expect(component.headers().length).toBe(200);
  });

  describe('header fetch polling (BUG-118)', () => {
    afterEach(() => vi.useRealTimers());

    it('stops polling and never shows the give-up toast after the view is destroyed', () => {
      vi.useFakeTimers();
      const { component, service, snack } = makeComponent();
      component.selectGroup(group);
      component.fetchHeaders();
      snack.open.mockClear();
      component.ngOnDestroy();
      const calls = service.listHeaders.mock.calls.length;
      vi.advanceTimersByTime(130_000);
      expect(service.listHeaders.mock.calls.length).toBe(calls);
      expect(snack.open).not.toHaveBeenCalled();
    });

    it('stops polling when the fetch request fails', () => {
      vi.useFakeTimers();
      const fetch = new Subject<never>();
      const { component, service, snack } = makeComponent({
        fetchHeaders: vi.fn(() => fetch.asObservable()),
      });
      component.selectGroup(group);
      component.fetchHeaders();
      fetch.error(new Error('no server'));
      expect(component.fetching()).toBe(false);
      const calls = service.listHeaders.mock.calls.length;
      snack.open.mockClear();
      vi.advanceTimersByTime(130_000);
      expect(service.listHeaders.mock.calls.length).toBe(calls);
      expect(snack.open).not.toHaveBeenCalledWith(
        expect.stringContaining('taking longer'), expect.anything(), expect.anything(),
      );
    });

    it('does not stack pollers when fetch is clicked twice', () => {
      vi.useFakeTimers();
      const { component, service } = makeComponent();
      component.selectGroup(group);
      component.fetchHeaders();
      component.fetchHeaders();
      const before = service.listHeaders.mock.calls.length;
      vi.advanceTimersByTime(3000);
      expect(service.listHeaders.mock.calls.length - before).toBe(1);
      component.ngOnDestroy();
    });
  });
});
