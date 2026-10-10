import { Component, OnInit, OnDestroy, signal, computed } from '@angular/core';
import { EMPTY, Observable, expand, map, reduce } from 'rxjs';
import { CommonModule } from '@angular/common';
import { FormsModule } from '@angular/forms';
import { MatSnackBar, MatSnackBarModule } from '@angular/material/snack-bar';
import { ApiService } from '../../core/services/api.service';
import { HistoryEntry, StatusResponse } from '../../core/models/queue.model';
import { ConfirmService } from '../../shared/confirm.service';
import { IconComponent } from '../../shared/icon.component';
import { showHttpError } from '../../core/http/http-error';

type StatusFilter = 'all' | 'completed' | 'failed';
type TimeFilter = '7d' | '30d' | 'all';

/** Aggregates the server computes over the whole time window. */
interface HistoryStats {
  completed: number;
  completed_bytes: number;
  failed: number;
  success_pct: number;
  avg_duration_secs: number | null;
  fail_reasons: { reason: string; count: number }[];
}

interface HistoryPage {
  entries: HistoryEntry[];
  /** Entries matching the filters across all pages. */
  total?: number;
  categories?: string[];
  stats?: HistoryStats;
}

const PAGE_SIZE = 50;
const EXPORT_PAGE_SIZE = 200;

