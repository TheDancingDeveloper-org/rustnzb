import { of, Subject } from 'rxjs';
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
});
