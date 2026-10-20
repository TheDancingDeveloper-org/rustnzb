import { Injectable, signal } from '@angular/core';

/**
 * Screen-reader announcements for results that arrive without a focus change:
 * bulk actions, uploads and the selection count. The app shell renders two
 * live regions bound to `polite` and `assertive`; failures go assertive so the
 * reader interrupts, everything else waits its turn.
 *
 * A live region only speaks when its text changes, so repeating the same
 * sentence (pausing two jobs gives "Job paused" twice) would be silent the
 * second time. An alternating zero-width space makes each message a distinct
 * DOM update without changing what is spoken.
 */
@Injectable({ providedIn: 'root' })
export class AnnouncerService {
  readonly polite = signal('');
  readonly assertive = signal('');
  private toggle = 0;

  announce(message: string): void {
    this.assertive.set('');
    this.polite.set(this.decorate(message));
  }

  announceError(message: string): void {
    this.polite.set('');
    this.assertive.set(this.decorate(message));
  }

  private decorate(message: string): string {
    this.toggle ^= 1;
    return this.toggle ? `${message}\u200b` : message;
  }
}
