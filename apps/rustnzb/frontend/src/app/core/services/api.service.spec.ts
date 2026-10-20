import '@angular/compiler';

import { HttpClient, HttpHeaders } from '@angular/common/http';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { of } from 'rxjs';

import { ApiService } from './api.service';

describe('ApiService', () => {
  let http: Record<'get' | 'post' | 'put' | 'delete', ReturnType<typeof vi.fn>>;
  let service: ApiService;

  beforeEach(() => {
    localStorage.clear();
    http = {
      get: vi.fn(() => of({})),
      post: vi.fn(() => of({})),
      put: vi.fn(() => of({})),
      delete: vi.fn(() => of({})),
    };
    service = new ApiService(http as unknown as HttpClient);
  });

  it('prefixes paths and forwards query parameters', () => {
    service.get('/groups', { search: 'linux', limit: '25' }).subscribe();
    const [url, options] = http.get.mock.calls[0];
    expect(url).toBe('/api/groups');
    expect(options.params).toEqual({ search: 'linux', limit: '25' });
    expect((options.headers as HttpHeaders).has('Authorization')).toBe(false);
  });

  it.each([
    ['post', '/queue/pause', { seconds: 60 }],
    ['put', '/config/general', { port: 9090 }],
  ] as const)('%s sends JSON bodies with bearer authentication', (method, path, body) => {
    localStorage.setItem('access_token', 'token-1');
    service[method](path, body).subscribe();
    const [url, actualBody, options] = http[method].mock.calls[0];
    expect(url).toBe(`/api${path}`);
    expect(actualBody).toEqual(body);
    expect((options.headers as HttpHeaders).get('Authorization')).toBe('Bearer token-1');
  });

  it('deletes with bearer authentication', () => {
    localStorage.setItem('access_token', 'token-2');
    service.delete('/queue/job-1').subscribe();
    const [url, options] = http.delete.mock.calls[0];
    expect(url).toBe('/api/queue/job-1');
    expect((options.headers as HttpHeaders).get('Authorization')).toBe('Bearer token-2');
  });

  it('fetches a text body with bearer authentication and query parameters', () => {
    localStorage.setItem('access_token', 'token-4');
    service.getText('/config/backup', { secrets: 'true' }).subscribe();
    const [url, options] = http.get.mock.calls[0];
    expect(url).toBe('/api/config/backup');
    expect(options.params).toEqual({ secrets: 'true' });
    expect(options.responseType).toBe('text');
    expect(options.observe).toBe('response');
    expect((options.headers as HttpHeaders).get('Authorization')).toBe('Bearer token-4');
  });

  it('posts application/toml with bearer authentication and no JSON content-type', () => {
    localStorage.setItem('access_token', 'token-5');
    service.postText('/config/backup/preview', '[config]').subscribe();
    const [url, body, options] = http.post.mock.calls[0];
    expect(url).toBe('/api/config/backup/preview');
    expect(body).toBe('[config]');
    expect((options.headers as HttpHeaders).get('Authorization')).toBe('Bearer token-5');
    expect((options.headers as HttpHeaders).get('Content-Type')).toBe('application/toml');
  });

  it('posts FormData without adding a content-type header', () => {
    localStorage.setItem('access_token', 'token-3');
    const form = new FormData();
    form.append('file', new Blob(['nzb']), 'sample.nzb');
    service.postForm('/queue/add', form).subscribe();
    const [url, body, options] = http.post.mock.calls[0];
    expect(url).toBe('/api/queue/add');
    expect(body).toBe(form);
    expect((options.headers as HttpHeaders).get('Authorization')).toBe('Bearer token-3');
    expect((options.headers as HttpHeaders).has('Content-Type')).toBe(false);
  });
});