@Component({
  selector: 'app-history-view',
  standalone: true,
  imports: [CommonModule, FormsModule, MatSnackBarModule, IconComponent],
  template: `
    <!-- Stat cards -->
    <div class="cards4">
      <div class="card">
        <div class="label">Completed · {{ statCards().windowLabel }}</div>
        <div class="val">{{ statCards().completed }}</div>
        <div class="sub">{{ formatBytes(statCards().completedBytes) }}</div>
      </div>
      <div class="card">
        <div class="label">Failed · {{ statCards().windowLabel }}</div>
        <div class="val">{{ statCards().failed }}</div>
        <div class="sub">{{ statCards().failReasons }}</div>
      </div>
      <div class="card">
        <div class="label">Success rate</div>
        <div class="val">{{ statCards().successPct }}%</div>
        <div class="bar green"><div [style.width.%]="statCards().successPct"></div></div>
        <div class="sub">Of recent jobs</div>
      </div>
      <div class="card">
        <div class="label">Avg job duration</div>
        <div class="val">{{ statCards().avgDurationLabel }}</div>
        <div class="sub">Download + post-processing</div>
      </div>
    </div>

    <!-- History panel -->
    <div class="panel">
      <h3>History
        <span class="hint">{{ rangeLabel() }}</span>
      </h3>
      <div class="body">
        <div class="search-bar">
          <input placeholder="Filter name…" [(ngModel)]="nameFilter" (ngModelChange)="onNameFilterChanged()" />
          <select [(ngModel)]="filterStatus" (ngModelChange)="onFiltersChanged()">
            <option value="all">All statuses</option>
            <option value="completed">Completed</option>
            <option value="failed">Failed</option>
          </select>
          <select [(ngModel)]="filterCategory" (ngModelChange)="onFiltersChanged()">
            <option value="">All categories</option>
            @for (cat of categoryOptions(); track cat) { <option [value]="cat">{{ cat }}</option> }
          </select>
          <select [(ngModel)]="filterTime" (ngModelChange)="onFiltersChanged()">
            <option value="7d">Last 7 days</option>
            <option value="30d">Last 30 days</option>
            <option value="all">All time</option>
          </select>
          <button class="btn ghost" (click)="exportCsv()" [disabled]="exporting()" title="Exports every entry matching the filters">
            {{ exporting() ? 'Exporting…' : 'Export CSV' }}
          </button>
          @if (total() > 0) {
            <button class="btn danger" (click)="clearAll()">Clear all</button>
          }
        </div>
      </div>
      <div class="body flush">
        <table class="data">
          <thead>
            <tr>
              <th class="name-column">Name</th>
              <th>Category</th>
              <th>Size</th>
              <th>Avg speed</th>
              <th>Duration</th>
              <th>Completed</th>
              <th>Status</th>
              <th style="width:1%"></th>
            </tr>
          </thead>
          <tbody>
            @for (e of filteredEntries(); track e.id) {
              <tr
                class="history-row"
                [class.selected]="selectedId() === e.id"
                tabindex="0"
                [attr.aria-expanded]="selectedId() === e.id"
                (click)="selectEntry(e)"
                (keydown.enter)="selectEntry(e)"
                (keydown.space)="$event.preventDefault(); selectEntry(e)"
              >
                <td class="name-cell">
                  <div class="e-name" [class.dim]="e.status === 'failed'" [title]="e.name">{{ e.name }}</div>
                  @if (e.error_message) {
                    <div class="e-err" [title]="e.error_message">{{ e.error_message }}</div>
                  }
                </td>
                <td>
                  @if (e.category) { <span class="tag cat">{{ e.category }}</span> }
                </td>
                <td>{{ formatBytes(e.total_bytes) }}</td>
                <td>{{ formatSpeed(averageSpeed(e)) }}</td>
                <td>{{ formatDuration(e.added_at, e.completed_at) }}</td>
                <td>{{ relativeTime(e.completed_at) }}</td>
                <td>
                  <span class="status-pill" [class]="e.status === 'completed' ? 's-ok' : 's-fail'">
                    {{ e.status }}
                  </span>
                </td>
                <td class="actions-cell">
                  <div class="actions">
                    @if (e.status === 'failed') {
                      <button class="row-action warn" (click)="$event.stopPropagation(); retry(e.id)">
                        <app-icon name="retry" [size]="11" /> retry
                      </button>
                    }
                    @if (e.status === 'completed' && webdavEnabled()) {
                      <button class="row-action media" (click)="$event.stopPropagation(); addToMedia(e.id)" title="Add to Media Library">
                        <app-icon name="play" [size]="11" /> media
                      </button>
                    }
                    <button class="row-action" (click)="$event.stopPropagation(); openOutput(e)">open</button>
                    <button class="row-action danger" (click)="$event.stopPropagation(); remove(e.id)" aria-label="Delete">
                      <app-icon name="close" [size]="11" />
                    </button>
                  </div>
                </td>
              </tr>
              @if (selectedId() === e.id) {
                @let detail = selectedEntry() || e;
                <tr class="detail-row">
                  <td colspan="8">
                    @if (detailLoading()) {
                      <div class="detail-loading">Loading download details…</div>
                    } @else {
                      <div class="detail-panel">
                        <div class="detail-head">
                          <div>
                            <div class="detail-title">{{ detail.name }}</div>
                            <div class="detail-path">{{ detail.output_dir || 'No output path recorded' }}</div>
                          </div>
                          <button class="row-action" (click)="closeDetails()" aria-label="Close details">close</button>
                        </div>
                        <div class="detail-metrics">
                          <div><span>Downloaded</span><b>{{ formatBytes(detail.downloaded_bytes) }} / {{ formatBytes(detail.total_bytes) }}</b></div>
                          <div><span>Average speed</span><b>{{ formatSpeed(averageSpeed(detail)) }}</b></div>
                          <div><span>Total duration</span><b>{{ formatDuration(detail.added_at, detail.completed_at) }}</b></div>
                          <div><span>Articles served</span><b>{{ articleServed(detail) }}</b></div>
                          <div><span>Articles missing</span><b>{{ articleMissing(detail) }}</b></div>
                          <div><span>Availability</span><b>{{ availability(detail) }}</b></div>
                        </div>

                        @if (detail.server_stats.length > 0) {
                          <h4>News server usage</h4>
                          <table class="detail-table">
                            <thead><tr><th>Server</th><th>Hits</th><th>Served</th><th>Missing</th><th>Downloaded</th></tr></thead>
                            <tbody>
                              @for (server of detail.server_stats; track server.server_id) {
                                <tr>
                                  <td>{{ server.server_name || server.server_id }}</td>
                                  <td>{{ server.articles_downloaded + server.articles_failed }}</td>
                                  <td>{{ server.articles_downloaded }}</td>
                                  <td>{{ server.articles_failed }}</td>
                                  <td>{{ formatBytes(server.bytes_downloaded) }}</td>
                                </tr>
                              }
                            </tbody>
                          </table>
                        }

                        @if (detail.stages.length > 0) {
                          <h4>Processing stages</h4>
                          <div class="stages">
                            @for (stage of detail.stages; track stage.name) {
                              <div class="stage">
                                <span class="status-pill" [class]="stage.status === 'success' ? 's-ok' : stage.status === 'failed' ? 's-fail' : 's-paused'">{{ stage.status }}</span>
                                <b>{{ stage.name }}</b>
                                <span>{{ formatStageDuration(stage.duration_secs) }}</span>
                                @if (stage.message) { <span class="stage-message">{{ stage.message }}</span> }
                              </div>
                            }
                          </div>
                        }
                        @if (detail.error_message) {
                          <div class="detail-error">{{ detail.error_message }}</div>
                        }
                      </div>
                    }
                  </td>
                </tr>
              }
            }

            @if (loading()) {
              <tr>
                <td colspan="8" class="empty-cell">Loading…</td>
              </tr>
            } @else if (filteredEntries().length === 0) {
              <tr>
                <td colspan="8" class="empty-cell">
                  @if (narrowingFilters()) {
                    No entries match the current filter.
                  } @else if (filterTime === 'all') {
                    No download history yet. Finished jobs will show up here.
                  } @else {
                    No downloads finished in the last {{ statCards().windowLabel }}.
                  }
                </td>
              </tr>
            }
          </tbody>
        </table>
      </div>
      @if (hasPrev() || hasNext()) {
        <div class="pager">
          <button class="btn ghost" (click)="prevPage()" [disabled]="!hasPrev()">‹ Newer</button>
          <span class="hint">{{ rangeLabel() }}</span>
          <button class="btn ghost" (click)="nextPage()" [disabled]="!hasNext()">Older ›</button>
        </div>
      }
    </div>
  `,
  styles: [`
    :host { display: block; }
    .name-column, .name-cell { width: 36%; }
    .name-cell { max-width: 0; }
    .e-name, .e-err {
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .e-name { color: var(--text); font-size: 13px; }
    .e-name.dim { color: var(--mute); }
    .e-err { color: var(--danger); font-size: 11px; margin-top: 2px; opacity: .8; }
    .empty-cell {
      text-align: center; padding: 36px 20px !important;
      color: var(--mute); font-size: 13px;
    }
    .row-action.media { color: var(--accent, #7c6af7); border-color: var(--accent, #7c6af7); }
    .actions-cell {
      white-space: nowrap;
    }
    .actions {
      display: flex;
      align-items: center;
      justify-content: flex-end;
      gap: 2px;
    }
    .pager {
      display: flex; align-items: center; justify-content: flex-end; gap: 10px;
      padding: 10px 14px; border-top: 1px solid var(--line);
    }
    .history-row { cursor: pointer; }
    .history-row:focus { outline: 1px solid var(--accent); outline-offset: -1px; }
    .history-row.selected td { background: var(--row-hover); border-bottom-color: transparent; }
    .detail-row > td { padding: 0 !important; background: var(--panel2); }
    .detail-loading { padding: 22px; color: var(--mute); }
    .detail-panel { padding: 16px 18px 18px; border-bottom: 1px solid var(--line); }
    .detail-head { display: flex; justify-content: space-between; gap: 16px; margin-bottom: 14px; }
    .detail-title { font-weight: 600; font-size: 14px; }
    .detail-path { color: var(--mute); font-size: 11px; margin-top: 3px; word-break: break-all; }
    .detail-metrics { display: grid; grid-template-columns: repeat(6, minmax(0, 1fr)); gap: 8px; }
    .detail-metrics > div { padding: 9px; background: var(--panel); border: 1px solid var(--line); border-radius: 5px; }
    .detail-metrics span { display: block; color: var(--mute); font-size: 10px; text-transform: uppercase; letter-spacing: .3px; }
    .detail-metrics b { display: block; margin-top: 3px; font-size: 12px; }
    h4 { margin: 16px 0 7px; color: var(--mute); font-size: 11px; text-transform: uppercase; letter-spacing: .4px; }
    .detail-table { width: 100%; border-collapse: collapse; font-size: 12px; }
    .detail-table th, .detail-table td { padding: 6px 8px; border: 1px solid var(--line); text-align: left; }
    .detail-table th { color: var(--mute); font-weight: 500; }
    .stages { display: flex; flex-direction: column; gap: 5px; }
    .stage { display: grid; grid-template-columns: 72px 150px 70px 1fr; gap: 8px; align-items: center; font-size: 12px; }
    .stage > span:not(.status-pill) { color: var(--mute); }
    .stage-message { overflow-wrap: anywhere; }
    .detail-error { margin-top: 14px; padding: 9px 11px; color: var(--danger); background: var(--fail-bg); border-radius: 5px; font-size: 12px; }
    @media (max-width: 1000px) {
      .detail-metrics { grid-template-columns: repeat(3, minmax(0, 1fr)); }
    }
  `],
})
export class HistoryViewComponent implements OnInit, OnDestroy {
  loading = signal(true);
  /** The current page of entries, already filtered by the server. */
  entries = signal<HistoryEntry[]>([]);
  /** Entries matching the filters across all pages. */
  total = signal(0);
  offset = signal(0);
  readonly pageSize = PAGE_SIZE;
  exporting = signal(false);
  private categories = signal<string[] | null>(null);
  private stats = signal<HistoryStats | null>(null);
  private loadSeq = 0;
  private nameDebounce: ReturnType<typeof setTimeout> | null = null;
  selectedId = signal<string | null>(null);
  selectedEntry = signal<HistoryEntry | null>(null);
  detailLoading = signal(false);
  webdavEnabled = signal(false);
  filterStatus: StatusFilter = 'all';
  filterCategory = '';
  filterTime: TimeFilter = '7d';
  nameFilter = '';

