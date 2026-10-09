import type { HttpErrorResponse } from '@angular/common/http';

/** Anything with MatSnackBar's `open` signature (kept narrow for tests). */
export interface SnackOpener {
  open(message: string, action?: string, config?: { duration?: number }): unknown;
}

const MAX_TEXT_BODY = 300;

function nonEmpty(value: unknown): string | null {
  return typeof value === 'string' && value.trim() ? value.trim() : null;
}

function fromBody(body: unknown): string | null {
  if (typeof body === 'string') {
    const text = body.trim();
    if (!text) return null;
    if (text.startsWith('{')) {
      try {
        return fromBody(JSON.parse(text));
      } catch {
        // Not JSON after all; treat it as plain text below.
      }
    }
    // Short plain-text bodies (e.g. axum extractor rejections) are useful;
    // HTML error pages from a proxy are not.
    if (text.startsWith('<') || text.length > MAX_TEXT_BODY) return null;
    return text;
  }
  if (body && typeof body === 'object') {
    const b = body as Record<string, unknown>;
    // Central API error shape first, then the legacy `{status:false, error}`
    // shape, then older `{message}` bodies.
    return nonEmpty(b['human_readable']) ?? nonEmpty(b['error']) ?? nonEmpty(b['message']);
  }
  return null;
}

/**
 * The server's own explanation of a failed request, or `null` when the
 * response carries nothing usable (network error, HTML proxy page, ...).
 */
export function httpErrorDetail(err: unknown): string | null {
  // Duck-typed rather than `instanceof HttpErrorResponse` so errors that were
  // re-thrown or built by hand with the same `{status, error}` shape work too.
  if (!err || typeof err !== 'object' || !('error' in err)) return null;
  return fromBody((err as HttpErrorResponse).error);
}

/**
 * Turn a failed request into a user-facing message: the server's own
 * explanation (`human_readable`, falling back to `error`/`message`) prefixed
 * with what the user was trying to do, or just `fallback` when the response
 * carries nothing usable.
 */
export function httpErrorMessage(err: unknown, fallback: string): string {
  const detail = httpErrorDetail(err);
  if (!detail || detail === fallback) return fallback;
  return `${fallback}: ${detail}`;
}

/**
 * A 401 reaching a component means the auth interceptor already tried to
 * refresh the session and has redirected to the login page; a toast on top
 * of that redirect is noise.
 */
export function isAuthHandledError(err: unknown): boolean {
  return !!err && typeof err === 'object' && (err as HttpErrorResponse).status === 401;
}

/** Show a failed request as a toast, unless the auth flow is handling it. */
export function showHttpError(
  snack: SnackOpener,
  err: unknown,
  fallback: string,
  duration = 5000,
): void {
  if (isAuthHandledError(err)) return;
  snack.open(httpErrorMessage(err, fallback), 'Close', { duration });
}
