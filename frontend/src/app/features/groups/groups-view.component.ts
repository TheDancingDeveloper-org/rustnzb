import { Component, OnInit, signal, computed } from '@angular/core';
import { CommonModule } from '@angular/common';
import { FormsModule } from '@angular/forms';
import { MatSnackBar, MatSnackBarModule } from '@angular/material/snack-bar';
import { MatDialog, MatDialogModule } from '@angular/material/dialog';
import { GroupService } from '../../core/services/group.service';
import { GroupRow, HeaderRow } from '../../core/models/group.model';
import { GroupBrowserDialogComponent } from './group-browser-dialog.component';

@Component({
  selector: 'app-groups-view',
  standalone: true,
  imports: [CommonModule, FormsModule, MatSnackBarModule, MatDialogModule],
  template: `
    <!-- Top search panel -->
    <div class="panel">
      <h3>
        Search Usenet headers
        <span class="hint">SQLite FTS5 over XOVER-fetched headers</span>
      </h3>
      <div class="body">
        <div class="search-bar">
          <input
            placeholder="Search title, poster, subject…"
            [(ngModel)]="searchQuery"
            (keyup.enter)="searchHeaders()"
          />
          <select [(ngModel)]="groupFilter">
            <option value="">All subscribed groups</option>
            @for (g of groups(); track g.id) {
              <option [value]="g.name">{{ g.name }}</option>
            }
          </select>
          <button class="btn primary" (click)="searchHeaders()">Search</button>
          <button class="btn" (click)="openBrowseDialog()">+ Subscribe</button>
        </div>
      </div>
    </div>

    <div class="shell">
      <!-- Left: subscribed groups -->
      <aside class="side">
        <div class="side-head">Subscribed ({{ groups().length }})</div>
        <div class="side-filter">
          <input [(ngModel)]="groupNameFilter" placeholder="Filter…" />
        </div>
        <div class="side-list">
          @for (g of filteredGroups(); track g.id) {
            <div class="g" [class.active]="selectedGroup()?.id === g.id" (click)="selectGroup(g)">
              <div class="name">{{ g.name }}</div>
              <div class="cnt">
                {{ g.article_count || 0 }} headers
                @if (g.unread_count > 0) {
                  · <span class="new">{{ g.unread_count }} new</span>
                }
              </div>
            </div>
          }
          @if (groups().length === 0) {
            <div class="empty">
              <button class="row-action" (click)="openBrowseDialog()">+ Subscribe to groups</button>
            </div>
          }
        </div>
      </aside>

      <!-- Right: results + thread preview -->
      <div class="main">
        @if (!selectedGroup()) {
          <div class="panel">
            <div class="body ctr">
              <div class="big-hint">
                Pick a group on the left to browse headers, or use the search bar at the top to
                query across all subscribed groups.
              </div>
            </div>
          </div>
        } @else {
          <!-- Results panel -->
          <div class="panel">
            <h3>
              Results — <code>{{ selectedGroup()!.name }}</code>
              @if (searchQuery) {
                · "{{ searchQuery }}"
              }
              <span class="hint">
                {{ headerTotal() }} match{{ headerTotal() === 1 ? '' : 'es' }}
                @if (newAvailable() > 0) {
                  · <span class="new">{{ newAvailable() }} new to fetch</span>
                }
              </span>
              <button class="btn sm" (click)="fetchHeaders()" [disabled]="fetching()">
                {{ fetching() ? 'Fetching…' : '↻ Fetch' }}
              </button>
              <button class="btn sm" (click)="markAllRead()">✓ Mark read</button>
            </h3>
            <div class="body flush">
              <table class="data">
                <thead>
                  <tr>
                    <th style="width:32px">
                      <input
                        type="checkbox"
                        [checked]="allSelected()"
                        (change)="toggleSelectAll()"
                      />
                    </th>
                    <th style="width:48%">Subject</th>
                    <th>Author</th>
                    <th>Size</th>
                    <th>Date</th>
                    <th></th>
                  </tr>
                </thead>
                <tbody>
                  @for (h of headers(); track h.id) {
                    <tr [class.unread]="!h.read" (click)="selectArticle(h)">
                      <td (click)="$event.stopPropagation()">
                        <input
                          type="checkbox"
                          [checked]="isSelected(h.message_id)"
                          (change)="toggleSelect(h.message_id)"
                        />
                      </td>
                      <td class="subj">{{ h.subject }}</td>
                      <td class="dim">{{ h.author }}</td>
                      <td>{{ formatBytes(h.bytes) }}</td>
                      <td class="dim">{{ h.date }}</td>
                      <td class="actions">
                        <button
                          class="row-action"
                          (click)="selectArticle(h); $event.stopPropagation()"
                        >
                          view
                        </button>
                      </td>
                    </tr>
                  }
                  @if (headers().length === 0 && !fetching()) {
                    <tr>
                      <td colspan="6" class="empty-cell">
                        No headers.
                        @if (newAvailable() > 0) {
                          Click <b>↻ Fetch</b> to pull {{ newAvailable() }} new.
                        }
                      </td>
                    </tr>
                  }
                </tbody>
              </table>
            </div>
            @if (headerTotal() > headers().length) {
              <div class="body load-more">
                <button class="btn sm" (click)="loadMore()">Load more…</button>
              </div>
            }
            @if (selectedIds().length > 0) {
              <div class="body download-bar">
                <span
                  >{{ selectedIds().length }} selected · {{ formatBytes(selectedBytes()) }}</span
                >
                <span class="spacer"></span>
                <button class="btn primary" (click)="downloadSelected()">
                  ↓ Download selected
                </button>
              </div>
            }
          </div>

          <!-- Article preview -->
          @if (previewHeader(); as ph) {
            <div class="panel">
              <h3>
                Article preview
                <span class="hint"
                  ><code>{{ ph.message_id }}</code></span
                >
              </h3>
              <div class="body">
                <div class="meta">
                  <div>
                    <b>From:</b> <span class="dim">{{ ph.author }}</span>
                  </div>
                  <div><b>Subject:</b> {{ ph.subject }}</div>
                  <div>
                    <b>Size:</b> <span class="dim">{{ formatBytes(ph.bytes) }}</span>
                  </div>
                </div>
                @if (articleLoading()) {
                  <div class="loading">Loading article…</div>
                } @else {
                  <pre class="body-pre">{{ articleBody() || '(empty)' }}</pre>
                }
              </div>
            </div>
          }
        }
      </div>
    </div>
  `,
  styles: [
    `
      :host {
        display: block;
      }
      .shell {
        display: grid;
        grid-template-columns: 260px 1fr;
        gap: 16px;
        align-items: stretch;
      }
      .side {
        background: var(--panel);
        border: 1px solid var(--line);
        border-radius: 8px;
        display: flex;
        flex-direction: column;
        align-self: stretch;
        height: 100%;
        min-height: 100%;
      }
      .side-head {
        padding: 10px 14px;
        border-bottom: 1px solid var(--line);
        font-size: 12px;
        color: var(--mute);
        text-transform: uppercase;
        letter-spacing: 0.5px;
      }
      .side-filter {
        padding: 8px 10px;
        border-bottom: 1px solid var(--line);
      }
      .side-filter input {
        width: 100%;
        box-sizing: border-box;
        background: var(--panel2);
        border: 1px solid var(--line);
        color: var(--text);
        padding: 6px 10px;
        border-radius: 5px;
        font: inherit;
        outline: none;
      }
      .side-filter input:focus {
        border-color: var(--accent);
      }
      .side-list {
        flex: 1 1 auto;
        min-height: 0;
        overflow-y: auto;
      }
      .g {
        padding: 8px 14px;
        border-bottom: 1px solid var(--line);
        cursor: pointer;
      }
      .g:last-child {
        border-bottom: none;
      }
      .g:hover {
        background: var(--panel2);
      }
      .g.active {
        background: var(--panel2);
        box-shadow: inset 2px 0 0 var(--accent);
      }
      .g .name {
        font-size: 12px;
        font-family: ui-monospace, Menlo, monospace;
      }
      .g .cnt {
        color: var(--mute);
        font-size: 11px;
        margin-top: 2px;
      }
      .g .new {
        color: var(--accent2);
      }
      .empty {
        padding: 16px 14px;
        color: var(--mute);
        font-size: 12px;
        text-align: center;
      }

      .main {
        min-width: 0;
        display: flex;
        flex-direction: column;
        gap: 16px;
        height: 100%;
      }
      .ctr {
        text-align: center;
        padding: 48px 16px;
      }
      .big-hint {
        color: var(--mute);
        font-size: 14px;
      }
      .new {
        color: var(--accent2);
      }

      tr.unread .subj {
        font-weight: 600;
      }
      td.dim {
        color: var(--mute);
      }
      td.subj {
        max-width: 0;
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      tr {
        cursor: pointer;
      }
      .actions {
        white-space: nowrap;
      }
      .empty-cell {
        text-align: center;
        padding: 28px !important;
        color: var(--mute);
        font-size: 13px;
      }
      .load-more {
        text-align: center;
        border-top: 1px solid var(--line);
      }
      .download-bar {
        display: flex;
        align-items: center;
        gap: 10px;
        border-top: 1px solid var(--line);
        font-size: 13px;
      }
      .spacer {
        flex: 1;
      }

      .meta {
        display: flex;
        flex-direction: column;
        gap: 4px;
        font-size: 13px;
        margin-bottom: 10px;
      }
      .meta b {
        color: var(--mute);
        font-weight: 500;
        margin-right: 6px;
      }
      .body-pre {
        margin: 0;
        background: var(--panel2);
        border: 1px solid var(--line);
        border-radius: 5px;
        padding: 10px 12px;
        font:
          12px ui-monospace,
          Menlo,
          Consolas,
          monospace;
        max-height: 300px;
        overflow: auto;
        white-space: pre-wrap;
        word-break: break-all;
      }
      .loading {
        color: var(--mute);
        font-size: 13px;
        padding: 20px;
        text-align: center;
      }
    `,
  ],
})
export class GroupsViewComponent implements OnInit {
  groups = signal<GroupRow[]>([]);
  selectedGroup = signal<GroupRow | null>(null);
  groupFilter = '';
  groupNameFilter = '';
  headers = signal<HeaderRow[]>([]);
  headerTotal = signal(0);
  searchQuery = '';
  offset = 0;
  pageSize = 100;
  selectedIds = signal<string[]>([]);
  newAvailable = signal(0);
  fetching = signal(false);
  previewHeader = signal<HeaderRow | null>(null);
  articleBody = signal<string | null>(null);
  articleLoading = signal(false);

