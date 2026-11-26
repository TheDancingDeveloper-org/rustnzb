import { Component, inject } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { MAT_DIALOG_DATA, MatDialogModule, MatDialogRef } from '@angular/material/dialog';

export interface ArchivePasswordDialogData {
  title: string;
  message: string;
  confirmLabel: string;
  /** Whether an empty password is an answer (it clears the password). */
  allowEmpty?: boolean;
}

/** Collects an archive password. Closes with the password, or null when cancelled. */
@Component({
  selector: 'app-archive-password-dialog',
  standalone: true,
  imports: [FormsModule, MatDialogModule],
  template: `
    <h2 mat-dialog-title>{{ data.title }}</h2>
    <mat-dialog-content>
      <p class="message">{{ data.message }}</p>
      <label class="field">
        Archive password
        <input
          class="field-input"
          type="password"
          name="archive-password"
          autocomplete="off"
          [(ngModel)]="password"
          (keyup.enter)="confirm()"
        />
      </label>
    </mat-dialog-content>
    <mat-dialog-actions align="end">
      <button class="btn" type="button" (click)="dialogRef.close(null)">Cancel</button>
      <button class="btn primary" type="button" [disabled]="!password && !data.allowEmpty" (click)="confirm()">
        {{ data.confirmLabel }}
      </button>
    </mat-dialog-actions>
  `,
  styles: [
    `
      h2[mat-dialog-title] {
        color: var(--text);
        margin: 0 0 var(--sp-1);
        font-size: var(--fs-title);
      }
      .message {
        margin: 0 0 var(--sp-3);
        color: var(--text-dim);
      }
      .field {
        display: flex;
        flex-direction: column;
        gap: var(--sp-1);
        color: var(--text);
        /* Room for the focus ring, which the scrolling content would clip. */
        padding: var(--sp-1) var(--sp-1) var(--sp-2);
      }
      .field-input {
        background: var(--bg);
        color: var(--text);
        border: 1px solid var(--border);
        border-radius: var(--radius-md);
        padding: var(--sp-2) var(--sp-3);
      }
      .field-input:focus-visible {
        outline: 2px solid var(--accent);
        outline-offset: 2px;
      }
      mat-dialog-actions {
        gap: var(--sp-2);
      }
    `,
  ],
})
export class ArchivePasswordDialogComponent {
  readonly data = inject<ArchivePasswordDialogData>(MAT_DIALOG_DATA);
  password = '';

  constructor(public dialogRef: MatDialogRef<ArchivePasswordDialogComponent, string | null>) {}

  confirm(): void {
    // Not trimmed: spaces can be part of an archive password.
    if (this.password || this.data.allowEmpty) this.dialogRef.close(this.password);
  }
}
