import '@angular/compiler';

import { HttpErrorResponse, HttpRequest, HttpResponse } from '@angular/common/http';
import { TestBed } from '@angular/core/testing';
import { Router } from '@angular/router';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { firstValueFrom, of, throwError } from 'rxjs';
import { MatDialog } from '@angular/material/dialog';

import { AuthService } from '../services/auth.service';
import { StepUpDialogComponent } from '../../shared/step-up-dialog.component';
import { authInterceptor } from './auth.interceptor';

function setup(token = 'access-1') {
  const auth = {
    getAccessToken: vi.fn(() => token),
    refresh: vi.fn(() => of({ access_token: 'access-2' })),
    clearTokens: vi.fn(),
    discardFailedSession: vi.fn(() => true),
    markSessionVerified: vi.fn(),
  };
  const router = { navigate: vi.fn() };
  const dialog = {
    open: vi.fn(() => ({ afterClosed: () => of('web-password') })),
  };
  TestBed.configureTestingModule({
    providers: [
      { provide: AuthService, useValue: auth },
      { provide: Router, useValue: router },
      { provide: MatDialog, useValue: dialog },
    ],
  });
  const run = (req: HttpRequest<unknown>, next: (r: HttpRequest<unknown>) => unknown) =>
    firstValueFrom(
      TestBed.runInInjectionContext(() => authInterceptor(req, next as never)),
    );
  return { auth, router, dialog, run };

  it('prompts and retries once when a write needs step-up', async () => {
    const { auth, dialog, run } = setup();
    const stepUp = new HttpErrorResponse({
      status: 403,
      url: '/api/config/general',
      error: { error_kind: 'step_up_required' },
    });
    const next = vi
      .fn()
      .mockReturnValueOnce(throwError(() => stepUp))
      .mockReturnValueOnce(of(new HttpResponse({ status: 200 })));

    await run(new HttpRequest('PUT', '/api/config/general', { port: 9091 }), next);

    expect(auth.refresh).not.toHaveBeenCalled();
    expect(dialog.open).toHaveBeenCalledWith(
      StepUpDialogComponent,
      expect.objectContaining({ width: '380px' }),
    );
    expect(next).toHaveBeenCalledTimes(2);
    expect(next.mock.calls[1][0].headers.get('X-Confirm-Password')).toBe('web-password');
  });

  it('does not prompt when an API key is told step-up is forbidden', async () => {
    const { auth, dialog, run } = setup();
    const next = vi.fn().mockReturnValue(
      throwError(
        () =>
          new HttpErrorResponse({
            status: 403,
            error: { error_kind: 'step_up_forbidden' },
          }),
      ),
    );

    await expect(run(new HttpRequest('PUT', '/api/config/general', {}), next)).rejects.toMatchObject({
      status: 403,
    });
    expect(dialog.open).not.toHaveBeenCalled();
    expect(auth.refresh).not.toHaveBeenCalled();
  });
}

const unauthorized = (url: string) =>
  throwError(() => new HttpErrorResponse({ status: 401, url }));

describe('authInterceptor', () => {
  afterEach(() => TestBed.resetTestingModule());

  it('refreshes and retries API requests that get a 401', async () => {
    const { auth, run } = setup();
    const next = vi
      .fn()
      .mockReturnValueOnce(unauthorized('/api/queue'))
      .mockReturnValueOnce(of(new HttpResponse({ status: 200 })));

    await run(new HttpRequest('GET', '/api/queue'), next);

    expect(auth.refresh).toHaveBeenCalledTimes(1);
    expect(next.mock.calls[1][0].headers.get('Authorization')).toBe('Bearer access-2');
  });

  it.each([
    `${window.location.origin}/dav/content`,
    '/dav/content/Release/',
    '/dav',
  ])('never spends a refresh token on a 401 from WebDAV (%s)', async (url) => {
    const { auth, router, run } = setup();
    const next = vi.fn((_req: HttpRequest<unknown>) => unauthorized(url));

    await expect(run(new HttpRequest('GET', url), next)).rejects.toMatchObject({ status: 401 });

    expect(next).toHaveBeenCalledTimes(1);
    expect(next.mock.calls[0]![0].headers.get('Authorization')).toBe('Bearer access-1');
    expect(auth.refresh).not.toHaveBeenCalled();
    expect(auth.clearTokens).not.toHaveBeenCalled();
    expect(router.navigate).not.toHaveBeenCalled();
  });

  it('treats /api/dav/... as a normal API request', async () => {
    const { auth, run } = setup();
    const next = vi
      .fn()
      .mockReturnValueOnce(unauthorized('/api/dav/status'))
      .mockReturnValueOnce(of(new HttpResponse({ status: 200 })));

    await run(new HttpRequest('GET', '/api/dav/status'), next);

    expect(auth.refresh).toHaveBeenCalledTimes(1);
  });

  it('verifies the session only once the retried request succeeds', async () => {
    const { auth, run } = setup();
    const next = vi
      .fn()
      .mockReturnValueOnce(unauthorized('/api/queue'))
      .mockReturnValueOnce(of(new HttpResponse({ status: 200 })));

    await run(new HttpRequest('GET', '/api/queue'), next);

    expect(auth.markSessionVerified).toHaveBeenCalledTimes(1);
  });

  it('leaves the session unverified when the retried request is rejected', async () => {
    const { auth, run } = setup();
    const next = vi
      .fn()
      .mockReturnValueOnce(unauthorized('/api/queue'))
      .mockReturnValueOnce(unauthorized('/api/queue'));

    await expect(run(new HttpRequest('GET', '/api/queue'), next)).rejects.toMatchObject({
      status: 401,
    });

    expect(auth.markSessionVerified).not.toHaveBeenCalled();
  });

  it('retries with the other tab’s access token, not its refresh token', async () => {
    const { auth, router, run } = setup('access-stale');
    auth.refresh.mockReturnValue(throwError(() => new HttpErrorResponse({ status: 401 })));
    auth.discardFailedSession.mockImplementation(() => {
      auth.getAccessToken.mockReturnValue('access-from-other-tab');
      return false;
    });
    const next = vi
      .fn()
      .mockReturnValueOnce(unauthorized('/api/queue'))
      .mockReturnValueOnce(of(new HttpResponse({ status: 200 })));

    await run(new HttpRequest('GET', '/api/queue'), next);

    expect(auth.discardFailedSession).toHaveBeenCalledTimes(1);
    expect(next.mock.calls[1][0].headers.get('Authorization')).toBe(
      'Bearer access-from-other-tab',
    );
    expect(auth.clearTokens).not.toHaveBeenCalled();
    expect(router.navigate).not.toHaveBeenCalled();
  });
});
