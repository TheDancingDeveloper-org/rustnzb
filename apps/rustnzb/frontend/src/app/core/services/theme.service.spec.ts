import { beforeEach, describe, expect, it } from 'vitest';
import { ThemeService } from './theme.service';

describe('ThemeService', () => {
  beforeEach(() => {
    localStorage.clear();
    delete document.body.dataset['theme'];
  });

  it('applies the default theme and persists a selected additional theme', () => {
    const service = new ThemeService();
    expect(service.current()).toBe('rust-dark');
    expect(document.body.dataset['theme']).toBe('rust-dark');

    service.set('midnight');
    expect(service.current()).toBe('midnight');
    expect(localStorage.getItem('rustnzb-theme')).toBe('midnight');
    expect(document.body.dataset['theme']).toBe('midnight');
  });

  it('restores a saved theme on startup', () => {
    localStorage.setItem('rustnzb-theme', 'light');
    const service = new ThemeService();
    expect(service.current()).toBe('light');
    expect(document.body.dataset['theme']).toBe('light');
  });
});
