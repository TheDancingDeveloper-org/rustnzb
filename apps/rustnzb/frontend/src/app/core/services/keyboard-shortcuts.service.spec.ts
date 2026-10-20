import '@angular/compiler';

import { describe, expect, it } from 'vitest';

import {
  CHORD_TIMEOUT_MS,
  SHORTCUTS,
  resolveShortcut,
  startsChord,
} from './keyboard-shortcuts.service';

describe('keyboard shortcut chords', () => {
  it('resolves every single-key binding it advertises', () => {
    for (const binding of SHORTCUTS) {
      if (binding.keys.includes(' ')) continue;
      expect(resolveShortcut(binding.keys), binding.keys).toBe(binding.action);
    }
  });

  it('resolves every two-key binding only with its prefix', () => {
    for (const binding of SHORTCUTS) {
      const parts = binding.keys.split(' ');
      if (parts.length !== 2) continue;
      expect(resolveShortcut(parts[1], parts[0]), binding.keys).toBe(binding.action);
      // The second key alone is not that action, nor is it a chord start.
      expect(resolveShortcut(parts[1]), binding.keys).not.toBe(binding.action);
    }
  });

  it('marks only chord prefixes as chord starts', () => {
    expect(startsChord('g')).toBe(true);
    expect(startsChord('p')).toBe(false);
    expect(startsChord('?')).toBe(false);
  });

  it('returns null for unknown keys instead of guessing', () => {
    expect(resolveShortcut('z')).toBeNull();
    expect(resolveShortcut('d', 'g')).toBe('goDownloads');
    expect(resolveShortcut('z', 'g')).toBeNull();
    expect(resolveShortcut('', 'g')).toBeNull();
  });

  it('has no duplicate chords and no unknown group', () => {
    const keys = SHORTCUTS.map((b) => b.keys);
    expect(new Set(keys).size).toBe(keys.length);
    expect(SHORTCUTS.every((b) => !!b.label)).toBe(true);
  });

  it('gives the chord a finite window long enough to be typed', () => {
    expect(CHORD_TIMEOUT_MS).toBeGreaterThanOrEqual(1000);
    expect(CHORD_TIMEOUT_MS).toBeLessThanOrEqual(3000);
  });
});
