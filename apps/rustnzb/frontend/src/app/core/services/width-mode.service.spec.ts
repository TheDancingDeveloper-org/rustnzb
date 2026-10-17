import '@angular/compiler';

import { beforeEach, describe, expect, it } from 'vitest';

import { WidthModeService } from './width-mode.service';

function setViewportWidth(width: number): void {
  Object.defineProperty(window, 'innerWidth', { configurable: true, writable: true, value: width });
}

describe('WidthModeService', () => {
  beforeEach(() => {
    localStorage.clear();
    delete document.body.dataset['widthMode'];
    setViewportWidth(1024);
  });

  it('picks expanded on wide viewports and compact on narrow ones when nothing is saved', () => {
    setViewportWidth(1920);
    expect(new WidthModeService().mode()).toBe('expanded');
    expect(document.body.getAttribute('data-width-mode')).toBe('expanded');

    setViewportWidth(1024);
    expect(new WidthModeService().mode()).toBe('compact');
    expect(document.body.getAttribute('data-width-mode')).toBe('compact');
  });

  it('restores a saved mode over the viewport heuristic', () => {
    localStorage.setItem('rustnzb.widthMode', 'compact');
    setViewportWidth(2560);
    expect(new WidthModeService().mode()).toBe('compact');
  });

  it('ignores an invalid saved value and falls back to the viewport heuristic', () => {
    localStorage.setItem('rustnzb.widthMode', 'huge');
    setViewportWidth(1920);
    expect(new WidthModeService().mode()).toBe('expanded');
  });

  it('persists set() and reflects it on the body attribute', () => {
    const service = new WidthModeService();
    service.set('expanded');
    expect(service.mode()).toBe('expanded');
    expect(localStorage.getItem('rustnzb.widthMode')).toBe('expanded');
    expect(document.body.getAttribute('data-width-mode')).toBe('expanded');
  });

  it('toggles between compact and expanded', () => {
    localStorage.setItem('rustnzb.widthMode', 'compact');
    const service = new WidthModeService();

    service.toggle();
    expect(service.mode()).toBe('expanded');
    expect(localStorage.getItem('rustnzb.widthMode')).toBe('expanded');

    service.toggle();
    expect(service.mode()).toBe('compact');
    expect(localStorage.getItem('rustnzb.widthMode')).toBe('compact');
  });
});
