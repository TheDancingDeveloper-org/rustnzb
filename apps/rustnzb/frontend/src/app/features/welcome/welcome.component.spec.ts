import '@angular/compiler';

import { describe, expect, it, vi } from 'vitest';
import { of, throwError } from 'rxjs';

import { ApiService } from '../../core/services/api.service';
import { WelcomeComponent } from './welcome.component';

function makeComponent(overrides: Record<string, ReturnType<typeof vi.fn>> = {}) {
  const api = { get: vi.fn(() => of({ has_servers: false })), post: vi.fn(() => of({})), postForm: vi.fn(() => of({})), ...overrides };
  const router = { navigate: vi.fn(() => Promise.resolve(true)) };
  return { component: new WelcomeComponent(api as unknown as ApiService, router as never), api, router };
}

describe('WelcomeComponent', () => {
  it('redirects established installations and validates required API import fields', () => {
    const { component, router } = makeComponent({ get: vi.fn(() => of({ has_servers: true })) });
    component.ngOnInit();
    expect(router.navigate).toHaveBeenCalledWith(['/downloads']);
    component.fetchConfig();
    expect(component.connectError()).toBe('SABnzbd URL is required.');
    component.sabnzbdUrl.set('https://sab.test');
    component.fetchConfig();
    expect(component.connectError()).toBe('API key is required.');
  });

  it('surfaces import and apply errors without advancing the wizard', () => {
    const { component } = makeComponent({
      post: vi.fn(() => throwError(() => ({ error: { message: 'unreachable' } }))),
    });
    component.sabnzbdUrl.set('https://sab.test');
    component.sabnzbdApiKey.set('key');
    component.fetchConfig();
    expect(component.connectError()).toBe('unreachable');

    component.preview.set({ servers: [], categories: [], general: {}, rss_feeds: [], warnings: [], skipped_fields: [] } as never);
    component.applyImport();
    expect(component.step()).toBe('preview');
    expect(component.applyError()).toBe('unreachable');
  });

  it('updates masked server passwords before applying the import', () => {
    const { component, api, router } = makeComponent();
    component.preview.set({ servers: [{ password_masked: true, password: null }], categories: [], general: {}, rss_feeds: [], warnings: [], skipped_fields: [] } as never);
    expect(component.hasMaskedPasswords()).toBe(true);
    component.setServerPassword(0, 'replacement');
    expect(component.hasMaskedPasswords()).toBe(false);
    component.applyImport();
    expect(api.post).toHaveBeenCalledWith('/setup/apply', expect.objectContaining({ servers: [expect.objectContaining({ password: 'replacement', password_masked: false })] }));
    expect(router.navigate).toHaveBeenCalledWith(['/downloads']);
  });
});
