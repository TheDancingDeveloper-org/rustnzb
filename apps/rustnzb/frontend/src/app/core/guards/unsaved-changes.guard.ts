import { CanDeactivateFn } from '@angular/router';
import { inject } from '@angular/core';
import { ConfirmService } from '../../shared/confirm.service';

/** A routed view that can hold edits the user has not saved yet. */
export interface HasUnsavedChanges {
  hasUnsavedChanges(): boolean;
}

// Ask before leaving a view with unsaved edits, so a stray click on the nav
// doesn't silently throw them away.
export const unsavedChangesGuard: CanDeactivateFn<HasUnsavedChanges> = (component) => {
  if (!component?.hasUnsavedChanges()) return true;
  return inject(ConfirmService).confirm({
    title: 'Discard unsaved changes?',
    message: 'Some settings on this page have not been saved. Leaving now discards them.',
    confirmLabel: 'Discard',
    cancelLabel: 'Keep editing',
    danger: true,
  });
};