  filteredGroups = computed(() => {
    const f = this.groupNameFilter.toLowerCase();
    return f ? this.groups().filter((g) => g.name.toLowerCase().includes(f)) : this.groups();
  });
  allSelected = computed(() => {
    const ids = this.selectedIds();
    const hdrs = this.headers();
    return hdrs.length > 0 && hdrs.every((h) => ids.includes(h.message_id));
  });
  selectedBytes = computed(() => {
    const ids = new Set(this.selectedIds());
    return this.headers()
      .filter((h) => ids.has(h.message_id))
      .reduce((s, h) => s + h.bytes, 0);
  });

  constructor(
    private svc: GroupService,
    private snack: MatSnackBar,
    private dialog: MatDialog,
  ) {}

  ngOnInit(): void {
    this.loadGroups();
  }

  loadGroups(): void {
    this.svc.list({ subscribed: true, limit: 500 }).subscribe((r) => this.groups.set(r.groups));
  }

  selectGroup(g: GroupRow): void {
    this.selectedGroup.set(g);
    this.offset = 0;
    this.searchQuery = '';
    this.selectedIds.set([]);
    this.previewHeader.set(null);
    this.articleBody.set(null);
    this.loadHeaders();
    this.loadStatus();
  }

  loadHeaders(): void {
    const g = this.selectedGroup();
    if (!g) return;
    this.svc
      .listHeaders(g.id, {
        search: this.searchQuery || undefined,
        limit: this.pageSize,
        offset: this.offset,
      })
      .subscribe((r) => {
        this.headers.set(r.headers);
        this.headerTotal.set(r.total);
      });
  }

