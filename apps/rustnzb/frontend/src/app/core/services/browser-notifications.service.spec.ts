import { describe, expect, it } from 'vitest';
import { HistoryEntry } from '../models/queue.model';
import { outcomeNotices } from './browser-notifications.service';

function entry(id: string, status: string, extra: Partial<HistoryEntry> = {}): HistoryEntry {
  return { id, name: `${id}.nzb`, status, error_message: null, ...extra } as HistoryEntry;
}

describe('outcomeNotices', () => {
  it('announces unseen completed and failed rows oldest first', () => {
    // History is newest first.
    const entries = [
      entry('c2', 'completed'),
      entry('f1', 'failed', { error_message: 'Archive is corrupt' }),
      entry('c1', 'completed'),
    ];
    const notices = outcomeNotices(entries, new Set());

    expect(notices.map((n) => n.tag)).toEqual(['c1', 'f1', 'c2']);
    expect(notices[0]).toEqual({ tag: 'c1', title: 'Download complete', body: 'c1.nzb' });
    expect(notices[1]).toEqual({
      tag: 'f1',
      title: 'Download failed',
      body: 'f1.nzb: Archive is corrupt',
    });
  });

  it('does not repeat an outcome on a later check', () => {
    const seen = new Set<string>();
    const entries = [entry('c1', 'completed')];

    expect(outcomeNotices(entries, seen)).toHaveLength(1);
    expect(outcomeNotices(entries, seen)).toHaveLength(0);
    expect(seen.has('c1')).toBe(true);
  });

  it('marks baseline rows as seen without announcing a new one', () => {
    const seen = new Set<string>();
    const notices = outcomeNotices([entry('old', 'completed')], seen);
    expect(notices).toHaveLength(1);

    const next = outcomeNotices([entry('new', 'completed'), entry('old', 'completed')], seen);
    expect(next.map((n) => n.tag)).toEqual(['new']);
  });

  it('ignores rows that are neither completed nor failed', () => {
    const notices = outcomeNotices([entry('x', 'deleted')], new Set());
    expect(notices).toEqual([]);
  });
});
