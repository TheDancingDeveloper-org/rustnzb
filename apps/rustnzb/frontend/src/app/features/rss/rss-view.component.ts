import { Component, OnInit, signal, computed } from '@angular/core';
import { CommonModule } from '@angular/common';
import { FormsModule } from '@angular/forms';
import { MatSnackBar, MatSnackBarModule } from '@angular/material/snack-bar';
import { ApiService } from '../../core/services/api.service';
import { ConfirmService } from '../../shared/confirm.service';

interface RssFeed {
  name: string; url: string; poll_interval_secs: number; category: string | null;
  filter_regex: string | null; enabled: boolean; auto_download: boolean;
}

interface RssItem {
  id: string; feed_name: string; title: string; url: string | null;
  published_at: string | null; downloaded: boolean; category: string; size_bytes: number;
}

interface RssRule {
  id: string; name: string; feed_names: string[]; category: string | null;
  priority: number; match_regex: string; enabled: boolean;
}

interface FeedFormModel {
  name: string; url: string; poll_interval_secs: number; category: string;
  filter_regex: string; enabled: boolean; auto_download: boolean;
}

interface RuleFormModel {
  name: string; match_regex: string; category: string; priority: number;
  enabled: boolean; feed_names_csv: string;
}

@Component({
  selector: 'app-rss-view',
  standalone: true,
  imports: [CommonModule, FormsModule, MatSnackBarModule],
  template: `
    <!-- Stat cards -->
    <div class="cards3">
      <div class="card">
        <div class="label">Feeds</div>
        <div class="val">{{ enabledFeedCount() }} <span class="unit">active</span></div>
        <div class="sub">{{ feeds().length }} total · avg poll {{ avgPollLabel() }}</div>
      </div>
      <div class="card">
        <div class="label">Items (all time)</div>
        <div class="val">{{ items().length }}</div>
        <div class="sub">{{ downloadedCount() }} downloaded · {{ pendingCount() }} pending</div>
      </div>
      <div class="card">
        <div class="label">Rules</div>
        <div class="val">{{ enabledRuleCount() }}</div>
        <div class="sub">{{ rules().length }} total</div>
      </div>
    </div>

    <!-- ============ Feeds panel ============ -->
    <div class="panel">
      <h3>Feeds
        <span class="hint">each feed polls an RSS URL, optionally filters by regex, and auto-enqueues matches</span>
      </h3>

      @if (feedFormVisible()) {
        <div class="body edit-form">
          <div class="form">
            <label>Name</label>
            <input type="text" [(ngModel)]="feedForm.name" [disabled]="!!editingFeedName()" placeholder="Feed name" />

            <label>URL</label>
            <input type="text" [(ngModel)]="feedForm.url" placeholder="https://indexer.example/rss?apikey=…" />

            <label>Poll interval</label>
            <div class="inline">
              <input type="number" [(ngModel)]="feedForm.poll_interval_secs" /> <span style="color:var(--mute)">seconds</span>
            </div>

            <label>Category</label>
            <input type="text" [(ngModel)]="feedForm.category" placeholder="(optional)" />

            <label>Filter regex</label>
            <input type="text" [(ngModel)]="feedForm.filter_regex" placeholder="/ubuntu|debian/i (optional)" />

            <label>Options</label>
            <div style="display:flex;gap:16px">
              <label class="check"><input type="checkbox" [(ngModel)]="feedForm.enabled" /> Enabled</label>
              <label class="check"><input type="checkbox" [(ngModel)]="feedForm.auto_download" /> Auto-download matches</label>
            </div>
          </div>
          <div class="form-actions">
            <button class="btn primary" (click)="saveFeed()">{{ editingFeedName() ? 'Update' : 'Add feed' }}</button>
            <button class="btn" (click)="cancelFeedForm()">Cancel</button>
          </div>
        </div>
      }

      <div class="body flush">
        @for (f of feeds(); track f.name) {
          <div class="feed-row">
            <div class="feed-info">
              <div class="feed-name">
                {{ f.name }}
                <span class="pill" [class.ok]="f.enabled" [class.warn]="!f.enabled" style="margin-left:8px">
                  ● {{ f.enabled ? 'active' : 'paused' }}
                </span>
              </div>
              <div class="feed-url">{{ maskUrl(f.url) }}</div>
              <div class="feed-regex">
                @if (f.filter_regex) { Filter: <code>{{ f.filter_regex }}</code> · }
                category: <code>{{ f.category || '—' }}</code>
                · poll every {{ f.poll_interval_secs }}s
                @if (f.auto_download) { · auto-enqueue } @else { · manual grab }
              </div>
            </div>
            <div class="feed-actions">
              <button class="btn sm" (click)="editFeed(f)">Edit</button>
              <button class="btn sm danger" (click)="deleteFeed(f.name)">Delete</button>
            </div>
          </div>
        }
        @if (feeds().length === 0 && !feedFormVisible()) {
          <div class="empty">No feeds configured.</div>
        }
      </div>

      <div class="body" style="border-top:1px solid var(--line)">
        <button class="btn primary" (click)="showAddFeed()" [disabled]="feedFormVisible()">+ Add feed</button>
      </div>
    </div>

    <!-- ============ Rules panel ============ -->
    <div class="panel">
      <h3>Download rules
        <span class="hint">regex rules applied across feeds; sets category + priority on match</span>
      </h3>

      @if (ruleFormVisible()) {
        <div class="body edit-form">
          <div class="form">
            <label>Name</label>
            <input type="text" [(ngModel)]="ruleForm.name" placeholder="Rule name" />

            <label>Match regex</label>
            <input type="text" [(ngModel)]="ruleForm.match_regex" placeholder=".*S\\d+E\\d+.*" />

            <label>Category</label>
            <input type="text" [(ngModel)]="ruleForm.category" placeholder="(optional — sets category for matches)" />

            <label>Priority</label>
            <input type="number" [(ngModel)]="ruleForm.priority" />

            <label>Feeds</label>
            <input type="text" [(ngModel)]="ruleForm.feed_names_csv" placeholder="feed1, feed2 (blank = all feeds)" />

            <label>Options</label>
            <label class="check"><input type="checkbox" [(ngModel)]="ruleForm.enabled" /> Enabled</label>
          </div>
          <div class="form-actions">
            <button class="btn primary" (click)="saveRule()">{{ editingRuleId() ? 'Update' : 'Add rule' }}</button>
            <button class="btn" (click)="cancelRuleForm()">Cancel</button>
          </div>
        </div>
      }

      <div class="body flush">
        <table class="data">
          <thead>
            <tr>
              <th>Name</th>
              <th>Regex</th>
              <th>Category</th>
              <th>Priority</th>
              <th>Feeds</th>
              <th>Status</th>
              <th style="width:130px"></th>
            </tr>
          </thead>
          <tbody>
            @for (r of rules(); track r.id) {
              <tr>
                <td>{{ r.name }}</td>
                <td><code>{{ r.match_regex }}</code></td>
                <td>
                  @if (r.category) { <span class="tag cat">{{ r.category }}</span> }
                  @else { <span class="dim">—</span> }
                </td>
                <td>{{ r.priority }}</td>
                <td class="dim">{{ r.feed_names.length === 0 ? 'all' : r.feed_names.join(', ') }}</td>
                <td>
                  <span class="status-pill" [class]="r.enabled ? 's-ok' : 's-paused'">
                    {{ r.enabled ? 'active' : 'disabled' }}
                  </span>
                </td>
                <td>
                  <button class="row-action" (click)="editRule(r)">edit</button>
                  <button class="row-action danger" (click)="deleteRule(r.id)">del</button>
                </td>
              </tr>
            }
            @if (rules().length === 0 && !ruleFormVisible()) {
              <tr><td colspan="7" class="empty-cell">No rules configured.</td></tr>
            }
          </tbody>
        </table>
      </div>

      <div class="body" style="border-top:1px solid var(--line)">
        <button class="btn primary" (click)="showAddRule()" [disabled]="ruleFormVisible()">+ Add rule</button>
      </div>
    </div>

    <!-- ============ Recent items ============ -->
    <div class="panel">
      <h3>Recent items
        <span class="hint">last {{ items().length }} · grouped by feed</span>
      </h3>
      <div class="body flush">
        <table class="data">
          <thead>
            <tr>
              <th>Feed</th>
              <th>Title</th>
              <th>Size</th>
              <th>Published</th>
              <th>Status</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            @for (i of items(); track i.id) {
              <tr>
                <td class="dim">{{ i.feed_name }}</td>
                <td>{{ i.title }}</td>
                <td>{{ formatBytes(i.size_bytes) || '—' }}</td>
                <td class="dim">{{ relativeTime(i.published_at) }}</td>
                <td>
                  <span class="status-pill" [class]="i.downloaded ? 's-ok' : 's-q'">
                    {{ i.downloaded ? 'downloaded' : 'pending' }}
                  </span>
                </td>
                <td>
                  @if (!i.downloaded) {
                    <button class="row-action" (click)="downloadItem(i.id)">↓ grab</button>
                  }
                </td>
              </tr>
            }
            @if (items().length === 0) {
              <tr><td colspan="6" class="empty-cell">No recent items.</td></tr>
            }
          </tbody>
        </table>
      </div>
    </div>
  `,
  styles: [`
    :host { display: block; }

    .feed-row {
      display: grid; grid-template-columns: 1fr auto; gap: 10px;
      padding: 12px 16px; border-bottom: 1px solid var(--line);
    }
    .feed-row:last-child { border: none; }
    .feed-name { font-weight: 600; }
    .feed-url { color: var(--mute); font-size: 12px; font-family: ui-monospace, Menlo, monospace; margin-top: 3px; word-break: break-all; }
    .feed-regex { margin-top: 4px; font-size: 11px; color: var(--mute); }
    .feed-actions { display: flex; gap: 6px; align-items: center; }

    .edit-form {
      background: var(--panel2);
      border-bottom: 1px solid var(--line);
    }
    .form-actions { margin-top: 14px; display: flex; gap: 8px; }

    td.dim { color: var(--mute); }
    .dim { color: var(--mute); }
    .empty { padding: 20px; color: var(--mute); font-size: 13px; text-align: center; }
    .empty-cell { text-align: center; padding: 28px !important; color: var(--mute); font-size: 13px; }
  `],
})
export class RssViewComponent implements OnInit {
  feeds = signal<RssFeed[]>([]);
  rules = signal<RssRule[]>([]);
  items = signal<RssItem[]>([]);

