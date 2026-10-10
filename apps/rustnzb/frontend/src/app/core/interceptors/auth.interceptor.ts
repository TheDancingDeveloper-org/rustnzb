import { HttpInterceptorFn, HttpErrorResponse, HttpRequest } from '@angular/common/http';
import { inject } from '@angular/core';
import { Router } from '@angular/router';
import { catchError, of, switchMap, throwError } from 'rxjs';
import { AuthService, TokenResponse } from '../services/auth.service';

function withToken<T>(req: HttpRequest<T>, token: string): HttpRequest<T> {
  return req.clone({ setHeaders: { Authorization: `Bearer ${token}` } });
}

export const authInterceptor: HttpInterceptorFn = (req, next) => {
  // Don't intercept auth endpoints
  if (req.url.includes('/api/auth/')) {
    return next(req);
  }

  const authService = inject(AuthService);
  const router = inject(Router);

  const token = authService.getAccessToken();
  if (token && !req.headers.has('Authorization')) {
    req = withToken(req, token);
  }

  return next(req).pipe(
    catchError((error: HttpErrorResponse) => {
      if (error.status !== 401) {
        return throwError(() => error);
      }

      // Tokens already rotated while this request was in flight: retry with
      // the current one rather than spending another single-use refresh token.
      const current = authService.getAccessToken();
      if (current && req.headers.get('Authorization') !== `Bearer ${current}`) {
        return next(withToken(req, current));
      }

      // Every concurrent 401 waits on the same refresh and then retries, so
      // parallel page loads all recover instead of only the first request.
      return authService.refresh().pipe(
        catchError((refreshError) => {
          // Another tab may have rotated the tokens after ours failed; only a
          // session nobody has replaced is discarded.
          if (!authService.discardFailedSession()) {
            const current = authService.getAccessToken();
            if (current) {
              return of({ access_token: current } as TokenResponse);
            }
          }
          authService.clearTokens();
          router.navigate(['/login']);
          return throwError(() => refreshError);
        }),
        switchMap((tokens) => next(withToken(req, tokens.access_token))),
      );
    }),
  );
};
