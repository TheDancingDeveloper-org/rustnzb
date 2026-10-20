import { Injectable } from '@angular/core';
import { Subject } from 'rxjs';

// Same list as the upload panel's file input `accept`.
export const NZB_UPLOAD_EXTENSIONS = ['.nzb', '.zip', '.rar', '.7z', '.gz'];

export function isNzbUploadName(name: string): boolean {
  const lower = name.toLowerCase();
  return NZB_UPLOAD_EXTENSIONS.some((ext) => lower.endsWith(ext));
}

/** The files a drop carries that the upload endpoint takes. */
export function nzbUploadFiles(files: ArrayLike<File> | null | undefined): File[] {
  return files ? Array.from(files).filter((f) => isNzbUploadName(f.name)) : [];
}

/** Pasted text, if it is one http(s) URL; otherwise null. */
export function pastedNzbUrl(text: string | null | undefined): string | null {
  const trimmed = (text ?? '').trim();
  if (!trimmed || /\s/.test(trimmed)) return null;
  try {
    const url = new URL(trimmed);
    return url.protocol === 'http:' || url.protocol === 'https:' ? trimmed : null;
  } catch {
    return null;
  }
}

/** True when a paste lands somewhere the user is typing. */
export function isEditableTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  if (target.isContentEditable) return true;
  return !!target.closest(
    'input, textarea, select, [contenteditable=""], [contenteditable="true"]',
  );
}

/** Files or a URL handed to the Add NZB panel from outside it. */
export type AddNzbPrefill = { files: File[] } | { url: string };

@Injectable({ providedIn: 'root' })
export class AddNzbService {
  private toggle$ = new Subject<void>();
  panelToggle$ = this.toggle$.asObservable();
  private prefillSubject = new Subject<AddNzbPrefill>();
  prefill$ = this.prefillSubject.asObservable();

  togglePanel(): void {
    this.toggle$.next();
  }

  /** Open the panel with these files or this URL filled in. */
  prefill(value: AddNzbPrefill): void {
    this.prefillSubject.next(value);
  }
}
