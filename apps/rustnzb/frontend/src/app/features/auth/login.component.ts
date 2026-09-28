import { Component, OnInit, signal } from '@angular/core';
import { CommonModule } from '@angular/common';
import { FormsModule } from '@angular/forms';
import { Router } from '@angular/router';
import { AuthService } from '../../core/services/auth.service';

@Component({
  selector: 'app-login',
  standalone: true,
  imports: [CommonModule, FormsModule],
  template: `
    <div class="login-wrapper">
      <div class="login-card">
        <div class="login-header">
          <img src="/logo.png" alt="rustnzb" class="login-logo" />
          @if (isSetup()) {
            <p class="login-subtitle">Create your account to get started</p>
          } @else {
            <p class="login-subtitle">Sign in to continue</p>
          }
        </div>

        @if (loading()) {
          <div class="login-loading">Checking status...</div>
        } @else {
          <form (ngSubmit)="onSubmit()" class="login-form">
            @if (errorMessage()) {
              <div class="login-error">{{ errorMessage() }}</div>
            }

            <div class="form-group">
              <label class="form-label" for="username">Username</label>
              <input
                id="username"
                type="text"
                class="form-input"
                [(ngModel)]="username"
                name="username"
                autocomplete="username"
                required
              />
            </div>

            <div class="form-group">
              <label class="form-label" for="password">Password</label>
              <input
                id="password"
                type="password"
                class="form-input"
                [(ngModel)]="password"
                name="password"
                autocomplete="current-password"
                required
              />
            </div>

            @if (isSetup()) {
              <div class="form-group">
                <label class="form-label" for="confirmPassword">Confirm Password</label>
                <input
                  id="confirmPassword"
                  type="password"
                  class="form-input"
                  [(ngModel)]="confirmPassword"
                  name="confirmPassword"
                  autocomplete="new-password"
                  required
                />
              </div>
            }

            <button type="submit" class="submit-btn" [disabled]="submitting()">
              @if (submitting()) {
                @if (isSetup()) { Creating Account... } @else { Signing In... }
              } @else {
                @if (isSetup()) { Create Account } @else { Sign In }
              }
            </button>
          </form>
        }
      </div>
    </div>
  `,
  styles: [`
    .login-wrapper {
      display: flex; align-items: center; justify-content: center;
      min-height: 100vh; background: var(--bg); padding: 16px;
    }

    .login-card {
      width: 100%; max-width: 400px; background: var(--panel);
      border: 1px solid var(--line); border-radius: 8px; padding: 32px;
    }

    .login-header { text-align: center; margin-bottom: 24px; }

    .login-logo { width: 220px; height: auto; }

    .login-subtitle {
      color: var(--mute); font-size: 14px; margin: 8px 0 0;
    }

    .login-loading {
      text-align: center; color: var(--mute); padding: 24px 0;
    }

    .login-form { display: flex; flex-direction: column; gap: 16px; }

    .login-error {
      background: color-mix(in srgb, var(--danger) 10%, transparent);
      border: 1px solid var(--danger);
      border-radius: 6px; padding: 10px 14px; color: var(--danger);
      font-size: 13px;
    }

    .form-group { display: flex; flex-direction: column; gap: 6px; }

    .form-label { color: var(--text); font-size: 13px; font-weight: 600; }

    .form-input {
      background: var(--panel2); border: 1px solid var(--line); border-radius: 6px;
      padding: 8px 12px; color: var(--text); font-size: 14px;
      outline: none; transition: border-color 0.15s ease;
    }
    .form-input:focus { border-color: var(--accent); }
    .form-input::placeholder { color: var(--mute); opacity: 0.7; }

    .submit-btn {
      background: var(--accent); border: 1px solid var(--accent); border-radius: 6px;
      padding: 10px 16px; color: #ffffff; font-size: 14px; font-weight: 600;
      cursor: pointer; transition: background 0.15s ease; margin-top: 4px;
    }
    .submit-btn:hover:not(:disabled) { background: #2563eb; border-color: #2563eb; }
    .submit-btn:disabled { opacity: 0.6; cursor: not-allowed; }
  `],
})
export class LoginComponent implements OnInit {
  username = '';
  password = '';
  confirmPassword = '';

  isSetup = signal(false);
  loading = signal(true);
  submitting = signal(false);
  errorMessage = signal('');

  constructor(
    private authService: AuthService,
    private router: Router,
  ) {}

  ngOnInit(): void {
    // Only a session the server accepts skips the form. A stale stored token
    // is cleared here; redirecting on it would bounce straight back to this
    // already-active route and leave the form stuck on "Checking status...".
    this.authService.ensureSession().subscribe((ok) => {
      if (ok) {
        this.router.navigate(['/downloads']);
      } else {
        this.loadAuthStatus();
      }
    });
  }

  private loadAuthStatus(): void {
    this.authService.checkAuth().subscribe({
      next: (status) => {
        if (!status.auth_enabled && !status.setup_required) {
          // Auth is disabled, go straight to downloads
          this.router.navigate(['/downloads']);
          return;
        }
        this.isSetup.set(status.setup_required);
        this.loading.set(false);
      },
      error: () => {
        // If we can't reach the server, show login anyway
        this.loading.set(false);
      },
    });
  }

  onSubmit(): void {
    this.errorMessage.set('');

    if (!this.username.trim() || !this.password.trim()) {
      this.errorMessage.set('Username and password are required.');
      return;
    }

    if (this.isSetup() && this.password !== this.confirmPassword) {
      this.errorMessage.set('Passwords do not match.');
      return;
    }

    this.submitting.set(true);

    const request$ = this.isSetup()
      ? this.authService.setup(this.username, this.password)
      : this.authService.login(this.username, this.password);

    const isSetup = this.isSetup();
    request$.subscribe({
      next: () => {
        this.router.navigate(isSetup ? ['/welcome'] : ['/downloads']);
      },
      error: (err) => {
        this.submitting.set(false);
        if (err.status === 401) {
          this.errorMessage.set('Invalid username or password.');
        } else if (err.status === 409) {
          this.errorMessage.set('An account already exists. Please sign in instead.');
        } else if (err.error?.message) {
          this.errorMessage.set(err.error.message);
        } else {
          this.errorMessage.set('An error occurred. Please try again.');
        }
      },
    });
  }
}
