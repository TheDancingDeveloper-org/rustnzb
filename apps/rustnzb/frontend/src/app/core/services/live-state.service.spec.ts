import { Observable, of, Subject } from 'rxjs';
import { beforeEach, afterEach, describe, expect, it, vi } from 'vitest';
import { LiveStateService } from './live-state.service';
const status = { speed_bps: 1, queue_size: 1 } as never;
const queue = { jobs: [], total: 0 } as never;
describe('LiveStateService', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.useFakeTimers();
  });
  afterEach(() => vi.useRealTimers());
  it('skips the next timer tick while requests are pending', () => {
    const a = new Subject<never>(),
      b = new Subject<never>();
    const api = { get: vi.fn((p: string) => (p === '/status' ? a : b)) };
    const s = new LiveStateService(api as never);
    s.start();
    vi.advanceTimersByTime(500);
    expect(api.get).toHaveBeenCalledTimes(2);
    vi.advanceTimersByTime(500);
    expect(api.get).toHaveBeenCalledTimes(2);
    a.next(status);
    a.complete();
    b.next(queue);
    b.complete();
    expect(s.snapshot()?.revision).toBe(1);
    s.stop();
  });
  it('publishes only after both responses complete', () => {
    const a = new Subject<never>(),
      b = new Subject<never>();
    const s = new LiveStateService({
      get: vi.fn((p: string) => (p === '/status' ? a : b)),
    } as never);
    s.refresh();
    a.next(status);
    a.complete();
    expect(s.snapshot()).toBeNull();
    b.next(queue);
    b.complete();
    expect(s.snapshot()?.revision).toBe(1);
  });
  it('discards a response that arrives after stop', () => {
    const status$ = new Subject<never>();
    const queue$ = new Subject<never>();
    const s = new LiveStateService({
      get: vi.fn((p: string) => (p === '/status' ? status$ : queue$)),
    } as never);
    s.refresh();
    s.stop();
    status$.next(status);
    status$.complete();
    queue$.next(queue);
    queue$.complete();
    expect(s.snapshot()).toBeNull();
  });
  it('discards a superseded response after a manual refresh', () => {
    const requests: Subject<never>[] = [];
    const api = {
      get: vi.fn(
        (_path: string) =>
          new Observable<never>((subscriber) => {
            const subject = new Subject<never>();
            requests.push(subject);
            const subscription = subject.subscribe(subscriber);
            return () => subscription.unsubscribe();
          }),
      ),
    };
    const s = new LiveStateService(api as never);
    s.refresh();
    s.refresh();
    requests[0].next(status);
    requests[0].complete();
    requests[1].next(queue);
    requests[1].complete();
    expect(s.snapshot()).toBeNull();
    requests[2].next(status);
    requests[2].complete();
    requests[3].next(queue);
    requests[3].complete();
    expect(s.snapshot()?.revision).toBe(1);
  });
  it('defaults to 500ms and persists supported timer intervals', () => {
    const api = { get: vi.fn(() => of({})) };
    const s = new LiveStateService(api as never);
    expect(s.interval()).toBe(500);
    s.start();
    s.setIntervalMs(200);
    vi.advanceTimersByTime(199);
    expect(api.get).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(api.get).toHaveBeenCalledTimes(2);
    expect(localStorage.getItem('rustnzb.liveStateInterval')).toBe('200');
    s.setIntervalMs(750);
    expect(s.interval()).toBe(200);
    s.stop();
  });
  it('does not run instrumentation when debug is off', () => {
    const now = vi.spyOn(performance, 'now');
    const debug = vi.spyOn(console, 'debug');
    const s = new LiveStateService({ get: vi.fn(() => of({})) } as never);
    s.refresh();
    expect(now).not.toHaveBeenCalled();
    expect(debug).not.toHaveBeenCalled();
    now.mockRestore();
    debug.mockRestore();
  });
});