  feedFormVisible = signal(false);
  editingFeedName = signal<string | null>(null);
  feedForm: FeedFormModel = this.emptyFeedForm();

  ruleFormVisible = signal(false);
  editingRuleId = signal<string | null>(null);
  ruleForm: RuleFormModel = this.emptyRuleForm();

  // ---- Stat-card derivations ----
  enabledFeedCount = computed(() => this.feeds().filter(f => f.enabled).length);
  enabledRuleCount = computed(() => this.rules().filter(r => r.enabled).length);
  downloadedCount = computed(() => this.items().filter(i => i.downloaded).length);
  pendingCount = computed(() => this.items().filter(i => !i.downloaded).length);
  avgPollLabel = computed(() => {
    const active = this.feeds().filter(f => f.enabled);
    if (active.length === 0) return '—';
    const avg = active.reduce((n, f) => n + f.poll_interval_secs, 0) / active.length;
    if (avg >= 3600) return Math.round(avg / 3600) + 'h';
    if (avg >= 60) return Math.round(avg / 60) + 'm';
    return Math.round(avg) + 's';
  });

  constructor(
    private api: ApiService,
    private snack: MatSnackBar,
    private confirmSvc: ConfirmService,
  ) {}

  ngOnInit(): void { this.loadAll(); }

