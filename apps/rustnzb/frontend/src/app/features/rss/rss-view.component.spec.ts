import '@angular/compiler';

import { describe, expect, it, vi } from 'vitest';
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

  it('validates feeds before making requests and reports API errors', () => {
    const { component, api, snack } = createComponent({ post: vi.fn(() => throwError(() => new Error('failed'))) });
    component.showAddFeed();
    component.saveFeed();
    expect(api.post).not.toHaveBeenCalled();
    expect(snack.open).toHaveBeenCalledWith('Name and URL are required', 'Close', { duration: 3000 });

    component.feedForm = { ...component.feedForm, name: 'daily', url: 'https://example.test' };
    component.saveFeed();
    expect(api.post).toHaveBeenCalledWith('/config/rss-feeds', expect.objectContaining({ name: 'daily' }));
    expect(snack.open).toHaveBeenCalledWith('Failed to save feed', 'Close', { duration: 3000 });
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
});
