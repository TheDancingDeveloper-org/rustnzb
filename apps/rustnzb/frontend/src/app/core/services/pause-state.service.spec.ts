import '@angular/compiler';

import { TestBed } from '@angular/core/testing';
import { describe, expect, it } from 'vitest';

import { PauseStateService } from './pause-state.service';

describe('PauseStateService', () => {
  it('starts unpaused and exposes changes through the paused signal', () => {
    const service = TestBed.inject(PauseStateService);
    expect(service.paused()).toBe(false);

    service.paused.set(true);
    expect(service.paused()).toBe(true);

    service.paused.set(false);
    expect(service.paused()).toBe(false);
  });
});
