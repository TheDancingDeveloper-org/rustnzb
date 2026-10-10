import '@angular/compiler';

import { describe, expect, it, vi } from 'vitest';
import { Observable, of } from 'rxjs';

import { ApiService } from '../../core/services/api.service';
import { LogsViewComponent } from './logs-view.component';

type TestLog = { seq: number; level: string; message: string; timestamp: string; target?: string };
type LogsBody = { entries: TestLog[]; latest_seq?: number; boot_id?: string };
const line = (seq: number, message = `line ${seq}`): TestLog => ({ seq, level: 'INFO', message, timestamp: '' });

function setup() {
  const api = {
    get: vi.fn<(...args: unknown[]) => Observable<LogsBody>>(() =>
      of({ entries: [] }),
    ),
  };
  const component = new LogsViewComponent(api as unknown as ApiService);
  component.entries.set([
    { seq: 1, level: 'INFO', message: 'server started', timestamp: '2026-07-10T10:11:12.123Z', target: 'app' },
    { seq: 2, level: 'ERROR', message: 'CRC mismatch', timestamp: '2026-07-10T10:11:13Z', target: 'decode' },
  ]);
  return { api, component };
}

describe('LogsViewComponent', () => {
  it('reactively combines level and regex filters', () => {
    const { component } = setup();
    component.levelFilter.set('ERROR');
    expect(component.visibleEntries().map((entry) => entry.seq)).toEqual([2]);
    component.filter.set('does-not-match');
    expect(component.visibleEntries()).toEqual([]);
  });

  it('falls back to a literal match for invalid regular expressions', () => {
    const { component } = setup();
    component.entries.update((entries) => [
      ...entries,
      { seq: 3, level: 'WARN', message: 'literal [ value', timestamp: '', target: '' },
    ]);
    component.filter.set('[');
    expect(component.visibleEntries().map((entry) => entry.seq)).toEqual([3]);
  });

  it('appends incremental logs and requests subsequent sequence numbers', () => {
    const { api, component } = setup();
    api.get
      .mockReturnValueOnce(of({ entries: [{ seq: 4, level: 'INFO', message: 'new', timestamp: '' }] }))
      .mockReturnValueOnce(of({ entries: [] }));
    component.entries.set([]);
    component.loadLogs();
    component.loadLogs();
    expect(api.get).toHaveBeenNthCalledWith(1, '/logs', {});
    expect(api.get).toHaveBeenNthCalledWith(2, '/logs', { after_seq: '4' });
  });

  it('maps timestamp and level display formats', () => {
    const { component } = setup();
    expect(component.formatTs('2026-07-10T10:11:12.123456Z')).toBe('10:11:12.123');
    expect(component.levelClass('ERROR')).toBe('err');
    expect(component.levelClass('warning')).toBe('warn');
    expect(component.levelClass('TRACE')).toBe('dbg');
  });

  it('clears buffered entries and toggles following state', () => {
    const { component } = setup();
    component.toggleFollow();
    expect(component.follow()).toBe(false);
    component.clear();
    expect(component.entries()).toEqual([]);
  });

  it('treats seq 0 as a real cursor instead of refetching the whole buffer (BUG-119)', () => {
    const { api, component } = setup();
    component.entries.set([]);
    api.get
      .mockReturnValueOnce(of({ entries: [line(0)], latest_seq: 0, boot_id: 'a' }))
      .mockReturnValueOnce(of({ entries: [], latest_seq: 0, boot_id: 'a' }));
    component.loadLogs();
    component.loadLogs();
    expect(api.get).toHaveBeenNthCalledWith(2, '/logs', { after_seq: '0' });
    expect(component.entries().map((e) => e.seq)).toEqual([0]);
  });

  it('starts over when the server restarts with a new boot id (BUG-119)', () => {
    const { api, component } = setup();
    component.entries.set([]);
    api.get
      .mockReturnValueOnce(of({ entries: [line(40), line(41)], latest_seq: 41, boot_id: 'a' }))
      // After a restart the filtered poll (after_seq=41) sees nothing new...
      .mockReturnValueOnce(of({ entries: [], latest_seq: 3, boot_id: 'b' }))
      // ...so the view must refetch the new process's buffer from the start.
      .mockReturnValueOnce(of({ entries: [line(0, 'boot'), line(1), line(2), line(3)], latest_seq: 3, boot_id: 'b' }))
      .mockReturnValueOnce(of({ entries: [line(4)], latest_seq: 4, boot_id: 'b' }));
    component.loadLogs();
    component.loadLogs();
    expect(api.get).toHaveBeenNthCalledWith(3, '/logs', {});
    expect(component.entries().map((e) => e.seq)).toEqual([0, 1, 2, 3]);
    component.loadLogs();
    expect(api.get).toHaveBeenNthCalledWith(4, '/logs', { after_seq: '3' });
    expect(component.entries().map((e) => e.seq)).toEqual([0, 1, 2, 3, 4]);
  });

  it('detects a restart from a backwards latest_seq when no boot id is sent (BUG-119)', () => {
    const { api, component } = setup();
    component.entries.set([]);
    api.get
      .mockReturnValueOnce(of({ entries: [line(40)], latest_seq: 40 }))
      .mockReturnValueOnce(of({ entries: [], latest_seq: 2 }))
      .mockReturnValueOnce(of({ entries: [line(0), line(1), line(2)], latest_seq: 2 }));
    component.loadLogs();
    component.loadLogs();
    expect(api.get).toHaveBeenNthCalledWith(3, '/logs', {});
    expect(component.entries().map((e) => e.seq)).toEqual([0, 1, 2]);
  });
});