  loadStatus(): void {
    const g = this.selectedGroup();
    if (!g) return;
    this.svc.getStatus(g.id).subscribe((s) => this.newAvailable.set(s.new_available));
  }

  searchHeaders(): void {
    this.offset = 0;
    // Top search bar may specify a group — resolve it if different from the
    // currently selected group, otherwise just re-query.
    if (this.groupFilter) {
      const g = this.groups().find((x) => x.name === this.groupFilter);
      if (g && g.id !== this.selectedGroup()?.id) this.selectedGroup.set(g);
    }
    this.loadHeaders();
  }

  loadMore(): void {
    this.offset += this.pageSize;
    const g = this.selectedGroup();
    if (!g) return;
    this.svc
      .listHeaders(g.id, {
        search: this.searchQuery || undefined,
        limit: this.pageSize,
        offset: this.offset,
      })
      .subscribe((r) => this.headers.set([...this.headers(), ...r.headers]));
  }

  fetchHeaders(): void {
    const g = this.selectedGroup();
    if (!g) return;
    this.fetching.set(true);
    this.svc.fetchHeaders(g.id).subscribe({
      next: () => this.snack.open('Fetching headers…', 'Close', { duration: 2000 }),
      error: () => this.fetching.set(false),
    });
    const poll = setInterval(() => {
      this.loadHeaders();
      this.loadStatus();
      this.loadGroups();
      if (this.newAvailable() <= 0) {
        this.fetching.set(false);
        clearInterval(poll);
      }
    }, 3000);
    setTimeout(() => {
      clearInterval(poll);
      this.fetching.set(false);
    }, 120_000);
  }

