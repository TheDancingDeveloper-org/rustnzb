import '@angular/compiler';

import { TestBed } from '@angular/core/testing';
import { describe, expect, it, vi } from 'vitest';

import { AddNzbService } from './add-nzb.service';

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