  private pollTimer: ReturnType<typeof setInterval> | null = null;

  constructor(
    private api: ApiService,
    private snack: MatSnackBar,
    private confirmSvc: ConfirmService,
  ) {}

  ngOnInit(): void {
    this.load();
    this.api.get<StatusResponse>('/status').subscribe({
      next: s => this.webdavEnabled.set(!!s.webdav_enabled),
      error: () => {},
    });
    this.pollTimer = setInterval(() => this.load(), 5000);
  }

  ngOnDestroy(): void {
    if (this.pollTimer) clearInterval(this.pollTimer);
    if (this.nameDebounce) clearTimeout(this.nameDebounce);
  }

  /** Query parameters for the active filters (shared by paging and export). */
  private filterParams(): Record<string, string> {
    const params: Record<string, string> = {};
    if (this.filterStatus !== 'all') params['status'] = this.filterStatus;
    if (this.filterCategory) params['category'] = this.filterCategory;
    const name = this.nameFilter.trim();
    if (name) params['search'] = name;
    if (this.filterTime !== 'all') params['days'] = this.filterTime === '7d' ? '7' : '30';
    return params;
  }

  load(): void {
    const seq = ++this.loadSeq;
    const params = { offset: String(this.offset()), limit: String(PAGE_SIZE), ...this.filterParams() };
    this.api.get<HistoryPage>('/history', params).subscribe({
      next: r => {
        // A slower response for superseded filters/page must not win.
        if (seq !== this.loadSeq) return;
        const entries = r.entries || [];
        const total = r.total ?? entries.length;
        // The page we were on vanished (deletes, retention): go to the last one.
        if (entries.length === 0 && this.offset() > 0 && this.offset() >= total) {
          this.offset.set(Math.max(0, Math.ceil(total / PAGE_SIZE) - 1) * PAGE_SIZE);
          this.load();
          return;
        }
        this.entries.set(entries);
        this.total.set(total);
        this.categories.set(r.categories ?? null);
        this.stats.set(r.stats ?? null);
        this.loading.set(false);
      },
      error: () => this.loading.set(false),
    });
  }

