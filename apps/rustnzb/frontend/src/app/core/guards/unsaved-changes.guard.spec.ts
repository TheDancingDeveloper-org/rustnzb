import '@angular/compiler';

import { describe, expect, it, vi } from 'vitest';
import { Injector, runInInjectionContext } from '@angular/core';
import { firstValueFrom, isObservable, of } from 'rxjs';

import { ConfirmService } from '../../shared/confirm.service';
import { unsavedChangesGuard } from './unsaved-changes.guard';

function run(dirty: boolean, answer: boolean) {
  const confirm = { confirm: vi.fn(() => of(answer)) };
  const injector = Injector.create({ providers: [{ provide: ConfirmService, useValue: confirm }] });
  const result = runInInjectionContext(injector, () =>
    unsavedChangesGuard({ hasUnsavedChanges: () => dirty }, {} as never, {} as never, {} as never),
  );
  return { result, confirm };
}

describe('unsavedChangesGuard', () => {
  it('lets a clean view go without asking', () => {
    const { result, confirm } = run(false, false);
    expect(result).toBe(true);
    expect(confirm.confirm).not.toHaveBeenCalled();
  });

  it('asks before leaving a dirty view and follows the answer', async () => {
    for (const answer of [true, false]) {
      const { result, confirm } = run(true, answer);
      expect(confirm.confirm).toHaveBeenCalledOnce();
      expect(isObservable(result)).toBe(true);
      expect(await firstValueFrom(result as never)).toBe(answer);
    }
  });
});
