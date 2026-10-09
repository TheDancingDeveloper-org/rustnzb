import '@angular/compiler';

import { HttpErrorResponse } from '@angular/common/http';
import { describe, expect, it, vi } from 'vitest';

import { httpErrorDetail, httpErrorMessage, isAuthHandledError, showHttpError } from './http-error';

function httpError(status: number, error: unknown): HttpErrorResponse {
  return new HttpErrorResponse({ status, error, url: '/api/x' });
}

describe('httpErrorMessage', () => {
  it('prefers the central human_readable field', () => {
    const err = httpError(400, {
      error_kind: 'bad_request',
      human_readable: 'Feed URL rejected: URL targets a private/reserved address',
      status: 400,
    });
    expect(httpErrorMessage(err, 'Failed to save feed')).toBe(
      'Failed to save feed: Feed URL rejected: URL targets a private/reserved address',
    );
  });

  it('falls back to the legacy error and message fields', () => {
    expect(httpErrorMessage(httpError(400, { status: false, error: 'No job ID' }), 'Failed')).toBe(
      'Failed: No job ID',
    );
    expect(httpErrorMessage(httpError(422, { message: 'Bad NZB' }), 'Failed')).toBe(
      'Failed: Bad NZB',
    );
  });

  it('parses JSON bodies delivered as text and accepts short plain-text bodies', () => {
    expect(
      httpErrorMessage(httpError(400, '{"human_readable":"Invalid regex"}'), 'Failed to save rule'),
    ).toBe('Failed to save rule: Invalid regex');
    expect(httpErrorMessage(httpError(415, 'Expected JSON content type'), 'Failed')).toBe(
      'Failed: Expected JSON content type',
    );
  });

  it('uses the fallback when there is no usable server message', () => {
    expect(httpErrorMessage(httpError(500, null), 'Failed to save')).toBe('Failed to save');
    expect(
      httpErrorMessage(httpError(502, '<html><body>Bad gateway</body></html>'), 'Failed'),
    ).toBe('Failed');
    expect(httpErrorMessage(httpError(400, { human_readable: '   ' }), 'Failed')).toBe('Failed');
    expect(httpErrorMessage(new Error('boom'), 'Failed')).toBe('Failed');
    expect(httpErrorMessage(undefined, 'Failed')).toBe('Failed');
  });

  it('does not repeat the fallback when the server message already is it', () => {
    expect(httpErrorMessage(httpError(400, { error: 'Failed' }), 'Failed')).toBe('Failed');
  });
});

describe('httpErrorDetail', () => {
  it('returns only the server text, or null', () => {
    expect(httpErrorDetail(httpError(400, { human_readable: 'Bad', error: 'ignored' }))).toBe(
      'Bad',
    );
    expect(httpErrorDetail(httpError(0, null))).toBeNull();
    expect(httpErrorDetail(new Error('client'))).toBeNull();
  });
});

describe('showHttpError', () => {
  it('opens a snackbar with the extracted message', () => {
    const snack = { open: vi.fn() };
    showHttpError(snack, httpError(400, { human_readable: 'Bad' }), 'Failed to save');
    expect(snack.open).toHaveBeenCalledWith('Failed to save: Bad', 'Close', { duration: 5000 });
  });

  it('stays silent on 401s, which the auth interceptor already handles', () => {
    const snack = { open: vi.fn() };
    const err = httpError(401, { human_readable: 'Unauthorized' });
    expect(isAuthHandledError(err)).toBe(true);
    showHttpError(snack, err, 'Failed to save');
    expect(snack.open).not.toHaveBeenCalled();
    expect(isAuthHandledError(httpError(403, null))).toBe(false);
  });
});
