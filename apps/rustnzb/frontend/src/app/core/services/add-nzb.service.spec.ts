import '@angular/compiler';

import { TestBed } from '@angular/core/testing';
import { describe, expect, it, vi } from 'vitest';

import {
  AddNzbService,
  isEditableTarget,
  isNzbUploadName,
  nzbUploadFiles,
  pastedNzbUrl,
} from './add-nzb.service';

describe('AddNzbService', () => {
  it('emits on panelToggle$ each time togglePanel is called', () => {
    const service = TestBed.inject(AddNzbService);
    const next = vi.fn();
    const sub = service.panelToggle$.subscribe(next);

    service.togglePanel();
    service.togglePanel();
    expect(next).toHaveBeenCalledTimes(2);

    sub.unsubscribe();
    service.togglePanel();
    expect(next).toHaveBeenCalledTimes(2);
  });
});

describe('AddNzbService.prefill', () => {
  it('emits the prefill to subscribers', () => {
    const service = TestBed.inject(AddNzbService);
    const next = vi.fn();
    const sub = service.prefill$.subscribe(next);
    service.prefill({ url: 'https://x/a.nzb' });
    expect(next).toHaveBeenCalledWith({ url: 'https://x/a.nzb' });
    sub.unsubscribe();
  });
});

describe('nzbUploadFiles', () => {
  const file = (name: string) => new File(['x'], name);

  it('keeps upload-able extensions, case-insensitively', () => {
    const got = nzbUploadFiles([
      file('a.nzb'),
      file('B.NZB'),
      file('c.nzb.gz'),
      file('d.zip'),
      file('e.txt'),
    ]);
    expect(got.map((f) => f.name)).toEqual(['a.nzb', 'B.NZB', 'c.nzb.gz', 'd.zip']);
  });

  it('handles a missing file list', () => {
    expect(nzbUploadFiles(null)).toEqual([]);
    expect(isNzbUploadName('movie.mkv')).toBe(false);
  });
});

describe('pastedNzbUrl', () => {
  it('accepts a single http(s) URL, trimmed', () => {
    expect(pastedNzbUrl('  https://idx.example/get?id=1 \n')).toBe('https://idx.example/get?id=1');
    expect(pastedNzbUrl('http://idx/a.nzb')).toBe('http://idx/a.nzb');
  });

  it('rejects other text', () => {
    expect(pastedNzbUrl('')).toBeNull();
    expect(pastedNzbUrl(null)).toBeNull();
    expect(pastedNzbUrl('hello world')).toBeNull();
    expect(pastedNzbUrl('ftp://x/a.nzb')).toBeNull();
    expect(pastedNzbUrl('javascript:alert(1)')).toBeNull();
    expect(pastedNzbUrl('https://a https://b')).toBeNull();
  });
});

describe('isEditableTarget', () => {
  it('detects inputs, textareas and contenteditable', () => {
    const input = document.createElement('input');
    const area = document.createElement('textarea');
    const div = document.createElement('div');
    const editable = document.createElement('div');
    editable.setAttribute('contenteditable', 'true');
    const inner = document.createElement('span');
    editable.appendChild(inner);
    expect(isEditableTarget(input)).toBe(true);
    expect(isEditableTarget(area)).toBe(true);
    expect(isEditableTarget(inner)).toBe(true);
    expect(isEditableTarget(div)).toBe(false);
    expect(isEditableTarget(null)).toBe(false);
  });
});
