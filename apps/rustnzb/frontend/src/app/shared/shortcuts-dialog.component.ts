import { Component } from '@angular/core';
import { MatDialogModule, MatDialogRef } from '@angular/material/dialog';
import { SHORTCUTS, ShortcutBinding } from '../core/services/keyboard-shortcuts.service';

/**
 * The `?` help sheet. Read-only, so it closes on Escape, the close button, or
 * a click outside — there is nothing to confirm.
 */
@Component({
  selector: 'app-shortcuts-dialog',
  standalone: true,
  imports: [MatDialogModule],
  template: `
    <h2 mat-dialog-title>Keyboard shortcuts</h2>
    <mat-dialog-content>
      @for (group of groups; track group) {
        <div class="group">
          <div class="group-title">{{ group }}</div>
          <dl>
            @for (binding of byGroup(group); track binding.keys) {
              <div class="row">
                <dt>
                  @for (key of binding.keys.split(' '); track $index) {
                    <kbd>{{ key }}</kbd>
                  }
                </dt>
                <dd>{{ binding.label }}</dd>
              </div>
            }
          </dl>
        </div>
      }
      <p class="hint">
        Shortcuts are ignored while you are typing in a field. A two-key chord
        like <kbd>g</kbd> <kbd>d</kbd> must be pressed within
        {{ chordHint }}.
      </p>
    </mat-dialog-content>
    <mat-dialog-actions align="end">
      <button class="btn" (click)="dialogRef.close()">Close</button>
    </mat-dialog-actions>
  `,
  styles: [
    `
      h2[mat-dialog-title] {
        color: var(--text);
        margin: 0 0 4px;
        font-size: 16px;
      }
      .group {
        margin-bottom: 16px;
      }
      .group-title {
        color: var(--mute);
        font-size: 12px;
        font-weight: 500;
        letter-spacing: 0.04em;
        text-transform: uppercase;
        margin-bottom: 6px;
      }
      dl {
        margin: 0;
      }
      .row {
        display: grid;
        grid-template-columns: 96px 1fr;
        align-items: baseline;
        gap: 12px;
        padding: 5px 0;
        border-bottom: 1px solid var(--line);
      }
      .row:last-child {
        border-bottom: none;
      }
      dt {
        display: flex;
        gap: 4px;
      }
      dd {
        margin: 0;
        color: var(--text);
        font-size: 13px;
      }
      kbd {
        background: var(--panel2);
        border: 1px solid var(--line-strong);
        border-radius: var(--radius-sm, 4px);
        color: var(--text);
        font-family: ui-monospace, Menlo, Consolas, monospace;
        font-size: 11px;
        line-height: 1;
        padding: 4px 6px;
        min-width: 16px;
        text-align: center;
      }
      .hint {
        color: var(--mute);
        font-size: 12px;
        line-height: 1.5;
        margin: 0;
        max-width: 360px;
      }
      mat-dialog-actions {
        padding-top: 12px;
      }
    `,
  ],
})
export class ShortcutsDialogComponent {
  readonly groups: ShortcutBinding['group'][] = ['Playback', 'Navigate', 'View'];
  readonly chordHint = '1.5 seconds';

  constructor(public dialogRef: MatDialogRef<ShortcutsDialogComponent>) {}

  byGroup(group: ShortcutBinding['group']): ShortcutBinding[] {
    return SHORTCUTS.filter((b) => b.group === group);
  }
}
