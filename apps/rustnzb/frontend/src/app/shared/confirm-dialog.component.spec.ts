import '@angular/compiler';

import { TestBed } from '@angular/core/testing';
import { MAT_DIALOG_DATA, MatDialogRef } from '@angular/material/dialog';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { ConfirmDialogComponent, ConfirmDialogData } from './confirm-dialog.component';

describe('ConfirmDialogComponent', () => {
  let dialogRef: { close: ReturnType<typeof vi.fn> };

  function render(data: ConfirmDialogData) {
    TestBed.configureTestingModule({
      imports: [ConfirmDialogComponent],
      providers: [
        { provide: MAT_DIALOG_DATA, useValue: data },
        { provide: MatDialogRef, useValue: dialogRef },
      ],
    });
    const fixture = TestBed.createComponent(ConfirmDialogComponent);
    fixture.detectChanges();
    return fixture.nativeElement as HTMLElement;
  }

  function buttons(root: HTMLElement): HTMLButtonElement[] {
    return Array.from(root.querySelectorAll<HTMLButtonElement>('mat-dialog-actions button'));
  }

  beforeEach(() => {
    dialogRef = { close: vi.fn() };
  });

  it('renders the title and message with default button labels', () => {
    const root = render({ title: 'Clear history', message: 'Remove all entries?' });
    expect(root.querySelector('h2')?.textContent?.trim()).toBe('Clear history');
    expect(root.querySelector('.message')?.textContent?.trim()).toBe('Remove all entries?');
    const [cancel, confirm] = buttons(root);
    expect(cancel.textContent?.trim()).toBe('Cancel');
    expect(confirm.textContent?.trim()).toBe('Confirm');
  });

  it('closes with false on cancel and true on confirm', () => {
    const root = render({ title: 't', message: 'm' });
    const [cancel, confirm] = buttons(root);

    cancel.click();
    expect(dialogRef.close).toHaveBeenLastCalledWith(false);

    confirm.click();
    expect(dialogRef.close).toHaveBeenLastCalledWith(true);
  });

  it('uses custom labels and styles the confirm button as danger when requested', () => {
    const root = render({
      title: 't',
      message: 'm',
      confirmLabel: 'Delete',
      cancelLabel: 'Keep',
      danger: true,
    });
    const [cancel, confirm] = buttons(root);
    expect(cancel.textContent?.trim()).toBe('Keep');
    expect(confirm.textContent?.trim()).toBe('Delete');
    expect(confirm.classList).toContain('danger');
    expect(confirm.classList).not.toContain('primary');
  });

  it('styles the confirm button as primary when not dangerous', () => {
    const root = render({ title: 't', message: 'm' });
    const [, confirm] = buttons(root);
    expect(confirm.classList).toContain('primary');
    expect(confirm.classList).not.toContain('danger');
  });
});
