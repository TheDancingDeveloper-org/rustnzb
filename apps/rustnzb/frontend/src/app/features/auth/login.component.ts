import { Component, OnInit, signal } from '@angular/core';
import { CommonModule } from '@angular/common';
import { FormsModule } from '@angular/forms';
import { Router } from '@angular/router';
import { AuthService } from '../../core/services/auth.service';
import { httpErrorDetail } from '../../core/http/http-error';

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
            <p class="login-subtitle">Set up this install: an admin account, folders and one server</p>
          } @else {
            <p class="login-subtitle">Sign in to continue</p>
          }
        </div>

        @if (loading()) {
          <div class="login-loading">Checking status...</div>
        } @else {
          <form (ngSubmit)="onSubmit()" class="login-form">
            @if (errorMessage()) {
              <div class="login-error" role="alert" id="login-error">{{ errorMessage() }}</div>
            }

            <div class="form-group">
              <label class="form-label" for="username">Username</label>
              <input
                id="username"
                [attr.aria-describedby]="errorMessage() ? 'login-error' : null"
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
                [attr.aria-describedby]="errorMessage() ? 'login-error' : null"
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
                <label class="form-label" for="incompleteDir">Incomplete folder</label>
                <input id="incompleteDir" type="text" class="form-input" [(ngModel)]="incompleteDir" name="incompleteDir" required />
              </div>
              <div class="form-group">
                <label class="form-label" for="completeDir">Complete folder</label>
                <input id="completeDir" type="text" class="form-input" [(ngModel)]="completeDir" name="completeDir" required />
              </div>
              <div class="form-group">
                <label class="form-label" for="serverHost">Usenet server</label>
                <input id="serverHost" type="text" class="form-input" [(ngModel)]="serverHost" name="serverHost" placeholder="news.example.com" required />
              </div>
              <div class="form-group">
                <label class="form-label" for="confirmPassword">Confirm Password</label>
                <input
                  id="confirmPassword"
                [attr.aria-describedby]="errorMessage() ? 'login-error' : null"
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
      background: var(--accent-fill); border: 1px solid var(--accent-fill); border-radius: 6px;
      padding: 10px 16px; color: #ffffff; font-size: 14px; font-weight: 600;
      cursor: pointer; transition: background 0.15s ease; margin-top: 4px;
    }
    .submit-btn:hover:not(:disabled) { background: var(--accent-fill-hover); border-color: var(--accent-fill-hover); }
    .submit-btn:disabled { opacity: 0.6; cursor: not-allowed; }
  `],
})
export class LoginComponent implements OnInit {
  username = '';
  password = '';
  confirmPassword = '';
  incompleteDir = '';
  completeDir = '';
  serverHost = '';

  isSetup = signal(false);
  firstRun = signal(false);
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
        if (status.setup_required) {
          this.authService.setupStatus().subscribe({
            next: (setup) => {
              // A container, or an install that already has a server, keeps
              // the account-only setup. First-run is the non-Docker path.
              this.firstRun.set(!!setup.first_run);
              this.loading.set(false);
            },
            error: () => this.loading.set(false),
          });
          return;
        }
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

    if (this.firstRun() && (!this.incompleteDir.trim() || !this.completeDir.trim() || !this.serverHost.trim())) {
      this.errorMessage.set('Folders and a server host are required.');
      return;
    }

    this.submitting.set(true);

    const request$ = this.firstRun()
      ? this.authService.firstRun({
          username: this.username,
          password: this.password,
          incomplete_dir: this.incompleteDir.trim(),
          complete_dir: this.completeDir.trim(),
          server_host: this.serverHost.trim(),
        })
      : this.isSetup()
        ? this.authService.setup(this.username, this.password)
        : this.authService.login(this.username, this.password);

    const isSetup = this.isSetup();
    request$.subscribe({
      next: () => {
        this.router.navigate(isSetup && !this.firstRun() ? ['/welcome'] : ['/downloads']);
      },
      error: (err) => {
        this.submitting.set(false);
        if (err.status === 401) {
          this.errorMessage.set('Invalid username or password.');
        } else if (err.status === 409) {
          this.errorMessage.set('An account already exists. Please sign in instead.');
        } else {
          this.errorMessage.set(httpErrorDetail(err) ?? 'An error occurred. Please try again.');
        }
      },
    });
  }
}
