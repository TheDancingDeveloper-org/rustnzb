import type { AppTheme } from './services/theme.service';

/** The OS-level colour scheme, as a theme id. Dark when it cannot be read. */
export function systemTheme(): AppTheme {
  return prefersLight() ? 'light' : 'rust-dark';
}

export function prefersLight(): boolean {
  return matches('(prefers-color-scheme: light)');
}

export function prefersReducedMotion(): boolean {
  return matches('(prefers-reduced-motion: reduce)');
}

function matches(query: string): boolean {
  if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') return false;
  return window.matchMedia(query).matches;
}

/**
 * Call `onChange` whenever the OS colour scheme changes. No-op where
 * matchMedia is unavailable (jsdom, older browsers).
 */
export function watchSystemTheme(onChange: () => void): void {
  if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') return;
  const media = window.matchMedia('(prefers-color-scheme: light)');
  media.addEventListener?.('change', onChange);
}