  markAllRead(): void {
    const g = this.selectedGroup();
    if (!g) return;
    this.svc.markAllRead(g.id).subscribe(() => {
      this.loadHeaders();
      this.loadGroups();
      this.snack.open('All marked read', 'Close', { duration: 2000 });
    });
  }

  toggleSelect(mid: string): void {
    const ids = this.selectedIds();
    this.selectedIds.set(ids.includes(mid) ? ids.filter((i) => i !== mid) : [...ids, mid]);
  }

  isSelected(mid: string): boolean {
    return this.selectedIds().includes(mid);
  }

  toggleSelectAll(): void {
    this.selectedIds.set(this.allSelected() ? [] : this.headers().map((h) => h.message_id));
  }

  selectArticle(h: HeaderRow): void {
    this.previewHeader.set(h);
    this.articleLoading.set(true);
    this.articleBody.set(null);
    this.svc.getArticle(h.message_id).subscribe({
      next: (r) => {
        this.articleBody.set(r.body);
        this.articleLoading.set(false);
        if (!h.read) {
          h.read = true;
          this.headers.set([...this.headers()]);
        }
      },
      error: () => {
        this.articleBody.set('(Failed to load)');
        this.articleLoading.set(false);
      },
    });
  }

  downloadSelected(): void {
    const g = this.selectedGroup();
    if (!g || !this.selectedIds().length) return;
    this.svc.downloadSelected(g.id, this.selectedIds()).subscribe({
      next: (r) => {
        this.snack.open(r.message, 'Close', { duration: 3000 });
        this.selectedIds.set([]);
      },
      error: () => this.snack.open('Download failed', 'Close', { duration: 5000 }),
    });
  }

  openBrowseDialog(): void {
    this.dialog
      .open(GroupBrowserDialogComponent, { width: '700px', maxHeight: '80vh' })
      .afterClosed()
      .subscribe(() => this.loadGroups());
  }

  formatBytes(b: number): string {
    if (b === 0) return '0 B';
    const k = 1024;
    const s = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.min(4, Math.floor(Math.log(b) / Math.log(k)));
    return (b / Math.pow(k, i)).toFixed(1) + ' ' + s[i];
  }
}
