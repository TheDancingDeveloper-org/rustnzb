import { Injectable, OnDestroy, signal } from '@angular/core';
import { Observable, Subscription, forkJoin, map, timeout } from 'rxjs';
import { ApiService, POLL_TIMEOUT_MS, pageHidden } from './api.service';
import { QueueResponse, StatusResponse } from '../models/queue.model';

export const LIVE_STATE_INTERVALS = [200, 500, 1000] as const;
export type LiveStateInterval = (typeof LIVE_STATE_INTERVALS)[number];

export interface LiveSnapshot {
  readonly revision: number;
  readonly capturedAt: number;
  readonly status: Readonly<StatusResponse>;
  readonly queue: Readonly<QueueResponse>;
}

export interface LiveStateMetrics {
  tickStart: number;
  tickEnd: number;
  requestStart: number;
  requestEnd: number;
  concurrentRequests: number;
  responseBytes: number;
  parseTime: number;
  updateTime: number;
  renderTime: number;
  skippedTicks: number;
}

const INTERVAL_KEY = 'rustnzb.liveStateInterval';

@Injectable({ providedIn: 'root' })
export class LiveStateService implements OnDestroy {
  readonly snapshot = signal<LiveSnapshot | null>(null);
  readonly interval = signal<LiveStateInterval>(this.readInterval());
  private timer: ReturnType<typeof setInterval> | null = null;
  private request: Subscription | null = null;
  private revision = 0;
  private concurrentRequests = 0;
  private skippedTicks = 0;
  private isAuthenticated = () => true;
  private readonly debug = this.isDebugEnabled();
  private readonly listeners = new Set<(snapshot: LiveSnapshot) => void>();
  private readonly errorListeners = new Set<() => void>();

  constructor(private readonly api: ApiService) {}

  start(isAuthenticated: () => boolean = () => true): void {
    this.isAuthenticated = isAuthenticated;
    if (this.timer) return;
    this.timer = setInterval(() => this.tick(), this.interval());
  }

  stop(): void {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
    this.request?.unsubscribe();
    this.request = null;
    this.listeners.clear();
    this.errorListeners.clear();
  }

  setIntervalMs(value: number): void {
    if (!(LIVE_STATE_INTERVALS as readonly number[]).includes(value)) return;
    const interval = value as LiveStateInterval;
    this.interval.set(interval);
    localStorage.setItem(INTERVAL_KEY, String(interval));
    if (this.timer) {
      clearInterval(this.timer);
      this.timer = setInterval(() => this.tick(), interval);
    }
  }

  refresh(): void {
    this.tick(true);
  }

  onError(listener: () => void): { unsubscribe(): void } {
    this.errorListeners.add(listener);
    return { unsubscribe: () => this.errorListeners.delete(listener) };
  }

  onSnapshot(listener: (snapshot: LiveSnapshot) => void): { unsubscribe(): void } {
    this.listeners.add(listener);
    const current = this.snapshot();
    if (current) listener(current);
    return { unsubscribe: () => this.listeners.delete(listener) };
  }

  ngOnDestroy(): void {
    this.stop();
  }

  private tick(manual = false): void {
    const clock = () => (this.debug ? performance.now() : 0);
    const tickStart = clock();
    if (this.request || !this.isAuthenticated() || (!manual && pageHidden() && !this.debug)) {
      if (this.debug) {
        this.skippedTicks++;
        this.debugMetrics({ tickStart, tickEnd: clock(), skippedTicks: this.skippedTicks });
      }
      return;
    }

    const requestStart = clock();
    this.concurrentRequests += 2;
    const request = forkJoin({
      status: this.fetch<StatusResponse>('/status'),
      queue: this.fetch<QueueResponse>('/queue'),
    }).subscribe({
      next: ({ status, queue }) => {
        const responseBytes = status.bytes + queue.bytes;
        const parseTime = status.parseTime + queue.parseTime;
        const updateStart = clock();
        const snapshot: LiveSnapshot = Object.freeze({
          revision: ++this.revision,
          capturedAt: Date.now(),
          status: Object.freeze(status.value),
          queue: Object.freeze(queue.value),
        });
        this.snapshot.set(snapshot);
        const renderStart = clock();
        for (const listener of this.listeners) listener(snapshot);
        const renderTime = this.debug ? clock() - renderStart : 0;
        const updateTime = this.debug ? clock() - updateStart : 0;
        this.finish(tickStart, requestStart, responseBytes, parseTime, updateTime, renderTime);
      },
      error: () => {
        for (const listener of this.errorListeners) listener();
        this.finish(tickStart, requestStart, 0, 0, 0, 0);
      },
    });
    this.request = request.closed ? null : request;
  }

  private fetch<T>(path: string): Observable<{ value: T; bytes: number; parseTime: number }> {
    if (!this.debug) {
      return this.api.get<T>(path).pipe(
        timeout(POLL_TIMEOUT_MS),
        map((value) => ({ value, bytes: 0, parseTime: 0 })),
      );
    }
    return this.api.getRaw(path).pipe(
      timeout(POLL_TIMEOUT_MS),
      map((raw) => {
        const parseStart = performance.now();
        const value = JSON.parse(raw) as T;
        return { value, bytes: raw.length, parseTime: performance.now() - parseStart };
      }),
    );
  }

  private finish(
    tickStart: number,
    requestStart: number,
    responseBytes: number,
    parseTime: number,
    updateTime: number,
    renderTime: number,
  ): void {
    this.concurrentRequests = Math.max(0, this.concurrentRequests - 2);
    this.request = null;
    this.debugMetrics({
      tickStart,
      tickEnd: this.debug ? performance.now() : 0,
      requestStart,
      requestEnd: this.debug ? performance.now() : 0,
      concurrentRequests: this.concurrentRequests,
      responseBytes,
      parseTime,
      updateTime,
      renderTime,
      skippedTicks: this.skippedTicks,
    });
  }

  private debugMetrics(
    metrics: Partial<LiveStateMetrics> &
      Pick<LiveStateMetrics, 'tickStart' | 'tickEnd' | 'skippedTicks'>,
  ): void {
    if (this.debug) console.debug('[live-state]', metrics);
  }

  private readInterval(): LiveStateInterval {
    const stored = Number(localStorage.getItem(INTERVAL_KEY));
    return (LIVE_STATE_INTERVALS as readonly number[]).includes(stored)
      ? (stored as LiveStateInterval)
      : 500;
  }

  private isDebugEnabled(): boolean {
    return (
      typeof localStorage !== 'undefined' &&
      localStorage.getItem('rustnzb.liveStateDebug') === 'true'
    );
  }
}