  loadAll(): void {
    this.api.get<RssFeed[]>('/config/rss-feeds').subscribe({
      next: feeds => this.feeds.set(feeds),
      error: () => {},
    });
    this.api.get<RssRule[]>('/rss/rules').subscribe({
      next: rules => this.rules.set(rules),
      error: () => {},
    });
    this.api.get<RssItem[]>('/rss/items').subscribe({
      next: items => this.items.set(items),
      error: () => {},
    });
  }

  // -- Feed CRUD --

  showAddFeed(): void {
    this.feedForm = this.emptyFeedForm();
    this.editingFeedName.set(null);
    this.feedFormVisible.set(true);
  }

  editFeed(f: RssFeed): void {
    this.feedForm = {
      name: f.name,
      url: f.url,
      poll_interval_secs: f.poll_interval_secs,
      category: f.category || '',
      filter_regex: f.filter_regex || '',
      enabled: f.enabled,
      auto_download: f.auto_download,
    };
    this.editingFeedName.set(f.name);
    this.feedFormVisible.set(true);
  }

  cancelFeedForm(): void {
    this.feedFormVisible.set(false);
    this.editingFeedName.set(null);
  }

  saveFeed(): void {
    const body: RssFeed = {
      name: this.feedForm.name.trim(),
      url: this.feedForm.url.trim(),
      poll_interval_secs: this.feedForm.poll_interval_secs,
      category: this.feedForm.category.trim() || null,
      filter_regex: this.feedForm.filter_regex.trim() || null,
      enabled: this.feedForm.enabled,
      auto_download: this.feedForm.auto_download,
    };
    if (!body.name || !body.url) {
      this.snack.open('Name and URL are required', 'Close', { duration: 3000 });
      return;
    }
    const editing = this.editingFeedName();
    const req = editing
      ? this.api.put(`/config/rss-feeds/${encodeURIComponent(editing)}`, body)
      : this.api.post('/config/rss-feeds', body);
    req.subscribe({
      next: () => {
        this.snack.open(editing ? 'Feed updated' : 'Feed added', 'Close', { duration: 2000 });
        this.cancelFeedForm();
        this.loadAll();
      },
      error: () => this.snack.open('Failed to save feed', 'Close', { duration: 3000 }),
    });
  }

