import '@angular/compiler';

import { describe, expect, it, vi } from 'vitest';
import { HttpErrorResponse } from '@angular/common/http';
import { of, throwError } from 'rxjs';

import { ApiService } from '../../core/services/api.service';
import { RssViewComponent } from './rss-view.component';

function createComponent(overrides: Record<string, ReturnType<typeof vi.fn>> = {}) {
  const api = {
    get: vi.fn(() => of([])), post: vi.fn(() => of({})), put: vi.fn(() => of({})), delete: vi.fn(() => of({})),
    ...overrides,
  };
  const snack = { open: vi.fn() };
  const confirm = { confirm: vi.fn(() => of(true)) };
  return { component: new RssViewComponent(api as unknown as ApiService, snack as never, confirm as never), api, snack, confirm };
}

describe('RssViewComponent', () => {
  it('formats item sizes; blank when unknown (BUG-120)', () => {
    const { component } = createComponent();
    expect(component.formatBytes(1024 ** 4)).toBe('1.0 TB');
    expect(component.formatBytes(0)).toBe('');
  });

  it('loads all RSS resources and derives summary counts', () => {
    const { component } = createComponent({
      get: vi.fn((path: string) => of(path === '/config/rss-feeds'
        ? [{ name: 'daily', url: 'https://example.test?apikey=secret', poll_interval_secs: 120, enabled: true, auto_download: false }]
        : path === '/rss/rules' ? [{ id: 'rule', name: 'TV', match_regex: 'TV', enabled: true, feed_names: [] }]
        : [{ id: 'item', downloaded: false }]))
    });
    component.ngOnInit();
    expect(component.enabledFeedCount()).toBe(1);
    expect(component.enabledRuleCount()).toBe(1);
    expect(component.pendingCount()).toBe(1);
    expect(component.avgPollLabel()).toBe('2m');
    expect(component.maskUrl('https://x.test?apikey=secret&token=other')).toContain('apikey=***');
  });

  it('shows a filter example in the server\'s Rust regex syntax (BUG-122)', () => {
    const { component } = createComponent();
    // The server compiles filter_regex with the Rust regex crate: JS-style
    // /pattern/flags delimiters would be matched literally and never hit.
    expect(component.filterRegexExample).not.toMatch(/^\/.*\/[a-z]*$/);
    expect(component.filterRegexExample).toBe('(?i)ubuntu|debian');
  });

  it('validates feeds before making requests and reports API errors', () => {
    const { component, api, snack } = createComponent({ post: vi.fn(() => throwError(() => new Error('failed'))) });
    component.showAddFeed();
    component.saveFeed();
    expect(api.post).not.toHaveBeenCalled();
    expect(snack.open).toHaveBeenCalledWith('Name and URL are required', 'Close', { duration: 3000 });

    component.feedForm = { ...component.feedForm, name: 'daily', url: 'https://example.test' };
    component.saveFeed();
    expect(api.post).toHaveBeenCalledWith('/config/rss-feeds', expect.objectContaining({ name: 'daily' }));
    expect(snack.open).toHaveBeenCalledWith('Failed to save feed', 'Close', { duration: 5000 });
  });

  it('encodes edited feed names and submits parsed rule feed names', () => {
    const { component, api } = createComponent();
    component.editFeed({ name: 'daily tv', url: 'https://example.test', poll_interval_secs: 60, category: null, filter_regex: null, enabled: true, auto_download: false });
    component.saveFeed();
    expect(api.put).toHaveBeenCalledWith('/config/rss-feeds/daily%20tv', expect.anything());

    component.showAddRule();
    component.ruleForm = { ...component.ruleForm, name: 'TV', match_regex: '1080p', feed_names_csv: ' daily, tv , ' };
    component.saveRule();
    expect(api.post).toHaveBeenCalledWith('/rss/rules', expect.objectContaining({ feed_names: ['daily', 'tv'] }));
  });

  it('shows the server message inline and as a toast when adding a feed is rejected', () => {
    const rejected = new HttpErrorResponse({
      status: 400,
      url: '/api/config/rss-feeds',
      error: {
        error_kind: 'bad_request',
        human_readable: 'Feed URL rejected: URL targets a private/reserved address',
        status: 400,
      },
    });
    const { component, snack } = createComponent({ post: vi.fn(() => throwError(() => rejected)) });
    component.showAddFeed();
    component.feedForm = { ...component.feedForm, name: 'lan', url: 'http://192.168.1.10/rss' };
    component.saveFeed();

    const expected = 'Failed to save feed: Feed URL rejected: URL targets a private/reserved address';
    expect(component.feedFormVisible()).toBe(true);
    expect(component.feedError()).toBe(expected);
    expect(snack.open).toHaveBeenCalledWith(expected, 'Close', { duration: 5000 });

    component.cancelFeedForm();
    component.showAddFeed();
    expect(component.feedError()).toBeNull();
  });

  it('shows the server message inline when saving a rule is rejected', () => {
    const rejected = new HttpErrorResponse({ status: 400, error: { human_readable: 'Invalid regex' } });
    const { component } = createComponent({ post: vi.fn(() => throwError(() => rejected)) });
    component.showAddRule();
    component.ruleForm = { ...component.ruleForm, name: 'TV', match_regex: '(' };
    component.saveRule();
    expect(component.ruleError()).toBe('Failed to save rule: Invalid regex');
  });
});
