import { CommonModule } from '@angular/common';
import { Component, OnInit, computed, signal } from '@angular/core';
import { ApiService } from '../../core/services/api.service';

interface StatisticsPeriod {
  downloads: number;
  completed: number;
  failed: number;
  bytes_downloaded: number;
  total_duration_secs: number;
  average_speed_bps: number;
  fastest_download_bps: number;
  news_server_hits: number;
  articles_served: number;
  articles_missing: number;
}

interface ServerStatistics {
  server_id: string;
  server_name: string;
  total_bytes: number;
  today_bytes: number;
  week_bytes: number;
  month_bytes: number;
  total_ok: number;
  today_ok: number;
  week_ok: number;
  month_ok: number;
  total_fail: number;
  today_fail: number;
  week_fail: number;
  month_fail: number;
  last_active: string | null;
}

interface DailyStatistics extends StatisticsPeriod { date: string; }

interface GlobalStatistics {
  generated_at: string;
  lifetime: StatisticsPeriod;
  today: StatisticsPeriod;
  week: StatisticsPeriod;
  month: StatisticsPeriod;
  servers: ServerStatistics[];
  daily: DailyStatistics[];
}

type Period = 'today' | 'week' | 'month' | 'lifetime';

@Component({
  selector: 'app-statistics-view',
  standalone: true,
  imports: [CommonModule],
  template: `
    <div class="section-head">
      <div>
        <h2>Statistics</h2>
        <div class="sub">Persistent download, speed, news server, and article availability totals.</div>
      </div>
      <div class="periods" role="group" aria-label="Statistics period">
        @for (period of periodOptions; track period.id) {
          <button class="btn sm" [class.active]="selectedPeriod() === period.id" (click)="selectedPeriod.set(period.id)">
            {{ period.label }}
          </button>
        }
      </div>
    </div>

    @if (loading()) {
      <div class="panel"><div class="body empty">Loading statistics…</div></div>
    } @else if (statistics()) {
      @let totals = selectedTotals();
      <div class="cards4">
        <div class="card"><div class="label">Downloads</div><div class="val">{{ totals.downloads }}</div><div class="sub">{{ totals.completed }} completed · {{ totals.failed }} failed</div></div>
        <div class="card"><div class="label">Downloaded</div><div class="val">{{ formatBytes(totals.bytes_downloaded) }}</div><div class="sub">Across selected period</div></div>
        <div class="card"><div class="label">Average speed</div><div class="val">{{ formatSpeed(totals.average_speed_bps) }}</div><div class="sub">Fastest {{ formatSpeed(totals.fastest_download_bps) }}</div></div>
        <div class="card"><div class="label">Article availability</div><div class="val">{{ availability(totals) }}</div><div class="sub">{{ formatCount(totals.articles_served) }} served · {{ formatCount(totals.articles_missing) }} missing</div></div>
      </div>

      <div class="cards3">
        <div class="card"><div class="label">News server hits</div><div class="val">{{ formatCount(totals.news_server_hits) }}</div><div class="sub">Every served or missing article response</div></div>
        <div class="card"><div class="label">Articles served</div><div class="val">{{ formatCount(totals.articles_served) }}</div><div class="sub">Successful article responses</div></div>
        <div class="card"><div class="label">Articles missing</div><div class="val">{{ formatCount(totals.articles_missing) }}</div><div class="sub">Unavailable article responses</div></div>
      </div>

      <div class="panel">
        <h3>News servers <span class="hint">Lifetime, 30-day, 7-day and 24-hour counters</span></h3>
        <div class="body flush">
          <table class="data">
            <thead><tr><th>Server</th><th>Downloaded</th><th>Hits</th><th>Served</th><th>Missing</th><th>Availability</th><th>Last active</th></tr></thead>
            <tbody>
              @for (server of statistics()!.servers; track server.server_id) {
                <tr>
                  <td>{{ server.server_name || server.server_id }}</td>
                  <td>{{ formatBytes(serverBytes(server)) }}</td>
                  <td>{{ formatCount(serverServed(server) + serverMissing(server)) }}</td>
                  <td>{{ formatCount(serverServed(server)) }}</td>
                  <td>{{ formatCount(serverMissing(server)) }}</td>
                  <td>{{ availabilityCounts(serverServed(server), serverMissing(server)) }}</td>
                  <td>{{ server.last_active ? relativeTime(server.last_active) : '—' }}</td>
                </tr>
              } @empty {
                <tr><td colspan="7" class="empty">No news server activity recorded yet.</td></tr>
              }
            </tbody>
          </table>
        </div>
      </div>

      <div class="panel">
        <h3>Last 30 days <span class="hint">Daily finalized download activity</span></h3>
        <div class="body flush">
          <table class="data">
            <thead><tr><th>Date</th><th>Downloads</th><th>Downloaded</th><th>Average speed</th><th>Server hits</th><th>Availability</th></tr></thead>
            <tbody>
              @for (day of statistics()!.daily; track day.date) {
                <tr><td>{{ day.date }}</td><td>{{ day.downloads }}</td><td>{{ formatBytes(day.bytes_downloaded) }}</td><td>{{ formatSpeed(day.average_speed_bps) }}</td><td>{{ formatCount(day.news_server_hits) }}</td><td>{{ availability(day) }}</td></tr>
              } @empty {
                <tr><td colspan="6" class="empty">No finalized downloads in the last 30 days.</td></tr>
              }
            </tbody>
          </table>
        </div>
      </div>
    } @else {
      <div class="panel"><div class="body empty">Statistics could not be loaded.</div></div>
    }
  `,
  styles: [`
    :host { display: block; }
    .periods { display: flex; gap: 5px; }
    .periods .active { color: #fff; background: var(--accent); border-color: var(--accent); }
    .empty { padding: 28px !important; text-align: center; color: var(--mute); }
    @media (max-width: 900px) {
      .section-head { align-items: flex-start; gap: 12px; flex-direction: column; }
    }
  `],
})
export class StatisticsViewComponent implements OnInit {
  readonly periodOptions: readonly { id: Period; label: string }[] = [
    { id: 'today', label: '24 hours' },
    { id: 'week', label: '7 days' },
    { id: 'month', label: '30 days' },
    { id: 'lifetime', label: 'Lifetime' },
  ];
  readonly selectedPeriod = signal<Period>('month');
  readonly statistics = signal<GlobalStatistics | null>(null);
  readonly loading = signal(true);
  readonly selectedTotals = computed(() => {
    const statistics = this.statistics();
    return statistics?.[this.selectedPeriod()] ?? this.emptyPeriod();
  });

