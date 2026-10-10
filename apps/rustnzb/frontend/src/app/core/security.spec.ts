import '@angular/compiler';

import {
  HttpErrorResponse,
  HttpRequest,
  HttpResponse,
  HttpHandlerFn,
} from '@angular/common/http';
import { TestBed } from '@angular/core/testing';
import { Router } from '@angular/router';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { Observable, Subject, firstValueFrom, of, throwError } from 'rxjs';

import { authGuard } from './guards/auth.guard';
import { authInterceptor } from './interceptors/auth.interceptor';
import { AuthService } from './services/auth.service';

describe('authGuard', () => {
  function run(session: boolean) {
    const auth = { ensureSession: vi.fn(() => of(session)) };
    const loginTree = { login: true };
    const router = { createUrlTree: vi.fn(() => loginTree) };
    TestBed.configureTestingModule({
      providers: [
        { provide: AuthService, useValue: auth },
        { provide: Router, useValue: router },
      ],
    });
    const result = TestBed.runInInjectionContext(() => authGuard({} as never, {} as never));
    return { result: result as Observable<unknown>, router, loginTree };
  }

  afterEach(() => TestBed.resetTestingModule());

  it('allows navigation once the session is confirmed', async () => {
    const { result, router } = run(true);
    expect(await firstValueFrom(result)).toBe(true);
    expect(router.createUrlTree).not.toHaveBeenCalled();
  });

  it('redirects to login when the stored session is rejected', async () => {
    const { result, router, loginTree } = run(false);
    expect(await firstValueFrom(result)).toBe(loginTree);
    expect(router.createUrlTree).toHaveBeenCalledWith(['/login']);
  });
});

describe('authInterceptor', () => {
  function configure(refreshResult: Observable<unknown> = of({ access_token: 'new-access' })) {
    let token: string | null = 'old-access';
    const auth = {
      getAccessToken: vi.fn(() => token),
      refresh: vi.fn(() => refreshResult),
      clearTokens: vi.fn(),
      discardFailedSession: vi.fn(() => true),
      setToken: (t: string | null) => (token = t),
    };
    const router = { navigate: vi.fn(() => Promise.resolve(true)) };
    TestBed.configureTestingModule({
      providers: [
        { provide: AuthService, useValue: auth },
        { provide: Router, useValue: router },
      ],
    });
    return { auth, router };
  }

  const unauthorized = () => throwError(() => new HttpErrorResponse({ status: 401 }));
  const ok = () => of(new HttpResponse({ status: 200 }));
  const intercept = (request: HttpRequest<unknown>, next: HttpHandlerFn) =>
    TestBed.runInInjectionContext(() => authInterceptor(request, next));
  const authHeader = (next: ReturnType<typeof vi.fn>, call: number) =>
    (next.mock.calls[call][0] as HttpRequest<unknown>).headers.get('Authorization');

  afterEach(() => TestBed.resetTestingModule());

  it('does not intercept authentication endpoints', async () => {
    const next = vi.fn(() => of(new HttpResponse({ status: 200 }))) as unknown as HttpHandlerFn;
    const request = new HttpRequest('POST', '/api/auth/login', {});
    await firstValueFrom(authInterceptor(request, next));
    expect(next).toHaveBeenCalledWith(request);
  });

  it('attaches the current access token when the request has none', async () => {
    configure();
    const next = vi.fn(ok);
    await firstValueFrom(intercept(new HttpRequest('GET', '/api/status'), next as HttpHandlerFn));
    expect(authHeader(next, 0)).toBe('Bearer old-access');
  });

  it('refreshes after a 401 and retries with the rotated access token', async () => {
    const { auth } = configure();
    const next = vi.fn().mockReturnValueOnce(unauthorized()).mockReturnValueOnce(ok());

    await firstValueFrom(intercept(new HttpRequest('GET', '/api/queue'), next as HttpHandlerFn));

    expect(auth.refresh).toHaveBeenCalledTimes(1);
    expect(next).toHaveBeenCalledTimes(2);
    expect(authHeader(next, 1)).toBe('Bearer new-access');
  });

  it('recovers every concurrent 401, not just the first', async () => {
    const refresh = new Subject<{ access_token: string }>();
    const { auth } = configure(refresh);
    // Mirrors AuthService.refresh(): concurrent callers share one request.
    auth.refresh.mockReturnValue(refresh);
    const next = vi.fn((req: HttpRequest<unknown>) =>
      req.headers.get('Authorization') === 'Bearer new-access' ? ok() : unauthorized(),
    );

    const results = ['/api/queue', '/api/history', '/api/config/servers'].map((url) =>
      firstValueFrom(intercept(new HttpRequest('GET', url), next as HttpHandlerFn)),
    );
    refresh.next({ access_token: 'new-access' });
    refresh.complete();

    await expect(Promise.all(results)).resolves.toHaveLength(3);
    expect(next).toHaveBeenCalledTimes(6);
  });

  it('retries with an already-rotated token without refreshing again', async () => {
    const { auth } = configure();
    auth.setToken('rotated-access');
    const next = vi.fn().mockReturnValueOnce(unauthorized()).mockReturnValueOnce(ok());
    const request = new HttpRequest('GET', '/api/queue').clone({
      setHeaders: { Authorization: 'Bearer old-access' },
    });

    await firstValueFrom(intercept(request, next as HttpHandlerFn));

    expect(auth.refresh).not.toHaveBeenCalled();
    expect(authHeader(next, 1)).toBe('Bearer rotated-access');
  });

  it('keeps the session when the retried request fails for another reason', async () => {
    const { auth, router } = configure();
    const next = vi
      .fn()
      .mockReturnValueOnce(unauthorized())
      .mockReturnValueOnce(throwError(() => new HttpErrorResponse({ status: 500 })));

    await expect(
      firstValueFrom(intercept(new HttpRequest('GET', '/api/queue'), next as HttpHandlerFn)),
    ).rejects.toMatchObject({ status: 500 });
    expect(auth.clearTokens).not.toHaveBeenCalled();
    expect(router.navigate).not.toHaveBeenCalled();
  });

  it('clears credentials and redirects when refresh fails', async () => {
    const { auth, router } = configure(
      throwError(() => new HttpErrorResponse({ status: 403 })),
    );
    const next = vi.fn(unauthorized);

    await expect(
      firstValueFrom(intercept(new HttpRequest('GET', '/api/queue'), next as HttpHandlerFn)),
    ).rejects.toMatchObject({ status: 403 });
    expect(auth.discardFailedSession).toHaveBeenCalledTimes(1);
    expect(router.navigate).toHaveBeenCalledWith(['/login']);
  });

  it('retries with tokens another tab stored when its own refresh failed (BUG-123)', async () => {
    const { auth, router } = configure(
      throwError(() => new HttpErrorResponse({ status: 403 })),
    );
    auth.discardFailedSession.mockReturnValue(false);
    auth.setToken('tab-b-access');
    const next = vi.fn().mockReturnValueOnce(unauthorized()).mockReturnValueOnce(ok());

    await firstValueFrom(intercept(new HttpRequest('GET', '/api/queue'), next as HttpHandlerFn));

    expect(authHeader(next, 1)).toBe('Bearer tab-b-access');
    expect(auth.clearTokens).not.toHaveBeenCalled();
    expect(router.navigate).not.toHaveBeenCalled();
  });
});
