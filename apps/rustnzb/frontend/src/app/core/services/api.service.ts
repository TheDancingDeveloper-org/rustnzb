import { Injectable } from '@angular/core';
import { HttpClient, HttpHeaders, HttpResponse } from '@angular/common/http';
import { Observable } from 'rxjs';

/** A background poll that has not answered by now errors out, so the next poll can run. */
export const POLL_TIMEOUT_MS = 15_000;

/** True while the page is in a background tab, where polls are skipped. */
export function pageHidden(): boolean {
  return typeof document !== 'undefined' && document.hidden;
}

@Injectable({ providedIn: 'root' })
export class ApiService {
  private baseUrl = '/api';

  constructor(private http: HttpClient) {}

  private headers(): HttpHeaders {
    const token = localStorage.getItem('access_token');
    return token ? new HttpHeaders({ Authorization: `Bearer ${token}` }) : new HttpHeaders();
  }

  get<T>(path: string, params?: Record<string, string>): Observable<T> {
    return this.http.get<T>(`${this.baseUrl}${path}`, { headers: this.headers(), params });
  }

  /**
   * A GET whose body is text, not JSON (the config backup download). The whole
   * response is returned so the caller can keep the server's `Content-Disposition`
   * filename as well as the bytes it wrote.
   */
  getText(path: string, params?: Record<string, string>): Observable<HttpResponse<string>> {
    return this.http.get(`${this.baseUrl}${path}`, {
      headers: this.headers(),
      params,
      responseType: 'text',
      observe: 'response',
    });
  }

  /** A POST whose body is `application/toml` and whose answer is JSON. */
  postText<T>(path: string, body: string): Observable<T> {
    const token = localStorage.getItem('access_token');
    let headers = token
      ? new HttpHeaders({ Authorization: `Bearer ${token}` })
      : new HttpHeaders();
    headers = headers.set('Content-Type', 'application/toml');
    return this.http.post<T>(`${this.baseUrl}${path}`, body, { headers });
  }

  post<T>(path: string, body?: unknown): Observable<T> {
    return this.http.post<T>(`${this.baseUrl}${path}`, body, { headers: this.headers() });
  }

  put<T>(path: string, body: unknown): Observable<T> {
    return this.http.put<T>(`${this.baseUrl}${path}`, body, { headers: this.headers() });
  }

  delete<T>(path: string, params?: Record<string, string>): Observable<T> {
    return this.http.delete<T>(`${this.baseUrl}${path}`, {
      headers: this.headers(),
      params,
    });
  }

  postForm<T>(path: string, body: FormData): Observable<T> {
    const token = localStorage.getItem('access_token');
    const headers = token ? new HttpHeaders({ Authorization: `Bearer ${token}` }) : new HttpHeaders();
    return this.http.post<T>(`${this.baseUrl}${path}`, body, { headers });
  }
}
