import { Component } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { MatDialogModule, MatDialogRef } from '@angular/material/dialog';

/** Collects the web password for a sensitive write. Closes with the password or null. */
@Component({
  selector: 'app-step-up-dialog',
  standalone: true,
  imports: [FormsModule, MatDialogModule],
  template: `
    <h2 mat-dialog-title>Confirm your password</h2>
    <mat-dialog-content>
      <p class="message">This change needs your password again. It stays valid for a short time.</p>
      <label class="field">
        Password
        <input
          class="field-input"
          type="password"
          name="confirm-password"
          autocomplete="current-password"
          [(ngModel)]="password"
          (keyup.enter)="confirm()"
        />
      </label>
    </mat-dialog-content>
    <mat-dialog-actions align="end">
      <button class="btn" type="button" (click)="dialogRef.close(null)">Cancel</button>
      <button class="btn primary" type="button" [disabled]="!password" (click)="confirm()">
        Confirm
      </button>
    </mat-dialog-actions>
  `,
  styles: [
    `
      h2[mat-dialog-title] {
        color: var(--text);
        margin: 0 0 4px;
        font-size: 16px;
      }
      .message {
        margin: 0 0 12px;
        color: var(--text-dim);
      }
      .field {
        display: flex;
        flex-direction: column;
        gap: 4px;
        color: var(--text);
      }
      .field-input {
        background: var(--bg);
        color: var(--text);
        border: 1px solid var(--border);
        border-radius: 6px;
        padding: 8px 10px;
      }
      .field-input:focus-visible {
        outline: 2px solid var(--accent);
        outline-offset: 2px;
      }
      mat-dialog-actions {
        gap: 8px;
      }
    `,
  ],
})
export class StepUpDialogComponent {
  password = '';

  constructor(public dialogRef: MatDialogRef<StepUpDialogComponent, string | null>) {}

  confirm(): void {
    const password = this.password.trim();
    if (password) this.dialogRef.close(password);
  }
}
