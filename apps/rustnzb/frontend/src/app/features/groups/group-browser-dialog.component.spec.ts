import '@angular/compiler';

import { TestBed } from '@angular/core/testing';
import { MatDialogRef } from '@angular/material/dialog';
import { MatSnackBar } from '@angular/material/snack-bar';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { of, throwError } from 'rxjs';

import { GroupService } from '../../core/services/group.service';
import { GroupRow } from '../../core/models/group.model';
import { GroupBrowserDialogComponent } from './group-browser-dialog.component';

function group(id: number, overrides: Partial<GroupRow> = {}): GroupRow {
  return {
    id,
    name: `alt.test.${id}`,
    article_count: id * 10,
    subscribed: false,
    ...overrides,
  } as GroupRow;
}

describe('GroupBrowserDialogComponent', () => {
  let groupService: {
    list: ReturnType<typeof vi.fn>;
    refresh: ReturnType<typeof vi.fn>;
    subscribe: ReturnType<typeof vi.fn>;
    unsubscribe: ReturnType<typeof vi.fn>;
  };
  let snack: { open: ReturnType<typeof vi.fn> };
  let dialogRef: { close: ReturnType<typeof vi.fn> };

  beforeEach(() => {
    groupService = {
      list: vi.fn(() => of({ groups: [group(1), group(2)], total: 2 })),
      refresh: vi.fn(() => of({ status: true, message: 'Refreshed 2 groups', total: 2 })),
      subscribe: vi.fn(() => of({})),
      unsubscribe: vi.fn(() => of({})),
    };
    snack = { open: vi.fn() };
    dialogRef = { close: vi.fn() };
    // The component imports MatSnackBarModule, which supplies its own MatSnackBar,
    // so overrideProvider is needed to replace it (a plain provider is shadowed).
    TestBed.overrideProvider(MatSnackBar, { useValue: snack });
    TestBed.configureTestingModule({
      imports: [GroupBrowserDialogComponent],
      providers: [
        { provide: GroupService, useValue: groupService },
        { provide: MatDialogRef, useValue: dialogRef },
      ],
    });
  });

  function render() {
    const fixture = TestBed.createComponent(GroupBrowserDialogComponent);
    fixture.detectChanges();
    const component = fixture.componentInstance;
    const el = fixture.nativeElement as HTMLElement;
    return { fixture, component, el };
  }

  it('loads the first page of groups on init and renders one row per group', () => {
    const { el } = render();
    expect(groupService.list).toHaveBeenCalledWith({ search: undefined, limit: 100, offset: 0 });
    const names = Array.from(el.querySelectorAll('.gname')).map((n) => n.textContent?.trim());
    expect(names).toEqual(['alt.test.1', 'alt.test.2']);
  });

  it('shows the empty-state hint when no groups are returned', () => {
    groupService.list.mockReturnValue(of({ groups: [], total: 0 }));
    const { el } = render();
    expect(el.querySelector('.empty')?.textContent).toContain('No groups found');
    expect(el.querySelector('.group-row')).toBeNull();
  });

  it('appends the next page when Load more is clicked', () => {
    groupService.list
      .mockReturnValueOnce(of({ groups: [group(1)], total: 2 }))
      .mockReturnValueOnce(of({ groups: [group(2)], total: 2 }));
    const { fixture, component, el } = render();

    const more = el.querySelector<HTMLButtonElement>('.more button');
    expect(more?.textContent).toContain('Load more (1 remaining)');
    component.loadMoreGroups();
    fixture.detectChanges();

    expect(groupService.list).toHaveBeenLastCalledWith({
      search: undefined,
      limit: 100,
      offset: 100,
    });
    expect(component.groups().map((g) => g.id)).toEqual([1, 2]);
  });

  it('refreshes from the server, shows the result and reloads the list', () => {
    const { component } = render();
    groupService.list.mockClear();

    component.refresh();

    expect(groupService.refresh).toHaveBeenCalledOnce();
    expect(snack.open).toHaveBeenCalledWith('Refreshed 2 groups', 'Close', { duration: 3000 });
    expect(groupService.list).toHaveBeenCalledOnce();
    expect(component.refreshing()).toBe(false);
  });

  it('clears the refreshing flag when a refresh fails', () => {
    groupService.refresh.mockReturnValue(
      throwError(() => ({ error: { message: 'upstream down' } })),
    );
    const { component } = render();

    component.refresh();

    expect(component.refreshing()).toBe(false);
    expect(snack.open).toHaveBeenCalled();
  });

  it('subscribes to an unsubscribed group and flips its star', () => {
    const { component } = render();
    const row = component.groups()[0];

    component.toggleSub(row);

    expect(groupService.subscribe).toHaveBeenCalledWith(1);
    expect(groupService.unsubscribe).not.toHaveBeenCalled();
    expect(row.subscribed).toBe(true);
    expect(snack.open).toHaveBeenCalledWith('Subscribed to alt.test.1', 'Close', {
      duration: 2000,
    });
    expect(component.subPendingIds().has(1)).toBe(false);
  });

  it('unsubscribes a subscribed group', () => {
    groupService.list.mockReturnValue(of({ groups: [group(3, { subscribed: true })], total: 1 }));
    const { component } = render();
    const row = component.groups()[0];

    component.toggleSub(row);

    expect(groupService.unsubscribe).toHaveBeenCalledWith(3);
    expect(row.subscribed).toBe(false);
    expect(snack.open).toHaveBeenCalledWith('Unsubscribed from alt.test.3', 'Close', {
      duration: 2000,
    });
  });

  it('keeps the star unchanged and clears the pending flag when a toggle fails', () => {
    groupService.subscribe.mockReturnValue(throwError(() => ({ error: { message: 'nope' } })));
    const { component } = render();
    const row = component.groups()[0];

    component.toggleSub(row);

    expect(row.subscribed).toBe(false);
    expect(component.subPendingIds().has(1)).toBe(false);
    expect(snack.open).toHaveBeenCalled();
  });
});
