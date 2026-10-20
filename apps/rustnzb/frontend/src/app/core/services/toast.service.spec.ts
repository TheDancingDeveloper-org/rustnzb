import '@angular/compiler';

import { Subject } from 'rxjs';
import { describe, expect, it, vi } from 'vitest';

import { ToastService, UNDO_WINDOW_MS } from './toast.service';

function service() {
  const ref = { onAction: vi.fn(() => new Subject<void>()) };
  const snack = { open: vi.fn(() => ref) };
  return { toast: new ToastService(snack as never), snack, ref };
}

describe('ToastService', () => {
  it('uses the house durations for success, info and error', () => {
    const { toast, snack } = service();
    toast.success('Saved');
    toast.info('Somewhere');
    toast.error('Broken');
    expect(snack.open).toHaveBeenNthCalledWith(1, 'Saved', 'Close', { duration: 2000 });
    expect(snack.open).toHaveBeenNthCalledWith(2, 'Somewhere', 'Close', { duration: 3000 });
    expect(snack.open).toHaveBeenNthCalledWith(3, 'Broken', 'Close', { duration: 5000 });
  });

  it('offers Undo for the full undo window and runs the callback on action', () => {
    const { toast, snack, ref } = service();
    const onUndo = vi.fn();
    toast.undo('Deleted "Release"', onUndo);
    expect(snack.open).toHaveBeenCalledWith('Deleted "Release"', 'Undo', {
      duration: UNDO_WINDOW_MS,
    });
    expect(onUndo).not.toHaveBeenCalled();
    const action = ref.onAction.mock.results[0].value as Subject<void>;
    action.next();
    expect(onUndo).toHaveBeenCalledTimes(1);
  });

  it('leaves the action in place when the toast times out or is dismissed', () => {
    const { toast, ref } = service();
    const onUndo = vi.fn();
    toast.undo('Deleted', onUndo);
    const action = ref.onAction.mock.results[0].value as Subject<void>;
    action.complete();
    expect(onUndo).not.toHaveBeenCalled();
  });

  it('still exposes MatSnackBar open for showHttpError and custom actions', () => {
    const { toast, snack } = service();
    toast.open('Failed', 'Close', { duration: 5000 });
    expect(snack.open).toHaveBeenCalledWith('Failed', 'Close', { duration: 5000 });
  });
});
