import '@angular/compiler';

import { Router } from '@angular/router';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { of, throwError } from 'rxjs';

import { AuthService, TokenResponse } from '../../core/services/auth.service';
import { LoginComponent } from './login.component';

const TOKENS: TokenResponse = {
  access_token: 'access',
  refresh_token: 'refresh',
  token_type: 'Bearer',
  expires_in: 3600,
};

describe('LoginComponent', () => {
  let auth: {
    ensureSession: ReturnType<typeof vi.fn>;
    checkAuth: ReturnType<typeof vi.fn>;
    setup: ReturnType<typeof vi.fn>;
    setupStatus: ReturnType<typeof vi.fn>;
    firstRun: ReturnType<typeof vi.fn>;
    login: ReturnType<typeof vi.fn>;
  };
  let router: { navigate: ReturnType<typeof vi.fn> };
  let component: LoginComponent;

  beforeEach(() => {
    auth = {
      ensureSession: vi.fn(() => of(false)),
      checkAuth: vi.fn(() => of({ auth_enabled: true, setup_required: false })),
      setup: vi.fn(() => of(TOKENS)),
      setupStatus: vi.fn(() => of({ needs_setup: true, first_run: false, container: false })),
      firstRun: vi.fn(() => of(TOKENS)),
      login: vi.fn(() => of(TOKENS)),
    };
    router = { navigate: vi.fn(() => Promise.resolve(true)) };
    component = new LoginComponent(
      auth as unknown as AuthService,
      router as unknown as Router,
    );
  });

  it('redirects a server-confirmed session without checking server auth state', () => {
    auth.ensureSession.mockReturnValue(of(true));
    component.ngOnInit();
    expect(router.navigate).toHaveBeenCalledWith(['/downloads']);
    expect(auth.checkAuth).not.toHaveBeenCalled();
  });

  it('enters account setup mode when the server requires it', () => {
    auth.checkAuth.mockReturnValue(of({ auth_enabled: true, setup_required: true }));
    component.ngOnInit();
    expect(component.isSetup()).toBe(true);
    expect(component.firstRun()).toBe(false);
    expect(component.loading()).toBe(false);
  });

  it('shows the first-run fields when setup status says so', () => {
    auth.checkAuth.mockReturnValue(of({ auth_enabled: true, setup_required: true }));
    auth.setupStatus.mockReturnValue(of({ needs_setup: true, first_run: true, container: false }));
    component.ngOnInit();
    expect(component.firstRun()).toBe(true);
    expect(component.loading()).toBe(false);
  });

  it('posts paths and a server on first-run submit', () => {
    component.firstRun.set(true);
    component.isSetup.set(true);
    component.username = 'admin';
    component.password = 'secret';
    component.confirmPassword = 'secret';
    component.incompleteDir = '/data/incomplete';
    component.completeDir = '/data/complete';
    component.serverHost = 'news.example.com';
    component.onSubmit();
    expect(auth.firstRun).toHaveBeenCalledWith({
      username: 'admin',
      password: 'secret',
      incomplete_dir: '/data/incomplete',
      complete_dir: '/data/complete',
      server_host: 'news.example.com',
    });
    expect(auth.setup).not.toHaveBeenCalled();
    expect(router.navigate).toHaveBeenCalledWith(['/downloads']);
  });

  it('bypasses login when authentication is disabled', () => {
    auth.checkAuth.mockReturnValue(of({ auth_enabled: false, setup_required: false }));
    component.ngOnInit();
    expect(router.navigate).toHaveBeenCalledWith(['/downloads']);
  });

  it('validates required credentials and setup password confirmation', () => {
    component.onSubmit();
    expect(component.errorMessage()).toBe('Username and password are required.');
    component.username = 'alice';
    component.password = 'one';
    component.confirmPassword = 'two';
    component.isSetup.set(true);
    component.onSubmit();
    expect(component.errorMessage()).toBe('Passwords do not match.');
    expect(auth.setup).not.toHaveBeenCalled();
  });

  it.each([
    [false, 'login', '/downloads'],
    [true, 'setup', '/welcome'],
  ] as const)('submits %s mode and navigates to its landing page', (setup, method, target) => {
    component.username = 'alice';
    component.password = 'secret';
    component.confirmPassword = 'secret';
    component.isSetup.set(setup);
    component.onSubmit();
    expect(auth[method]).toHaveBeenCalledWith('alice', 'secret');
    expect(router.navigate).toHaveBeenCalledWith([target]);
  });

  it.each([
    [401, undefined, 'Invalid username or password.'],
    [409, undefined, 'An account already exists. Please sign in instead.'],
    [500, 'Backend exploded', 'Backend exploded'],
  ])('maps HTTP %i failures to actionable feedback', (status, message, expected) => {
    auth.login.mockReturnValue(throwError(() => ({ status, error: { message } })));
    component.username = 'alice';
    component.password = 'secret';
    component.onSubmit();
    expect(component.submitting()).toBe(false);
    expect(component.errorMessage()).toBe(expected);
  });
});
