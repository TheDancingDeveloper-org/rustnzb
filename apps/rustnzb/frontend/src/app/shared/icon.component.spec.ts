import '@angular/compiler';

import { TestBed } from '@angular/core/testing';
import { describe, expect, it } from 'vitest';

import { IconComponent, IconName } from './icon.component';

describe('IconComponent', () => {
  function render(name: IconName, size?: number): HTMLElement {
    const fixture = TestBed.createComponent(IconComponent);
    fixture.componentRef.setInput('name', name);
    if (size !== undefined) fixture.componentRef.setInput('size', size);
    fixture.detectChanges();
    return fixture.nativeElement as HTMLElement;
  }

  it('renders an aria-hidden svg sized to the default 14px', () => {
    const root = render('close');
    const svg = root.querySelector('svg');
    expect(svg).not.toBeNull();
    expect(svg?.getAttribute('aria-hidden')).toBe('true');
    expect(svg?.style.width).toBe('14px');
    expect(svg?.style.height).toBe('14px');
  });

  it('honours a custom size', () => {
    const svg = render('play', 20).querySelector('svg');
    expect(svg?.style.width).toBe('20px');
    expect(svg?.style.height).toBe('20px');
  });

  it('draws a distinct glyph for every icon name', () => {
    const names: IconName[] = [
      'close',
      'play',
      'pause',
      'retry',
      'chevron-down',
      'chevron-right',
      'drag-handle',
    ];
    const markup = names.map((name) => render(name).querySelector('svg')?.innerHTML ?? '');
    for (const html of markup) expect(html.trim().length).toBeGreaterThan(0);
    expect(new Set(markup).size).toBe(names.length);
  });

  it('draws the six-dot grip for drag-handle', () => {
    const svg = render('drag-handle').querySelector('svg');
    expect(svg?.querySelectorAll('circle').length).toBe(6);
  });
});