  onFiltersChanged(): void {
    this.offset.set(0);
    this.closeDetails();
    this.load();
  }

  onNameFilterChanged(): void {
    if (this.nameDebounce) clearTimeout(this.nameDebounce);
    this.nameDebounce = setTimeout(() => this.onFiltersChanged(), 250);
  }

  hasPrev(): boolean {
    return this.offset() > 0;
  }

  hasNext(): boolean {
    return this.offset() + PAGE_SIZE < this.total();
  }

  prevPage(): void {
    if (!this.hasPrev()) return;
    this.offset.set(Math.max(0, this.offset() - PAGE_SIZE));
    this.load();
  }

  nextPage(): void {
    if (!this.hasNext()) return;
    this.offset.set(this.offset() + PAGE_SIZE);
    this.load();
  }

  rangeLabel(): string {
    const total = this.total();
    if (total === 0) return '0 of 0';
    const start = Math.min(this.offset() + 1, total);
    const end = Math.min(this.offset() + PAGE_SIZE, total);
    return `${start}–${end} of ${total}`;
  }

  /** Filters other than the time window are narrowing the list. */
  narrowingFilters(): boolean {
    return this.filterStatus !== 'all' || !!this.filterCategory || !!this.nameFilter.trim();
  }

  selectEntry(entry: HistoryEntry): void {
    if (this.selectedId() === entry.id) {
      this.closeDetails();
      return;
    }
    this.selectedId.set(entry.id);
    this.selectedEntry.set(entry);
    this.detailLoading.set(true);
    this.api.get<HistoryEntry>(`/history/${encodeURIComponent(entry.id)}`).subscribe({
      next: detail => {
        this.selectedEntry.set(detail);
        this.detailLoading.set(false);
      },
      error: () => this.detailLoading.set(false),
    });
  }

