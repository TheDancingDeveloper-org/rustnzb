import { Injectable } from '@angular/core';
import { Subject } from 'rxjs';

/**
 * Every global shortcut, in the order the help overlay lists them. The chord
 * strings here are the single source of truth: the matcher, the chord timeout
 * and the help table all read them, so they cannot disagree.
 */
export type ShortcutAction =
  | 'togglePause'
  | 'upload'
  | 'focusSearch'
  | 'goDownloads'
  | 'goHistory'
  | 'goSettings'
  | 'toggleLayout'
  | 'showHelp';

export interface ShortcutBinding {
  /** Display and match form, e.g. "g d" or "/". */
  keys: string;
  action: ShortcutAction;
  label: string;
  group: 'Playback' | 'Navigate' | 'View';
}

export const SHORTCUTS: readonly ShortcutBinding[] = [
  { keys: 'p', action: 'togglePause', label: 'Pause or resume all downloads', group: 'Playback' },
  { keys: 'u', action: 'upload', label: 'Add an NZB', group: 'Playback' },
  { keys: '/', action: 'focusSearch', label: 'Search Usenet headers', group: 'Playback' },
  { keys: 'g d', action: 'goDownloads', label: 'Go to Downloads', group: 'Navigate' },
  { keys: 'g h', action: 'goHistory', label: 'Go to History', group: 'Navigate' },
  { keys: 'g s', action: 'goSettings', label: 'Go to Settings', group: 'Navigate' },
  { keys: 'w', action: 'toggleLayout', label: 'Toggle compact / expanded width', group: 'View' },
  { keys: '?', action: 'showHelp', label: 'Show this list', group: 'View' },
];

/** How long a leading `g` waits for its second key before it is dropped. */
export const CHORD_TIMEOUT_MS = 1500;

/**
 * The action a keydown means, given a pending chord prefix.
 *
 * `key` is the printable form of `KeyboardEvent.key`, lowercased by the
 * caller (`'d'`, `'/'`, `'?'`, `'G'` -> `'g'`). Returns `null` when nothing
 * matches, so the caller can leave the event alone.
 */
export function resolveShortcut(key: string, chordPrefix = ''): ShortcutAction | null {
  if (!key) return null;
  if (chordPrefix) {
    return (
      SHORTCUTS.find((b) => {
        const parts = b.keys.split(' ');
        return parts.length === 2 && parts[0] === chordPrefix && parts[1] === key;
      })?.action ?? null
    );
  }
  return SHORTCUTS.find((b) => !b.keys.includes(' ') && b.keys === key)?.action ?? null;
}

/** True for the keys that start a two-key chord. */
export function startsChord(key: string): boolean {
  return SHORTCUTS.some((b) => {
    const parts = b.keys.split(' ');
    return parts.length === 2 && parts[0] === key;
  });
}

/**
 * Cross-component requests that a key press produces but cannot fulfil alone:
 * `/` focuses a search box that belongs to another page.
 */
@Injectable({ providedIn: 'root' })
export class KeyboardShortcutsService {
  private readonly searchFocusSubject = new Subject<void>();
  /** Emits when the search box should take focus (it may have just mounted). */
  readonly searchFocus$ = this.searchFocusSubject.asObservable();
  // A lazy route can mount its page after the request is made, so a request
  // also waits to be consumed by whichever search box appears next.
  private pendingSearchFocus = false;

  requestSearchFocus(): void {
    this.pendingSearchFocus = true;
    this.searchFocusSubject.next();
  }

  /** True once per outstanding request; clears it. */
  consumeSearchFocus(): boolean {
    const pending = this.pendingSearchFocus;
    this.pendingSearchFocus = false;
    return pending;
  }
}
