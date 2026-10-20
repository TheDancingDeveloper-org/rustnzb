import { Injectable } from '@angular/core';
import { MatSnackBar } from '@angular/material/snack-bar';

/** How long a destructive action's undo stays on screen. */
export const UNDO_WINDOW_MS = 10_000;

/**
 * The app's one way to speak through a snackbar.
 *
 * Results that arrive without a focus change also go through
 * `AnnouncerService`; this service only owns the visual toast, so pages keep
 * their spoken message next to the one a sighted user sees.
 */
@Injectable({ providedIn: 'root' })
export class ToastService {
  constructor(private readonly snack: MatSnackBar) {}

  /** A routine success. Two seconds is the house default. */
  success(message: string, duration = 2_000): void {
    this.snack.open(message, 'Close', { duration });
  }

  /** An informational message that needs longer to read (a path, a hint). */
  info(message: string, duration = 3_000): void {
    this.snack.open(message, 'Close', { duration });
  }

  /** A failure. Errors stay up longer than successes. */
  error(message: string, duration = 5_000): void {
    this.snack.open(message, 'Close', { duration });
  }

  /**
   * Report a destructive action that the server can reverse, with an `Undo`
   * action. `onUndo` runs only when the user presses it; dismissing the toast
   * or letting it time out leaves the action in place.
   */
  undo(
    message: string,
    onUndo: () => void,
    options: { duration?: number; undoLabel?: string } = {},
  ): void {
    const ref = this.snack.open(message, options.undoLabel ?? 'Undo', {
      duration: options.duration ?? UNDO_WINDOW_MS,
    });
    ref.onAction().subscribe(() => onUndo());
  }

  /**
   * MatSnackBar's own shape, kept so `showHttpError()` and callers that pass a
   * custom action work unchanged.
   */
  open(message: string, action?: string, config?: { duration?: number }): unknown {
    return this.snack.open(message, action, config);
  }
}
