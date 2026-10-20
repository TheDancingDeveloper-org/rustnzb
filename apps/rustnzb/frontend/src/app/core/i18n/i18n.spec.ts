import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { beforeEach, describe, expect, it } from 'vitest';
import { EN } from './en';
import { I18nService, LOCALES, interpolate, translate } from './i18n.service';

// Specs run from the frontend directory.
const appDir = resolve('src/app');

function sourceFiles(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return sourceFiles(path);
    return name.endsWith('.ts') && !name.endsWith('.spec.ts') ? [path] : [];
  });
}

/** Literal keys used through the `t` pipe or a `t(...)` / `.t(...)` call. */
function usedKeys(source: string): string[] {
  const keys: string[] = [];
  const patterns = [
    /'([a-z][\w]*(?:\.[\w]+)+)'\s*\|\s*t\b/g,
    /\bt\(\s*'([a-z][\w]*(?:\.[\w]+)+)'/g,
  ];
  for (const pattern of patterns) {
    for (const match of source.matchAll(pattern)) keys.push(match[1]);
  }
  return keys;
}

const placeholders = (text: string) => [...text.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort();

describe('i18n catalogs', () => {
  it('every key used in the app exists in the English catalog', () => {
    const missing: string[] = [];
    for (const file of sourceFiles(appDir)) {
      for (const key of usedKeys(readFileSync(file, 'utf8'))) {
        if (!(key in EN)) missing.push(`${file.slice(appDir.length + 1)}: ${key}`);
      }
    }
    expect(missing).toEqual([]);
  });

  it('finds keys in the app shell, so the scan itself is working', () => {
    const shell = usedKeys(readFileSync(join(appDir, 'app.ts'), 'utf8'));
    expect(shell).toContain('nav.downloads');
    expect(shell).toContain('status.queued');
  });

  it('every locale defines exactly the English keys with the same placeholders', () => {
    for (const locale of LOCALES) {
      const catalog: Record<string, string> = locale.catalog;
      expect(Object.keys(catalog).sort(), locale.id).toEqual(Object.keys(EN).sort());
      for (const [key, text] of Object.entries(EN)) {
        expect(placeholders(catalog[key]), `${locale.id} ${key}`).toEqual(placeholders(text));
      }
    }
  });

  it('has no empty strings', () => {
    for (const locale of LOCALES) {
      for (const [key, text] of Object.entries(locale.catalog)) {
        expect(text.trim(), `${locale.id} ${key}`).not.toBe('');
      }
    }
  });
});

describe('translate', () => {
  it('interpolates named parameters and leaves unknown ones as written', () => {
    expect(interpolate('{count} queued', { count: 3 })).toBe('3 queued');
    expect(interpolate('{a} and {b}', { a: 'x' })).toBe('x and {b}');
  });

  it('falls back to English for a key a catalog lacks', () => {
    expect(translate({}, 'nav.history')).toBe('History');
  });
});

describe('I18nService', () => {
  beforeEach(() => localStorage.clear());

  it('defaults to English and sets the document language', () => {
    const service = new I18nService();
    expect(service.locale()).toBe('en');
    expect(document.documentElement.lang).toBe('en');
    expect(service.t('status.free', { size: '1 GB' })).toBe('1 GB free');
  });

  it('persists a known locale and ignores an unknown one', () => {
    const service = new I18nService();
    service.set('xx');
    expect(service.locale()).toBe('en');
    expect(localStorage.getItem('rustnzb-locale')).toBeNull();
    service.set('en');
    expect(localStorage.getItem('rustnzb-locale')).toBe('en');
  });

  it('ignores a stored locale that is not shipped', () => {
    localStorage.setItem('rustnzb-locale', 'xx');
    expect(new I18nService().locale()).toBe('en');
  });
});