  closeDetails(): void {
    this.selectedId.set(null);
    this.selectedEntry.set(null);
    this.detailLoading.set(false);
  }

  /** All categories in history (server-provided), not just the visible page. */
  categoryOptions = computed(
    () =>
      this.categories() ??
      Array.from(new Set(this.entries().map(e => e.category).filter(c => !!c))).sort(),
  );

  /** The current page; filtering happens server-side. */
  filteredEntries(): HistoryEntry[] {
    return this.entries();
  }

  /**
   * The 4 stat cards at the top. The server computes them over the whole
   * time window (ignoring the status/category/name filters) so the
   * success-rate card stays meaningful when the user filters to failures.
   */
  statCards = computed(() => {
    const st = this.stats();
    const failed = st?.failed ?? 0;
    const topReasons = (st?.fail_reasons ?? [])
      .slice(0, 2)
      .map(r => `${r.count} ${r.reason}`)
      .join(' · ');
    return {
      windowLabel: this.filterTime === 'all' ? 'all time' : this.filterTime === '7d' ? '7 days' : '30 days',
      completed: st?.completed ?? 0,
      completedBytes: st?.completed_bytes ?? 0,
      failed,
      failReasons: failed === 0 || !topReasons ? 'none' : topReasons,
      successPct: st?.success_pct ?? 0,
      avgDurationLabel:
        st?.avg_duration_secs != null ? this.formatShortDuration(st.avg_duration_secs) : '—',
    };
  });

  retry(id: string): void {
    this.api.post(`/history/${id}/retry`).subscribe({
      next: () => {
        this.load();
        this.snack.open('Retrying…', 'Close', { duration: 2000 });
      },
      error: (err) => showHttpError(this.snack, err, 'Failed to retry job'),
    });
  }

  addToMedia(id: string): void {
    this.api.post(`/dav/add?id=${id}`).subscribe({
      next: () => this.snack.open('Queued for Media Library', 'Close', { duration: 3000 }),
      error: (err) => showHttpError(this.snack, err, 'Failed to add to Media Library'),
    });
  }

  remove(id: string): void {
    this.api.delete(`/history/${id}`).subscribe({
      next: () => this.load(),
      error: (err) => showHttpError(this.snack, err, 'Failed to delete history entry'),
    });
  }

  clearAll(): void {
    this.confirmSvc
      .confirm({
        title: 'Clear all history?',
        message: 'This permanently deletes every history entry. This cannot be undone.',
        confirmLabel: 'Clear all',
        danger: true,
      })
      .subscribe((ok) => {
        if (!ok) return;
        this.api.delete('/history').subscribe({
          next: () => {
            this.load();
            this.snack.open('History cleared', 'Close', { duration: 2000 });
          },
          error: (err) => showHttpError(this.snack, err, 'Failed to clear history'),
        });
      });
  }

  openOutput(e: HistoryEntry): void {
    // No server endpoint for "reveal in file manager"; surface the path.
    this.snack.open(e.output_dir || '(no output path recorded)', 'Close', { duration: 5000 });
  }

