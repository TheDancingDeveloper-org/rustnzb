import { of, Subject } from 'rxjs';
import { describe, expect, it, vi } from 'vitest';
import { LiveStateService } from './live-state.service';

const status = { speed_bps: 1, queue_size: 1 } as never;
const queue = { jobs: [], total: 0 } as never;

describe('LiveStateService', () => {
  it('skips a tick while the paired requests are in flight', () => {
    const status$ = new Subject<never>();
    const queue$ = new Subject<never>();
    const api = { get: vi.fn((path: string) => (path === '/status' ? status$ : queue$)) };
    const service = new LiveStateService(api as never);
    service.start();
    service.refresh();
    expect(api.get).toHaveBeenCalledTimes(2);
    status$.next(status);
    status$.complete();
    queue$.next(queue);
    queue$.complete();
    expect(service.snapshot()?.revision).toBe(1);
    service.stop();
  });

  it('publishes only complete snapshots and revisions increase monotonically', () => {
    const api = { get: vi.fn((path: string) => of(path === '/status' ? status : queue)) };
    const service = new LiveStateService(api as never);
    service.refresh();
    service.refresh();
    expect(service.snapshot()?.revision).toBe(2);
  });

  it('accepts only the supported local intervals', () => {
    const service = new LiveStateService({ get: vi.fn(() => of({})) } as never);
    service.setIntervalMs(200);
    expect(service.interval()).toBe(200);
    service.setIntervalMs(750);
    expect(service.interval()).toBe(200);
  });
});
