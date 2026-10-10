import '@angular/compiler';

import { HttpErrorResponse, HttpRequest, HttpResponse } from '@angular/common/http';
import { TestBed } from '@angular/core/testing';
import { Router } from '@angular/router';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { firstValueFrom, of, throwError } from 'rxjs';

import { AuthService } from '../services/auth.service';
import { authInterceptor } from './auth.interceptor';

function setup(token = 'access-1') {
  const auth = {
    getAccessToken: vi.fn(() => token),
    refresh: vi.fn(() => of({ access_token: 'access-2' })),
    clearTokens: vi.fn(),
  };
  const router = { navigate: vi.fn() };
  TestBed.configureTestingModule({
    providers: [
      { provide: AuthService, useValue: auth },
      { provide: Router, useValue: router },
    ],
  });
  const run = (req: HttpRequest<unknown>, next: (r: HttpRequest<unknown>) => unknown) =>
    firstValueFrom(
      TestBed.runInInjectionContext(() => authInterceptor(req, next as never)),
    );
  return { auth, router, run };
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
});