  /** Every entry matching the current filters, fetched page by page. */
  fetchAllMatching(): Observable<HistoryEntry[]> {
    const filters = this.filterParams();
    const page = (offset: number) =>
      this.api
        .get<HistoryPage>('/history', { offset: String(offset), limit: String(EXPORT_PAGE_SIZE), ...filters })
        .pipe(map(r => ({ offset, entries: r.entries || [], total: r.total ?? 0 })));
    return page(0).pipe(
      expand(p => {
        const next = p.offset + p.entries.length;
        return p.entries.length > 0 && next < p.total ? page(next) : EMPTY;
      }),
      reduce((all, p) => all.concat(p.entries), [] as HistoryEntry[]),
    );
  }

  exportCsv(): void {
    if (this.exporting()) return;
    this.exporting.set(true);
    this.fetchAllMatching().subscribe({
      next: entries => {
        this.exporting.set(false);
        this.downloadCsv(entries);
      },
      error: () => {
        this.exporting.set(false);
        this.snack.open('Failed to export history', 'Close', { duration: 3000 });
      },
    });
  }

  private downloadCsv(entries: HistoryEntry[]): void {
    const rows = [['name', 'category', 'size_bytes', 'average_speed_bps', 'status', 'added_at', 'completed_at', 'error']];
    for (const e of entries) {
      rows.push([
        e.name, e.category || '', String(e.total_bytes), String(this.averageSpeed(e)), e.status,
        e.added_at, e.completed_at, e.error_message || '',
      ]);
    }
    const csv = rows.map(r => r.map(c => `"${(c || '').replace(/"/g, '""')}"`).join(',')).join('\n');
    const blob = new Blob([csv], { type: 'text/csv' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `rustnzb-history-${new Date().toISOString().slice(0, 10)}.csv`;
    a.click();
    URL.revokeObjectURL(url);
  }

  // ---- Formatting ----

  formatBytes(b: number): string {
    if (b === 0) return '0 B';
    const k = 1024;
    const s = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.min(4, Math.floor(Math.log(b) / Math.log(k)));
    return (b / Math.pow(k, i)).toFixed(1) + ' ' + s[i];
  }

  averageSpeed(entry: HistoryEntry): number {
    if (entry.average_speed_bps != null) return entry.average_speed_bps;
    const seconds = (new Date(entry.completed_at).getTime() - new Date(entry.added_at).getTime()) / 1000;
    return seconds > 0 ? Math.round(entry.downloaded_bytes / seconds) : 0;
  }

  formatSpeed(bps: number): string {
    return `${this.formatBytes(bps)}/s`;
  }

  articleServed(entry: HistoryEntry): number {
    return entry.articles_served ?? entry.server_stats.reduce((total, server) => total + server.articles_downloaded, 0);
  }

  articleMissing(entry: HistoryEntry): number {
    return entry.articles_missing ?? entry.server_stats.reduce((total, server) => total + server.articles_failed, 0);
  }

  availability(entry: HistoryEntry): string {
    const served = this.articleServed(entry);
    const total = served + this.articleMissing(entry);
    return total > 0 ? `${((served / total) * 100).toFixed(2)}%` : '—';
  }

  formatStageDuration(seconds: number | undefined): string {
    return seconds != null && Number.isFinite(seconds) ? this.formatShortDuration(seconds) : '—';
  }

  formatDuration(start: string, end: string): string {
    if (!start || !end) return '—';
    const ms = new Date(end).getTime() - new Date(start).getTime();
    if (ms <= 0) return '—';
    return this.formatShortDuration(ms / 1000);
  }

  formatShortDuration(secs: number): string {
    const h = Math.floor(secs / 3600);
    const m = Math.floor((secs % 3600) / 60);
    const s = Math.floor(secs % 60);
    if (h > 0) return `${h}h ${m}m`;
    if (m > 0) return `${m}m ${s}s`;
    return `${s}s`;
  }

  relativeTime(d: string): string {
    if (!d) return '—';
    const diff = (Date.now() - new Date(d).getTime()) / 1000;
    if (diff < 60) return 'just now';
    if (diff < 3600) return `${Math.floor(diff / 60)} min ago`;
    if (diff < 86400) return `${Math.floor(diff / 3600)} h ago`;
    if (diff < 86400 * 2) return 'yesterday';
    return `${Math.floor(diff / 86400)} d ago`;
  }
}
