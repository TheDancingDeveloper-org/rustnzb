import { Injectable, signal } from '@angular/core';
import { systemTheme, watchSystemTheme } from '../system-preference';

export type AppTheme = 'rust-dark' | 'midnight' | 'light';

export interface ThemeOption {
  id: AppTheme;
  name: string;
  description: string;
}

const STORAGE_KEY = 'rustnzb-theme';

@Injectable({ providedIn: 'root' })
export class ThemeService {
  readonly options: readonly ThemeOption[] = [
    { id: 'rust-dark', name: 'Rust dark', description: 'The original charcoal and blue palette.' },
    { id: 'midnight', name: 'Midnight', description: 'Deep navy surfaces with violet highlights.' },
    { id: 'light', name: 'Daylight', description: 'A bright, high-contrast neutral palette.' },
  ];

  readonly current = signal<AppTheme>(this.readStoredTheme());
  /** True until the user picks a theme: the OS colour scheme decides it. */
  readonly followsSystem = signal(!this.hasStoredTheme());

  constructor() {
    this.apply(this.current());
    // A user who never chose a theme gets the OS setting as it changes,
    // including when the OS flips at sunset. An explicit choice wins.
    watchSystemTheme(() => {
      if (!this.followsSystem()) return;
      this.current.set(systemTheme());
      this.apply(this.current());
    });
  }

  /** Drop an explicit choice and go back to following the OS. */
  followSystem(): void {
    localStorage.removeItem(STORAGE_KEY);
    this.followsSystem.set(true);
    this.current.set(systemTheme());
    this.apply(this.current());
  }

  set(theme: AppTheme): void {
    if (!this.options.some((option) => option.id === theme)) return;
    this.followsSystem.set(false);
    this.current.set(theme);
    localStorage.setItem(STORAGE_KEY, theme);
    this.apply(theme);
  }

  private hasStoredTheme(): boolean {
    const stored = localStorage.getItem(STORAGE_KEY);
    return this.options.some((option) => option.id === stored);
  }

  private readStoredTheme(): AppTheme {
    const stored = localStorage.getItem(STORAGE_KEY);
    return this.options.some((option) => option.id === stored)
      ? (stored as AppTheme)
      : systemTheme();
  }

  private apply(theme: AppTheme): void {
    document.body.dataset['theme'] = theme;
    // Keep the browser chrome (address bar on mobile) on the theme's own
    // background instead of always painting the default dark colour.
    const meta = document.querySelector('meta[name="theme-color"]');
    const bg =
      typeof getComputedStyle === 'function'
        ? getComputedStyle(document.body).getPropertyValue('--bg').trim()
        : '';
    if (bg && meta) meta.setAttribute('content', bg);
  }
}
