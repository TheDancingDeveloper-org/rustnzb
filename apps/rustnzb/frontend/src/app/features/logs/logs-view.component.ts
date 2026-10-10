import { Component, OnInit, OnDestroy, signal, ElementRef, ViewChild, computed } from '@angular/core';
import { CommonModule } from '@angular/common';
import { FormsModule } from '@angular/forms';
import { ApiService } from '../../core/services/api.service';

interface LogEntry {
  seq: number;
  level: string;
  message: string;
  timestamp: string;
  target?: string;
}

interface LogsResponse {
  entries: LogEntry[];
  latest_seq?: number;
  /** Changes whenever the server restarts (seq numbers then restart at 0). */
  boot_id?: string;
}

@Component({
  selector: 'app-logs-view',
  standalone: true,
  imports: [CommonModule, FormsModule],
  template: `
    <div class="panel">
      <h3>Live logs
        <span class="hint">tracing subscriber · {{ follow() ? 'following' : 'paused' }} · {{ entries().length }} lines</span>
      </h3>
      <div class="body">
        <div class="search-bar">
          <input
            placeholder="Filter… (regex ok)"
            [ngModel]="filter()"
            (ngModelChange)="filter.set($event)"
          />
          <select [ngModel]="levelFilter()" (ngModelChange)="levelFilter.set($event)">
            <option value="">All levels</option>
            <option value="ERROR">ERROR</option>
            <option value="WARN">WARN</option>
            <option value="INFO">INFO</option>
            <option value="DEBUG">DEBUG</option>
            <option value="TRACE">TRACE</option>
          </select>
          <button class="btn" [class.primary]="follow()" (click)="toggleFollow()">
            {{ follow() ? 'Following ●' : 'Paused' }}
          </button>
          <button class="btn ghost" (click)="clear()">Clear</button>
          <button class="btn ghost" (click)="download()">Download</button>
        </div>
      </div>

      <div class="body flush logs" #logContainer>
        @for (e of visibleEntries(); track e.seq) {
          <div class="l">
            <span class="t">{{ formatTs(e.timestamp) }}</span>
            <span class="lv" [class]="levelClass(e.level)">{{ e.level }}</span>
            <span class="tgt">{{ e.target || '—' }}</span>
            <span class="msg">{{ e.message }}</span>
          </div>
        }
        @if (entries().length === 0) {
          <div class="empty">No log entries yet.</div>
        }
        @if (entries().length > 0 && visibleEntries().length === 0) {
          <div class="empty">No lines match the current filter.</div>
        }
      </div>
    </div>
  `,
  styles: [`
    :host { display: block; }

    .logs {
      font: 12px/1.5 ui-monospace, Menlo, Consolas, monospace;
      padding: 10px 14px;
      max-height: calc(100vh - 260px);
      overflow: auto;
    }
    .logs .l {
      display: grid;
      grid-template-columns: 90px 60px 150px 1fr;
      gap: 10px;
      padding: 2px 0;
      border-bottom: 1px dashed transparent;
    }
    .logs .l:hover { background: rgba(255,255,255,.02); }
    .logs .t { color: var(--mute); }
    .logs .lv { font-weight: 600; }
    .logs .lv.info  { color: var(--accent); }
    .logs .lv.warn  { color: var(--warn); }
    .logs .lv.err   { color: var(--danger); }
    .logs .lv.dbg   { color: var(--mute); }
    .logs .tgt { color: var(--purple); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .logs .msg { color: var(--text); word-break: break-word; }
    .empty { padding: 24px; text-align: center; color: var(--mute); font-size: 13px; }
  `],
})
export class LogsViewComponent implements OnInit, OnDestroy {
  entries = signal<LogEntry[]>([]);
  filter = signal('');
  levelFilter = signal('');
  follow = signal(true);