  deleteFeed(name: string): void {
    this.confirmSvc
      .confirm({
        title: `Delete feed "${name}"?`,
        message: 'This removes the feed from RSS monitoring. Existing download rules are kept but will no longer match items from it.',
        confirmLabel: 'Delete',
        danger: true,
      })
      .subscribe((ok) => {
        if (!ok) return;
        this.api.delete(`/config/rss-feeds/${encodeURIComponent(name)}`).subscribe({
          next: () => {
            this.snack.open('Feed deleted', 'Close', { duration: 2000 });
            this.loadAll();
          },
          error: () => this.snack.open('Failed to delete feed', 'Close', { duration: 3000 }),
        });
      });
  }

  // -- Rule CRUD --

  showAddRule(): void {
    this.ruleForm = this.emptyRuleForm();
    this.editingRuleId.set(null);
    this.ruleFormVisible.set(true);
  }

  editRule(r: RssRule): void {
    this.ruleForm = {
      name: r.name,
      match_regex: r.match_regex,
      category: r.category || '',
      priority: r.priority,
      enabled: r.enabled,
      feed_names_csv: r.feed_names.join(', '),
    };
    this.editingRuleId.set(r.id);
    this.ruleFormVisible.set(true);
  }

  cancelRuleForm(): void {
    this.ruleFormVisible.set(false);
    this.editingRuleId.set(null);
  }

  saveRule(): void {
    const feedNames = this.ruleForm.feed_names_csv
      .split(',')
      .map(s => s.trim())
      .filter(s => s.length > 0);
    const body = {
      name: this.ruleForm.name.trim(),
      match_regex: this.ruleForm.match_regex.trim(),
      category: this.ruleForm.category.trim() || null,
      priority: this.ruleForm.priority,
      enabled: this.ruleForm.enabled,
      feed_names: feedNames,
    };
    if (!body.name || !body.match_regex) {
      this.snack.open('Name and regex are required', 'Close', { duration: 3000 });
      return;
    }
    const editing = this.editingRuleId();
    const req = editing
      ? this.api.put(`/rss/rules/${editing}`, body)
      : this.api.post('/rss/rules', body);
    req.subscribe({
      next: () => {
        this.snack.open(editing ? 'Rule updated' : 'Rule added', 'Close', { duration: 2000 });
        this.cancelRuleForm();
        this.loadAll();
      },
      error: () => this.snack.open('Failed to save rule', 'Close', { duration: 3000 }),
    });
  }

  deleteRule(id: string): void {
    this.confirmSvc
      .confirm({
        title: 'Delete this rule?',
        message: 'Matching items will no longer be auto-downloaded once removed.',
        confirmLabel: 'Delete',
        danger: true,
      })
      .subscribe((ok) => {
        if (!ok) return;
        this.api.delete(`/rss/rules/${id}`).subscribe({
          next: () => {
            this.snack.open('Rule deleted', 'Close', { duration: 2000 });
            this.loadAll();
          },
          error: () => this.snack.open('Failed to delete rule', 'Close', { duration: 3000 }),
        });
      });
  }

  // -- Items --

  downloadItem(id: string): void {
    this.api.post(`/rss/items/${id}/download`).subscribe({
      next: () => {
        this.snack.open('Added to queue', 'Close', { duration: 2000 });
        this.loadAll();
      },
      error: () => this.snack.open('Download failed', 'Close', { duration: 3000 }),
    });
  }

  // -- Helpers --

  formatBytes(b: number): string {
    if (!b) return '';
    const k = 1024;
    const s = ['B', 'KB', 'MB', 'GB'];
    const i = Math.floor(Math.log(b) / Math.log(k));
    return (b / Math.pow(k, i)).toFixed(1) + ' ' + s[i];
  }

  /**
   * Mask obvious secrets in a feed URL before rendering. Covers common
   * apikey-style query params; anything more exotic the user can still see
   * in the edit form.
   */
  maskUrl(url: string): string {
    return url.replace(/(apikey|api_key|token|auth)=([^&]+)/gi, (_m, k) => `${k}=***`);
  }

  relativeTime(d: string | null): string {
    if (!d) return '—';
    const diff = (Date.now() - new Date(d).getTime()) / 1000;
    if (diff < 60) return 'just now';
    if (diff < 3600) return `${Math.floor(diff / 60)} min ago`;
    if (diff < 86400) return `${Math.floor(diff / 3600)} h ago`;
    return `${Math.floor(diff / 86400)} d ago`;
  }

  private emptyFeedForm(): FeedFormModel {
    return { name: '', url: '', poll_interval_secs: 900, category: '', filter_regex: '', enabled: true, auto_download: false };
  }

  private emptyRuleForm(): RuleFormModel {
    return { name: '', match_regex: '', category: '', priority: 1, enabled: true, feed_names_csv: '' };
  }
}
