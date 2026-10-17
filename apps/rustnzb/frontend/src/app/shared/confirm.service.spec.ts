import '@angular/compiler';

import { TestBed } from '@angular/core/testing';
import { MatDialog } from '@angular/material/dialog';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { of } from 'rxjs';

import { ConfirmDialogComponent } from './confirm-dialog.component';
import { ConfirmService } from './confirm.service';

describe('ConfirmService', () => {
  let dialog: { open: ReturnType<typeof vi.fn> };
  let service: ConfirmService;

  beforeEach(() => {
    dialog = { open: vi.fn(() => ({ afterClosed: () => of(true) })) };
    TestBed.configureTestingModule({ providers: [{ provide: MatDialog, useValue: dialog }] });
    service = TestBed.inject(ConfirmService);
  });

  it('opens the confirm dialog with the given data and emits true when confirmed', () => {
    const data = { title: 'Delete server', message: 'This cannot be undone.', danger: true };
    let result: boolean | undefined;
    service.confirm(data).subscribe((value) => (result = value));

    expect(dialog.open).toHaveBeenCalledWith(ConfirmDialogComponent, {
      data,
      width: '380px',
      autoFocus: 'dialog',
    });
    expect(result).toBe(true);
  });

  it('emits false for any close result other than an explicit true', () => {
    dialog.open.mockReturnValue({ afterClosed: () => of(false) });
    let cancelled: boolean | undefined;
    service.confirm({ title: 't', message: 'm' }).subscribe((value) => (cancelled = value));
    expect(cancelled).toBe(false);

    dialog.open.mockReturnValue({ afterClosed: () => of(undefined) });
    let dismissed: boolean | undefined;
    service.confirm({ title: 't', message: 'm' }).subscribe((value) => (dismissed = value));
    expect(dismissed).toBe(false);
  });
});
