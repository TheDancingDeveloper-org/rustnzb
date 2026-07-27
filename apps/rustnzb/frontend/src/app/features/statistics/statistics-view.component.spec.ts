import '@angular/compiler';

import { describe, expect, it, vi } from 'vitest';
import { of, throwError } from 'rxjs';

import { ApiService } from '../../core/services/api.service';
import { StatisticsViewComponent } from './statistics-view.component';

describe('StatisticsViewComponent', () => {
  it('loads statistics, switches periods, and formats boundaries', () => {
    const period = { downloads: 1, completed: 0, failed: 0, bytes_downloaded: 0, total_duration_secs: 0, average_speed_bps: 0, fastest_download_bps: 0, news_server_hits: 0, articles_served: 9, articles_missing: 1 };
    const api = { get: vi.fn(() => of({ today: period, week: period, month: period, lifetime: period, servers: [], daily: [] })) };
    const component = new StatisticsViewComponent(api as unknown as ApiService);
    component.ngOnInit();
    expect(component.selectedTotals().downloads).toBe(1);
    expect(component.availabilityCounts(9, 1)).toBe('90.00%');
    expect(component.availabilityCounts(0, 0)).toBe('—');
    expect(component.formatBytes(1536)).toBe('1.5 KB');
    expect(component.formatSpeed(1024)).toBe('1.0 KB/s');
  });

  it('keeps empty statistics when the API fails', () => {
    const api = { get: vi.fn(() => throwError(() => new Error('offline'))) };
    const component = new StatisticsViewComponent(api as unknown as ApiService);
    component.ngOnInit();
    expect(component.selectedTotals().downloads).toBe(0);
  });
});
