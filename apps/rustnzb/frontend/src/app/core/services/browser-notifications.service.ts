import { Injectable, signal, inject } from '@angular/core';

import { HistoryEntry, PauseReason, StatusResponse } from '../models/queue.model';
import { ApiService } from './api.service';

const STORAGE_KEY = 'rustnzb-browser-notifications';
/** How many recent history ids are remembered, so each outcome alerts once. */
const SEEN_LIMIT = 200;
/** Newest history rows fetched when checking for outcomes. */
const CHECK_PAGE_SIZE = '20';

export interface BrowserNotice {
  /** History id for completed or failed jobs; stable tag for other alerts. */
  tag: string;
  title: string;
  body: string;
}

interface HistoryPage {
  entries: HistoryEntry[];
}

/**
 * The notices for history rows not seen before, oldest first. Marks every
 * row in `entries` as seen, so a later check does not repeat them. Rows
 * other than completed or failed produce no notice.
 */
export function outcomeNotices(entries: HistoryEntry[], seen: Set<string>): BrowserNotice[] {
  const fresh = entries.filter((entry) => !seen.has(entry.id));
  for (const entry of entries) seen.add(entry.id);

  const notices: BrowserNotice[] = [];
  for (const entry of fresh.reverse()) {
    if (entry.status === 'completed') {
      notices.push({ tag: entry.id, title: 'Download complete', body: entry.name });
    } else if (entry.status === 'failed') {
      notices.push({
        tag: entry.id,
        title: 'Download failed',
        body: entry.error_message ? `${entry.name}: ${entry.error_message}` : entry.name,
      });
    }
  }
  return notices;
}

/**
 * Opt-in desktop alerts for finished downloads and disk-space pauses.
 *
 * The status poll drives it. A pause for low disk space alerts on the
 * transition into that pause. A job outcome is looked up in history when the
 * queue shrinks, so each finished job costs one request. The first check
 * after enabling only records what already exists, so old outcomes do not
 * alert. Alerts show only while the tab is in the background.
 */
@Injectable({ providedIn: 'root' })
export class BrowserNotificationsService {
  private readonly api = inject(ApiService);

  /** Whether this browser can show notifications at all. */
  readonly supported: boolean = typeof Notification !== 'undefined';
  /** Whether the user has opted in and the browser grants permission. */
  readonly enabled = signal(this.readStored());
  /** The browser's current permission, or `unsupported`. */
  readonly permission = signal<NotificationPermission | 'unsupported'>(this.currentPermission());

  /** History ids already seen; `null` until the baseline check has run. */
  private seen: Set<string> | null = null;
  private lastQueueSize: number | null = null;
  private lastPauseReason: PauseReason | null = null;
  private checking = false;

  /**
   * Turn alerts on or off. Turning them on asks the browser for permission,
   * which must follow a user gesture. Resolves to whether alerts are now on.
   */
  async setEnabled(on: boolean): Promise<boolean> {
    if (!on) {
      this.store(false);
      this.enabled.set(false);
      return false;
    }
    if (!this.supported) return false;

    let permission = Notification.permission;
    if (permission === 'default') {
      permission = await Notification.requestPermission();
    }
    this.permission.set(permission);

    const granted = permission === 'granted';
    this.store(granted);
    this.enabled.set(granted);
    // Re-baseline on the next status poll: outcomes from before this point
    // are recorded, not announced.
    this.seen = null;
    this.lastQueueSize = null;
    return granted;
  }

  /** Feed each successful status poll into the alert logic. */
  observeStatus(status: StatusResponse): void {
    const pauseReason = status.pause_reason ?? null;
    const enteredDiskPause = pauseReason === 'disk_space' && this.lastPauseReason !== 'disk_space';
    this.lastPauseReason = pauseReason;

    const queueShrank = this.lastQueueSize !== null && status.queue_size < this.lastQueueSize;
    this.lastQueueSize = status.queue_size;

    if (!this.enabled()) return;
    if (enteredDiskPause) {
      this.show({
        tag: 'disk-space-pause',
        title: 'Downloads paused',
        body: 'Free space on a storage volume is below the minimum.',
      });
    }
    if (this.seen === null || queueShrank) {
      this.checkHistory();
    }
  }

  private checkHistory(): void {
    if (this.checking) return;
    this.checking = true;
    this.api.get<HistoryPage>('/history', { offset: '0', limit: CHECK_PAGE_SIZE }).subscribe({
      next: (page) => {
        this.checking = false;
        const entries = page.entries ?? [];
        if (this.seen === null) {
          this.seen = new Set(entries.map((entry) => entry.id));
          return;
        }
        for (const notice of outcomeNotices(entries, this.seen)) {
          this.show(notice);
        }
        this.trimSeen();
      },
      error: () => {
        this.checking = false;
      },
    });
  }

  /** Keep the seen set bounded. Oldest ids go first (insertion order). */
  private trimSeen(): void {
    if (!this.seen || this.seen.size <= SEEN_LIMIT) return;
    const excess = this.seen.size - SEEN_LIMIT;
    let removed = 0;
    for (const id of this.seen) {
      if (removed++ >= excess) break;
      this.seen.delete(id);
    }
  }

  private show(notice: BrowserNotice): void {
    if (!this.enabled() || !this.supported) return;
    if (Notification.permission !== 'granted') return;
    if (document.visibilityState === 'visible') return;
    try {
      new Notification(notice.title, { body: notice.body, tag: notice.tag });
    } catch {
      // Some platforms refuse direct construction (e.g. service-worker-only).
      // The in-app status pill still shows the state, so there is nothing to do.
    }
  }

  private readStored(): boolean {
    try {
      return localStorage.getItem(STORAGE_KEY) === 'on' && this.currentPermission() === 'granted';
    } catch {
      return false;
    }
  }

  private store(on: boolean): void {
    try {
      localStorage.setItem(STORAGE_KEY, on ? 'on' : 'off');
    } catch {
      // Storage can be blocked (private windows). The choice then lasts for this page only.
    }
  }

  private currentPermission(): NotificationPermission | 'unsupported' {
    return this.supported ? Notification.permission : 'unsupported';
  }
}