  constructor(private api: ApiService) {}

  ngOnInit(): void {
    this.api.get<GlobalStatistics>('/statistics').subscribe({
      next: statistics => { this.statistics.set(statistics); this.loading.set(false); },
      error: () => this.loading.set(false),
    });
  }

  serverBytes(server: ServerStatistics): number {
    switch (this.selectedPeriod()) {
      case 'today': return server.today_bytes;
      case 'week': return server.week_bytes;
      case 'month': return server.month_bytes;
      case 'lifetime': return server.total_bytes;
    }
  }
  serverServed(server: ServerStatistics): number {
    switch (this.selectedPeriod()) {
      case 'today': return server.today_ok;
      case 'week': return server.week_ok;
      case 'month': return server.month_ok;
      case 'lifetime': return server.total_ok;
    }
  }
  serverMissing(server: ServerStatistics): number {
    switch (this.selectedPeriod()) {
      case 'today': return server.today_fail;
      case 'week': return server.week_fail;
      case 'month': return server.month_fail;
      case 'lifetime': return server.total_fail;
    }
  }

  availability(period: StatisticsPeriod): string { return this.availabilityCounts(period.articles_served, period.articles_missing); }
  availabilityCounts(served: number, missing: number): string {
    const total = served + missing;
    return total > 0 ? `${((served / total) * 100).toFixed(2)}%` : '—';
  }

  formatBytes(bytes: number): string {
    if (!bytes) return '0 B';
    const units = ['B', 'KB', 'MB', 'GB', 'TB'];
    const index = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
    return `${(bytes / 1024 ** index).toFixed(index === 0 ? 0 : 1)} ${units[index]}`;
  }
  formatSpeed(bytes: number): string { return `${this.formatBytes(bytes)}/s`; }
  formatCount(value: number): string { return new Intl.NumberFormat().format(value || 0); }
  relativeTime(value: string): string {
    const seconds = Math.max(0, (Date.now() - new Date(value).getTime()) / 1000);
    if (seconds < 60) return 'just now';
    if (seconds < 3600) return `${Math.floor(seconds / 60)} min ago`;
    if (seconds < 86400) return `${Math.floor(seconds / 3600)} h ago`;
    return `${Math.floor(seconds / 86400)} d ago`;
  }

  private emptyPeriod(): StatisticsPeriod {
    return { downloads: 0, completed: 0, failed: 0, bytes_downloaded: 0, total_duration_secs: 0, average_speed_bps: 0, fastest_download_bps: 0, news_server_hits: 0, articles_served: 0, articles_missing: 0 };
  }
}
