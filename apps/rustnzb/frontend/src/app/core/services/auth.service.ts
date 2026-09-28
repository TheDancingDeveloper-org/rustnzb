import { Injectable, computed, signal } from '@angular/core';
import { HttpClient, HttpErrorResponse } from '@angular/common/http';
import { Observable, catchError, finalize, map, of, shareReplay, tap, throwError } from 'rxjs';

export interface AuthStatus {
  auth_enabled: boolean;
  setup_required: boolean;
}

export interface TokenResponse {
  access_token: string;
  refresh_token: string;
  token_type: string;
  expires_in: number;
}

const ACCESS_KEY = 'access_token';
const REFRESH_KEY = 'refresh_token';
const EXPIRES_KEY = 'access_token_expires_at';
// Refresh slightly early so a token never expires between check and use.
const EXPIRY_SKEW_MS = 30_000;

@Injectable({ providedIn: 'root' })
export class AuthService {
  private baseUrl = '/api/auth';

  private readonly accessToken = signal<string | null>(localStorage.getItem(ACCESS_KEY));
  // A stored token is only a claim: the server keeps tokens in memory, so a
  // restart (or expiry) invalidates it. Chrome and guarded routes wait until
  // the token has been confirmed against the server once per page load.
  private readonly verified = signal(false);
  readonly authenticated = computed(() => !!this.accessToken() && this.verified());

  private refresh$: Observable<TokenResponse> | null = null;
  private verify$: Observable<boolean> | null = null;

  constructor(private http: HttpClient) {}

  checkAuth(): Observable<AuthStatus> {
    return this.http.get<AuthStatus>(`${this.baseUrl}/status`);
  }

  setup(username: string, password: string): Observable<TokenResponse> {
    return this.http
      .post<TokenResponse>(`${this.baseUrl}/setup`, { username, password })
      .pipe(tap((res) => this.storeTokens(res)));
  }

  login(username: string, password: string): Observable<TokenResponse> {
    return this.http
      .post<TokenResponse>(`${this.baseUrl}/login`, { username, password })
      .pipe(tap((res) => this.storeTokens(res)));
  }

  /**
   * Rotate tokens. Concurrent callers share one in-flight request: refresh
   * tokens are single-use, so parallel refreshes would revoke each other.
   */
  refresh(): Observable<TokenResponse> {
    if (!this.refresh$) {
      const refreshToken = localStorage.getItem(REFRESH_KEY);
      this.refresh$ = this.http
        .post<TokenResponse>(`${this.baseUrl}/refresh`, { refresh_token: refreshToken })
        .pipe(
          tap((res) => this.storeTokens(res)),
          finalize(() => (this.refresh$ = null)),
          shareReplay({ bufferSize: 1, refCount: false }),
        );
    }
    return this.refresh$;
  }

  logout(): Observable<void> {
    const refreshToken = localStorage.getItem(REFRESH_KEY);
    this.clearTokens();
    return this.http.post<void>(`${this.baseUrl}/logout`, { refresh_token: refreshToken });
  }

  /**
   * Resolve whether the stored session is usable, confirming it with the
   * server on first use. Emits false (and clears tokens) when the server
   * rejects it; a network failure leaves the session in place.
   */
  ensureSession(): Observable<boolean> {
    if (!this.getAccessToken()) return of(false);
    if (this.verified()) return of(true);
    if (!this.verify$) {
      const probe$: Observable<unknown> = this.accessTokenExpired()
        ? this.refresh()
        : this.http.get('/api/status');
      this.verify$ = probe$.pipe(
        map(() => true),
        catchError((err) => {
          const rejected =
            err instanceof HttpErrorResponse && (err.status === 401 || err.status === 403);
          if (rejected) this.clearTokens();
          return of(!rejected);
        }),
        map((ok) => ok && this.isLoggedIn()),
        tap((ok) => this.verified.set(ok)),
        finalize(() => (this.verify$ = null)),
        shareReplay({ bufferSize: 1, refCount: false }),
      );
    }
    return this.verify$;
  }

  isLoggedIn(): boolean {
    return !!localStorage.getItem(ACCESS_KEY);
  }

  getAccessToken(): string | null {
    return localStorage.getItem(ACCESS_KEY);
  }

  private accessTokenExpired(): boolean {
    const expiresAt = Number(localStorage.getItem(EXPIRES_KEY));
    return !!expiresAt && Date.now() >= expiresAt - EXPIRY_SKEW_MS;
  }

  private storeTokens(res: TokenResponse): void {
    localStorage.setItem(ACCESS_KEY, res.access_token);
    localStorage.setItem(REFRESH_KEY, res.refresh_token);
    if (res.expires_in) {
      localStorage.setItem(EXPIRES_KEY, String(Date.now() + res.expires_in * 1000));
    }
    this.accessToken.set(res.access_token);
    this.verified.set(true);
  }

  clearTokens(): void {
    localStorage.removeItem(ACCESS_KEY);
    localStorage.removeItem(REFRESH_KEY);
    localStorage.removeItem(EXPIRES_KEY);
    this.accessToken.set(null);
    this.verified.set(false);
  }
}