  /** Highest seq received so far; null until the first entry (seq 0 is valid). */
  private lastSeq: number | null = null;
  /** Server log-buffer identity; changes when the server process restarts. */
  private bootId: string | null = null;
  private pollTimer: ReturnType<typeof setInterval> | null = null;

  @ViewChild('logContainer') logContainer!: ElementRef<HTMLElement>;

  constructor(private api: ApiService) {}

  ngOnInit(): void {
    this.loadLogs();
    this.pollTimer = setInterval(() => {
      if (this.follow()) this.loadLogs();
    }, 2000);
  }

  ngOnDestroy(): void {
    if (this.pollTimer) clearInterval(this.pollTimer);
  }

  loadLogs(): void {
    const params: Record<string, string> = {};
    if (this.lastSeq !== null) params['after_seq'] = String(this.lastSeq);
    this.api.get<LogsResponse>('/logs', params).subscribe({
      next: r => {
        if (this.serverRestarted(r)) {
          // Sequence numbers restarted at 0 in a new process, so our cursor
          // would filter out every new line. Start over from the new buffer.
          this.bootId = r.boot_id ?? null;
          this.lastSeq = null;
          this.entries.set([]);
          this.loadLogs();
          return;
        }
        if (r.boot_id) this.bootId = r.boot_id;
        if (r.entries?.length) {
          const all = [...this.entries(), ...r.entries].slice(-1000);
          this.entries.set(all);
          this.lastSeq = r.entries[r.entries.length - 1].seq;
          setTimeout(() => this.scrollToBottom(), 50);
        }
      },
      error: () => {},
    });
  }

  private serverRestarted(r: LogsResponse): boolean {
    if (this.lastSeq === null) return false;
    if (r.boot_id && this.bootId && r.boot_id !== this.bootId) return true;
    // Older servers send no boot_id: a cursor ahead of the server's newest
    // seq can only mean the counter was reset.
    return typeof r.latest_seq === 'number' && r.latest_seq < this.lastSeq;
  }

  private scrollToBottom(): void {
    const el = this.logContainer?.nativeElement;
    if (el) el.scrollTop = el.scrollHeight;
  }

  /**
   * Entries after the filter + level selectors. Live-derived so the view
   * reacts to ngModel changes on the next change detection cycle.
   */
  visibleEntries = computed(() => {
    const rx = this.compiledFilter();
    const lvl = this.levelFilter();
    return this.entries().filter(e => {
      if (lvl && e.level.toUpperCase() !== lvl) return false;
      if (rx && !rx.test(e.message) && !rx.test(e.target || '')) return false;
      return true;
    });
  });

  private compiledFilter(): RegExp | null {
    const f = this.filter().trim();
    if (!f) return null;
    try { return new RegExp(f, 'i'); }
    catch { return new RegExp(f.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'i'); }
  }

  toggleFollow(): void {
    this.follow.set(!this.follow());
    if (this.follow()) {
      this.loadLogs();
      setTimeout(() => this.scrollToBottom(), 50);
    }
  }

  clear(): void {
    this.entries.set([]);
  }

  download(): void {
    const lines = this.entries().map(e => `${e.timestamp} ${e.level} ${e.target || ''} ${e.message}`);
    const blob = new Blob([lines.join('\n')], { type: 'text/plain' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `rustnzb-logs-${new Date().toISOString().replace(/[:.]/g, '-')}.log`;
    a.click();
    URL.revokeObjectURL(url);
  }

  formatTs(ts: string): string {
    // Keep only HH:MM:SS.fff — full date wastes column width.
    if (!ts) return '';
    const m = ts.match(/(\d{2}:\d{2}:\d{2}(?:\.\d+)?)/);
    return m ? m[1].slice(0, 12) : ts;
  }

  levelClass(level: string): string {
    const l = level.toUpperCase();
    if (l === 'ERROR') return 'err';
    if (l === 'WARN' || l === 'WARNING') return 'warn';
    if (l === 'INFO') return 'info';
    return 'dbg';
  }
}
