import { Injectable, signal } from '@angular/core';
import { Catalog, EN, TranslationKey } from './en';

export interface LocaleOption {
  id: string;
  /** The language's own name for itself, so it is findable whatever the current locale. */
  name: string;
  catalog: Catalog;
}

/** Shipped locales. Add a catalog here; i18n.spec.ts checks it has every key. */
export const LOCALES: readonly LocaleOption[] = [{ id: 'en', name: 'English', catalog: EN }];

export const DEFAULT_LOCALE = 'en';
const STORAGE_KEY = 'rustnzb-locale';

export type TranslateParams = Record<string, string | number>;

/** Fill `{name}` placeholders; an unknown placeholder is left as written. */
export function interpolate(text: string, params?: TranslateParams): string {
  if (!params) return text;
  return text.replace(/\{(\w+)\}/g, (match, name: string) =>
    name in params ? String(params[name]) : match,
  );
}

/** Translate against a catalog, falling back to English and then to the key itself. */
export function translate(
  catalog: Partial<Catalog>,
  key: TranslationKey,
  params?: TranslateParams,
): string {
  return interpolate(catalog[key] ?? EN[key] ?? key, params);
}

export type Translate = (key: TranslationKey, params?: TranslateParams) => string;

export const translateEn: Translate = (key, params) => translate(EN, key, params);

@Injectable({ providedIn: 'root' })
export class I18nService {
  readonly locales = LOCALES;
  readonly locale = signal<string>(this.readStoredLocale());

  constructor() {
    this.applyLang(this.locale());
  }

  set(locale: string): void {
    if (!LOCALES.some((option) => option.id === locale)) return;
    this.locale.set(locale);
    try {
      localStorage.setItem(STORAGE_KEY, locale);
    } catch {
      // Storage blocked: the choice still applies for this page load.
    }
    this.applyLang(locale);
  }

  readonly t: Translate = (key, params) => translate(this.catalog(), key, params);

  private catalog(): Catalog {
    const id = this.locale();
    return (LOCALES.find((option) => option.id === id) ?? LOCALES[0]).catalog;
  }

  private readStoredLocale(): string {
    let stored: string | null = null;
    try {
      stored = localStorage.getItem(STORAGE_KEY);
    } catch {
      stored = null;
    }
    return LOCALES.some((option) => option.id === stored) ? (stored as string) : DEFAULT_LOCALE;
  }

  private applyLang(locale: string): void {
    document.documentElement.lang = locale;
  }
}
