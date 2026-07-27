import '@angular/compiler';

import { describe, expect, it, vi } from 'vitest';
import { of, throwError } from 'rxjs';

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
});
