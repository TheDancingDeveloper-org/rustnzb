import { Injectable, signal } from '@angular/core';

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

  constructor() {
    this.apply(this.current());
  }

  set(theme: AppTheme): void {
    if (!this.options.some((option) => option.id === theme)) return;
    this.current.set(theme);
    localStorage.setItem(STORAGE_KEY, theme);
    this.apply(theme);
  }

  private readStoredTheme(): AppTheme {
    const stored = localStorage.getItem(STORAGE_KEY);
    return this.options.some((option) => option.id === stored)
      ? (stored as AppTheme)
      : 'rust-dark';
  }

  private apply(theme: AppTheme): void {
    document.body.dataset['theme'] = theme;
  }
}
