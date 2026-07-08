import { Component, OnInit, OnDestroy, signal, computed } from '@angular/core';
import { CommonModule } from '@angular/common';
import { FormsModule } from '@angular/forms';
import { RouterModule } from '@angular/router';
import { HttpClient } from '@angular/common/http';
import { MatSnackBar, MatSnackBarModule } from '@angular/material/snack-bar';
import { Observable, Subscription, finalize } from 'rxjs';
import { ApiService } from '../../core/services/api.service';
import { AddNzbService } from '../../core/services/add-nzb.service';
import { NzbJob, QueueResponse, StatusResponse } from '../../core/models/queue.model';

interface CategoryConfig {
  name: string;
  output_dir: string | null;
  post_processing: number;
}

interface ServerConfigLite {
  id: string;
  name: string;
  host: string;
  port: number;
  connections: number;
  priority: number;
  enabled: boolean;
  ssl: boolean;
}

// One post-processing step in the inline pipeline strip. `state` drives styling.
interface PipelineStep {
  label: string;
  state: 'done' | 'active' | 'pending';
}

@Component({
  selector: 'app-queue-view',
  standalone: true,
  imports: [CommonModule, FormsModule, RouterModule, MatSnackBarModule],
  template: `
    <!-- ============ Stat cards ============ -->
    <div class="cards4">
      <div class="card">
        <div class="label">Download speed</div>
        <div class="val">{{ speedValue() }} <span class="unit">{{ speedUnit() }}</span></div>
        <div class="sub">{{ paused() ? 'Paused' : 'Active · limit off' }}</div>
      </div>
      <div class="card">
        <div class="label">NNTP connections</div>
        <div class="val">{{ connsActive() }} / {{ connsTotal() }}</div>
        <div class="bar"><div [style.width.%]="connPct()"></div></div>
        <div class="sub">{{ servers().length }} server{{ servers().length === 1 ? '' : 's' }} ({{ serversEnabled() }} enabled)</div>
      </div>
      <div class="card">
        <div class="label">Queue</div>
        <div class="val">{{ jobs().length }} jobs · {{ formatBytes(remainingBytes()) }}</div>
        <div class="sub">{{ etaTotal() }}</div>
      </div>
      <div class="card">
        <div class="label">Disk free</div>
        <div class="val">{{ diskFreeValue() }} <span class="unit">{{ diskFreeUnit() }}</span></div>
        <div class="bar green"><div [style.width.%]="diskUsedPct()"></div></div>
        <div class="sub">Downloads volume</div>
      </div>
    </div>

    <!-- ============ Per-server connection pool ============ -->
    <div class="panel pool-panel" [class.collapsed]="poolCollapsed()">
      <h3>NNTP connection pool
        <span class="hint">priority failover · TLS via rustls · live</span>
        <button class="collapse-btn" (click)="togglePool()" [title]="poolCollapsed() ? 'Expand' : 'Collapse'">
          {{ poolCollapsed() ? '▸' : '▾' }}
        </button>
      </h3>
      @if (!poolCollapsed()) {
        <div class="body">
          @if (servers().length === 0) {
            <div class="empty">No servers configured. <a routerLink="/settings">Add one →</a></div>
          }
          @for (s of serversWithConns(); track s.id) {
            <div class="srv-block">
              <div class="srv-head">
                <div>
                  <span class="srv-name" [class.dim]="!s.enabled">{{ s.name || s.host }}</span>
                  <span class="prio">
                    priority {{ s.priority }} · {{ s.connections }} slots
                    @if (!s.enabled) { · disabled }
                  </span>
                </div>
                <div class="srv-meta">
                  @if (s.enabled) {
                    {{ s.active }} active · {{ s.idle }} idle
                  } @else {
                    off
                  }
                </div>
              </div>
              <div class="conn-grid">
                @for (i of gridRange(s.connections); track i) {
                  <div class="c"
                       [class.active]="s.enabled && i < s.active"
                       [class.idle]="s.enabled && i >= s.active && i < s.active + s.idle"
                       [class.err]="!s.enabled"></div>
                }
              </div>
            </div>
          }
          <div class="legend">
            <span class="sw a">Active transfer</span>
            <span class="sw i">Idle (pooled)</span>
            <span class="sw f">Free slot</span>
            <span class="sw e">Disabled / error</span>
            <span style="margin-left:auto">Transport: NNTPS · rustls (ring)</span>
          </div>
        </div>
      }
    </div>

    <!-- ============ Post-processing pipeline (shown when a job is in PP) ============ -->
    @if (ppJob(); as pp) {
      <div class="panel">
        <h3>Post-processing · <code>{{ pp.name }}</code>
          <span class="hint">{{ pp.status }}</span>
        </h3>
        <div class="pipeline">
          @for (step of ppSteps(); track step.label) {
            <div class="step" [class.done]="step.state === 'done'" [class.active]="step.state === 'active'">
              <div class="dot">{{ stepIcon(step) }}</div>
              <div class="lbl">{{ step.label }}</div>
            </div>
          }
        </div>
      </div>
    }

    <!-- ============ Add NZB panel (collapsible) ============ -->
    @if (showAddPanel) {
      <div class="panel add-panel">
        <h3>Add NZB
          <span class="hint">upload .nzb files or paste a URL</span>
          <button class="row-action" (click)="showAddPanel = false" title="close">✕</button>
        </h3>
        <div class="body">
          <div class="add-tabs">
            <button class="btn sm" [class.primary]="addMode === 'file'" (click)="addMode = 'file'">Upload files</button>
            <button class="btn sm" [class.primary]="addMode === 'url'"  (click)="addMode = 'url'">From URL</button>
          </div>

          @if (addMode === 'file') {
            <div class="dropzone"
                 (dragover)="onDragOver($event)"
                 (dragleave)="onDragLeave($event)"
                 (drop)="onDrop($event)"
                 [class.dragover]="isDragging">
              <div class="dz-title">Drop files here or click to browse</div>
              <div class="dz-hint">.nzb, .zip, .rar, .7z, .gz — multiple files supported</div>
              <input type="file" accept=".nzb,.zip,.rar,.7z,.gz" multiple class="dz-input" (change)="onFilesSelected($event)" />
            </div>
            @if (selectedFiles.length > 0) {
              <div class="file-chips">
                @for (f of selectedFiles; track f.name) {
                  <div class="file-chip">
                    <span>{{ f.name }}</span>
                    <span class="chip-x" (click)="removeFile(f)">✕</span>
                  </div>
                }
              </div>
            }
          }

          @if (addMode === 'url') {
            <input type="text" class="url-input" placeholder="https://example.com/file.nzb"
                   [(ngModel)]="addUrl" (keydown.enter)="addFromUrl()" />
          }

          <div class="add-options">
            <div class="add-field">
              <label>Category</label>
              <select [(ngModel)]="addCategory">
                <option value="">None</option>
                @for (cat of categories(); track cat.name) { <option [value]="cat.name">{{ cat.name }}</option> }
              </select>
            </div>
            <div class="add-field">
              <label>Priority</label>
              <select [(ngModel)]="addPriority">
                <option [ngValue]="0">Low</option>
                <option [ngValue]="1">Normal</option>
                <option [ngValue]="2">High</option>
                <option [ngValue]="3">Force</option>
              </select>
            </div>
            <span class="spacer"></span>
            @if (addMode === 'file') {
              <button class="btn primary" [disabled]="selectedFiles.length === 0 || uploading" (click)="uploadFiles()">
                {{ uploading ? 'Uploading...' : (selectedFiles.length > 1 ? 'Upload ' + selectedFiles.length + ' files' : 'Upload') }}
              </button>
            } @else {
              <button class="btn primary" [disabled]="!addUrl || uploading" (click)="addFromUrl()">
                {{ uploading ? 'Adding...' : 'Add' }}
              </button>
            }
          </div>
        </div>
      </div>
    }

    <!-- ============ Filter bar + bulk actions ============ -->
    <div class="filter-bar">
      <button class="chip" [class.active]="filterStatus === 'all'"    (click)="filterStatus = 'all'">All ({{ jobs().length }})</button>
      <button class="chip" [class.active]="filterStatus === 'active'" (click)="filterStatus = 'active'">Active</button>
      <button class="chip" [class.active]="filterStatus === 'queued'" (click)="filterStatus = 'queued'">Queued</button>
      <button class="chip" [class.active]="filterStatus === 'paused'" (click)="filterStatus = 'paused'">Paused</button>
      <span class="spacer"></span>

      @if (selectedIds().size > 0) {
        <span class="bulk-count">{{ selectedIds().size }} selected</span>
        <button class="btn sm" (click)="bulkResume()">▶ Start</button>
        <button class="btn sm" (click)="bulkPause()">❚❚ Pause</button>
        <button class="btn sm danger" (click)="bulkDelete()">Delete</button>
        <button class="btn sm ghost" (click)="clearSelection()">✕</button>
      }
    </div>

    <!-- ============ Active downloads table ============ -->
    <div class="panel">
      <h3>Active downloads
        <span class="hint">{{ filteredJobs().length }} shown · {{ formatBytes(remainingBytes()) }} remaining</span>
      </h3>
      <div class="body flush">
        <table class="data">
          <thead>
            <tr>
              <th style="width:32px">
                <input type="checkbox"
                       [checked]="allFilteredSelected()"
                       (change)="toggleSelectAll($event)"
                       [disabled]="filteredJobs().length === 0" />
              </th>
              <th style="width:34%">Name</th>
              <th>Size</th>
              <th>Progress</th>
              <th>Speed</th>
              <th>ETA</th>
              <th>Status</th>
              <th>Priority</th>
              <th style="width:130px"></th>
            </tr>
          </thead>
          <tbody>
            @for (job of filteredJobs(); track job.id) {
              <tr>
                <td>
                  <input type="checkbox"
                         [checked]="selectedIds().has(job.id)"
                         (change)="toggleSelected(job.id)" />
                </td>
                <td>
                  <div class="job-name">{{ job.name }}</div>
                  @if (job.category) {
                    <div class="job-tags"><span class="tag cat">{{ job.category }}</span></div>
                  }
                </td>
                <td>{{ formatBytes(job.total_bytes) }}</td>
                <td>
                  <div class="progress" [class.pp]="isPostProc(job.status)" [class.done]="job.status === 'completed'">
                    <div [style.width.%]="percent(job)"></div>
                  </div>
                  <div class="prog-sub">
                    @if (isPostProc(job.status)) { {{ job.status }} · {{ percent(job) }}% }
                    @else if (job.status === 'queued') { queued }
                    @else { {{ percent(job) }}% · {{ formatBytes(job.downloaded_bytes) }} }
                  </div>
                </td>
                <td>{{ job.speed_bps > 0 ? formatSpeed(job.speed_bps) : '—' }}</td>
                <td>{{ job.speed_bps > 0 ? eta(job) : '—' }}</td>
                <td><span class="status-pill" [class]="statusClass(job.status)">{{ displayStatus(job.status) }}</span></td>
                <td>
                  <select class="pri-select" [class.pri-low]="job.priority === 0" [class.pri-normal]="job.priority === 1" [class.pri-high]="job.priority === 2" [class.pri-force]="job.priority === 3" [value]="job.priority" (change)="setPriority(job, +$any($event.target).value)">
                    <option value="0">Low</option>
                    <option value="1">Normal</option>
                    <option value="2">High</option>
                    <option value="3">Force</option>
                  </select>
                </td>
                <td class="actions">
                  @if (job.status === 'paused') {
                    <button class="row-action" [disabled]="isActionPending(job.id)" (click)="resumeJob(job.id)" title="resume">▶</button>
                  } @else {
                    <button class="row-action" [disabled]="isActionPending(job.id)" (click)="pauseJob(job.id)" title="pause">❚❚</button>
                  }
                  <button class="row-action danger" [disabled]="isActionPending(job.id)" (click)="deleteJob(job.id)" title="remove">✕</button>
                </td>
              </tr>
            }

            @if (filteredJobs().length === 0) {
              <tr><td colspan="9" class="empty-cell">
                @if (jobs().length === 0) {
                  No downloads in queue. Click <b>+ Upload NZB</b> in the top bar to add one.
                } @else {
                  No jobs match the current filter.
                }
              </td></tr>
            }
          </tbody>
        </table>
      </div>
    </div>
  `,
  styles: [`
    /* Compact queue page — roughly 20% smaller than app default. */
    :host {
      display: block;
      font-size: 11.2px;
    }
    :host ::ng-deep .cards4 { gap: 12px; margin-bottom: 14px; }
    :host ::ng-deep .card { padding: 10px; border-radius: 6px; }
    :host ::ng-deep .card .label { font-size: 10px; }
    :host ::ng-deep .card .val { font-size: 17px; margin-top: 4px; }
    :host ::ng-deep .card .val .unit { font-size: 11px; }
    :host ::ng-deep .card .sub { font-size: 10px; margin-top: 3px; }
    :host ::ng-deep .panel { margin-bottom: 12px; border-radius: 6px; }
    :host ::ng-deep .panel h3 { padding: 9px 13px; font-size: 12px; }
    :host ::ng-deep .panel h3 .hint { font-size: 10px; }
    :host ::ng-deep .panel .body { padding: 11px 13px; }
    :host ::ng-deep table.data { font-size: 11.5px; }
    :host ::ng-deep table.data th { font-size: 10px; padding: 6px 10px; }
    :host ::ng-deep table.data td { padding: 6px 10px; }
    :host ::ng-deep .status-pill { font-size: 10px; padding: 1px 6px; }
    :host ::ng-deep .tag { font-size: 10px; padding: 0 5px; }
    :host ::ng-deep .progress { width: 112px; height: 5px; }

    /* Connection pool — heavily compacted per design feedback. */
    .panel.pool-panel { font-size: 10.5px; }
    .panel.pool-panel h3 { padding: 6px 10px; font-size: 11px; }
    .panel.pool-panel .body { padding: 8px 10px; }
    .panel.pool-panel.collapsed h3 { border-bottom: none; }
    .collapse-btn {
      background: none; border: none; cursor: pointer; color: var(--mute);
      font-size: 13px; padding: 0 4px; margin-left: 4px; line-height: 1;
    }
    .collapse-btn:hover { color: var(--text); }
    .srv-block { padding: 6px 0; border-bottom: 1px solid var(--line); }
    .srv-block:last-of-type { border: none; padding-bottom: 0; }
    .srv-block:first-of-type { padding-top: 0; }
    .srv-head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 4px; }
    .srv-name { font-weight: 600; font-size: 11px; }
    .srv-name.dim { color: var(--mute); }
    .prio { color: var(--mute); font-weight: 400; font-size: 10px; margin-left: 6px; }
    .srv-meta { color: var(--mute); font-size: 10px; }
    .conn-grid { display: grid; grid-template-columns: repeat(40, 1fr); gap: 2px; }
    .conn-grid .c { height: 8px; border-radius: 1px; background: var(--panel2); }
    .conn-grid .c.active { background: var(--accent2); }
    .conn-grid .c.idle   { background: var(--accent); }
    .conn-grid .c.err    { background: var(--danger); opacity: .6; }
    .legend { display: flex; gap: 10px; font-size: 10px; color: var(--mute); margin-top: 6px; align-items: center; }
    .legend .sw { display: inline-flex; align-items: center; }
    .legend .sw::before { content: ""; display: inline-block; width: 10px; height: 10px; border-radius: 2px; margin-right: 5px; }
    .legend .a::before { background: var(--accent2); }
    .legend .i::before { background: var(--accent); }
    .legend .f::before { background: var(--panel2); border: 1px solid var(--line); }
    .legend .e::before { background: var(--danger); opacity: .6; }
    .empty { color: var(--mute); font-size: 13px; padding: 4px 0; }
    .empty a { margin-left: 4px; }

    /* Post-processing pipeline */
    .pipeline {
      display: flex; align-items: center; gap: 0;
      padding: 14px 16px; background: var(--panel2); border-radius: 6px;
      margin: 0 16px 16px; border: 1px solid var(--line);
    }
    .pipeline .step { flex: 1; text-align: center; position: relative; padding: 4px; }
    .pipeline .step .dot {
      width: 26px; height: 26px; border-radius: 50%;
      background: var(--panel); border: 2px solid var(--line);
      margin: 0 auto 6px; display: flex; align-items: center; justify-content: center;
      font-size: 12px; color: var(--mute); font-weight: 600;
    }
    .pipeline .step.done .dot {
      background: var(--accent2); border-color: var(--accent2); color: #fff;
    }
    .pipeline .step.active .dot {
      background: var(--purple); border-color: var(--purple); color: #fff;
      box-shadow: 0 0 0 4px rgba(167,139,250,.18);
    }
    .pipeline .step .lbl { font-size: 11px; color: var(--mute); text-transform: uppercase; letter-spacing: .4px; }
    .pipeline .step.done .lbl, .pipeline .step.active .lbl { color: var(--text); }
    .pipeline .step:not(:last-child)::after {
      content: ""; position: absolute; top: 17px; right: -50%; left: 50%;
      height: 2px; background: var(--line); z-index: 0;
    }
    .pipeline .step.done::after { background: var(--accent2); }

    /* Add NZB panel */
    .add-panel h3 .row-action { margin-left: auto; }
    .add-tabs { display: flex; gap: 8px; margin-bottom: 12px; }
    .dropzone {
      border: 2px dashed var(--line); border-radius: 6px; padding: 28px;
      text-align: center; position: relative; cursor: pointer; transition: all .2s;
    }
    .dropzone:hover, .dropzone.dragover { border-color: var(--accent); background: rgba(59,130,246,.05); }
    .dz-title { font-size: 14px; color: var(--text); margin-bottom: 4px; }
    .dz-hint { font-size: 12px; color: var(--mute); }
    .dz-input { position: absolute; inset: 0; opacity: 0; cursor: pointer; width: 100%; height: 100%; }
    .file-chips { display: flex; flex-wrap: wrap; gap: 6px; margin-top: 10px; }
    .file-chip {
      display: flex; align-items: center; gap: 6px;
      background: var(--panel2); border: 1px solid var(--line);
      border-radius: 16px; padding: 4px 10px; font-size: 12px;
    }
    .chip-x { color: var(--mute); cursor: pointer; }
    .chip-x:hover { color: var(--danger); }
    .url-input {
      width: 100%; padding: 10px 12px; border-radius: 6px;
      border: 1px solid var(--line); background: var(--panel2);
      color: var(--text); font: inherit; outline: none;
    }
    .url-input:focus { border-color: var(--accent); }
    .add-options { display: flex; align-items: flex-end; gap: 14px; margin-top: 12px; }
    .add-field { display: flex; flex-direction: column; gap: 4px; }
    .add-field label { font-size: 11px; color: var(--mute); }
    .add-field select {
      background: var(--panel2); border: 1px solid var(--line); color: var(--text);
      padding: 8px 10px; border-radius: 5px; font: inherit; outline: none;
    }
    .spacer { flex: 1; }

    /* Filter bar */
    .filter-bar {
      display: flex; align-items: center; gap: 8px;
      margin-bottom: 12px; padding: 8px 0;
    }
    .chip {
      padding: 5px 12px; border-radius: 14px;
      border: 1px solid var(--line); background: transparent;
      color: var(--mute); cursor: pointer; font: inherit; font-size: 12px;
    }
    .chip:hover { color: var(--text); border-color: #3a4656; }
    .chip.active { border-color: var(--accent); color: var(--accent); background: rgba(59,130,246,.08); }
    .bulk-count { color: var(--mute); font-size: 12px; margin-right: 4px; }

    /* Table overrides */
    .job-name { font-size: 13px; color: var(--text); }
    .job-tags { margin-top: 3px; }
    .pri-select {
      background: var(--surface, #1e2533); border: 1px solid var(--line); border-radius: 4px;
      color: var(--text); cursor: pointer; font: inherit; font-size: 11px;
      padding: 2px 4px; line-height: 18px; transition: border-color .15s;
      -webkit-appearance: auto;
    }
    .pri-select:focus { outline: none; border-color: var(--accent); }
    .pri-select.pri-low    { color: var(--mute); }
    .pri-select.pri-normal { color: var(--text); }
    .pri-select.pri-high   { color: var(--accent); border-color: var(--accent); }
    .pri-select.pri-force  { color: #a78bfa; border-color: #a78bfa; }
    .prog-sub { color: var(--mute); font-size: 11px; margin-top: 2px; }
    .actions { white-space: nowrap; }
    .row-action:disabled { opacity: .45; cursor: wait; background: transparent; }
    .empty-cell {
      text-align: center; padding: 36px 20px !important;
      color: var(--mute); font-size: 13px;
    }
  `],
})
export class QueueViewComponent implements OnInit, OnDestroy {
  jobs = signal<NzbJob[]>([]);
  remainingBytes = signal(0);
  categories = signal<CategoryConfig[]>([]);
  servers = signal<ServerConfigLite[]>([]);
  status = signal<StatusResponse | null>(null);
  selectedIds = signal<Set<string>>(new Set());
  paused = signal(false);
  actionPendingIds = signal<Set<string>>(new Set());

  readonly POOL_KEY = 'rustnzb.poolPanelCollapsed';
  poolCollapsed = signal(localStorage.getItem('rustnzb.poolPanelCollapsed') === 'true');

  togglePool(): void {
    const next = !this.poolCollapsed();
    this.poolCollapsed.set(next);
    localStorage.setItem(this.POOL_KEY, String(next));
  }

  private pollTimer: ReturnType<typeof setInterval> | null = null;

  // Filter
  filterStatus: 'all' | 'active' | 'queued' | 'paused' = 'all';

  // Add NZB panel state
  showAddPanel = false;
  addMode: 'file' | 'url' = 'file';
  selectedFiles: File[] = [];
  addUrl = '';
  addCategory = '';
  addPriority = 1;
  uploading = false;
  isDragging = false;
  private toggleSub: Subscription | null = null;

  constructor(
    private api: ApiService,
    private http: HttpClient,
    private snackBar: MatSnackBar,
    private addNzbService: AddNzbService,
  ) {}

  ngOnInit(): void {
    this.loadAll();
    this.pollTimer = setInterval(() => this.loadQueue(), 2000);
    this.toggleSub = this.addNzbService.panelToggle$.subscribe(() => {
      this.showAddPanel = !this.showAddPanel;
    });
  }

  ngOnDestroy(): void {
    if (this.pollTimer) clearInterval(this.pollTimer);
    this.toggleSub?.unsubscribe();
  }

  private loadAll(): void {
    this.loadQueue();
    this.loadCategories();
    this.loadServers();
  }

  loadQueue(): void {
    this.api.get<QueueResponse>('/queue').subscribe({
      next: (r) => {
        this.jobs.set(r.jobs);
        this.paused.set(r.paused);
        this.remainingBytes.set(r.jobs.reduce((sum, j) => sum + (j.total_bytes - j.downloaded_bytes), 0));
        // Prune selectedIds of jobs that no longer exist.
        const liveIds = new Set(r.jobs.map(j => j.id));
        const cur = this.selectedIds();
        const next = new Set<string>();
        for (const id of cur) if (liveIds.has(id)) next.add(id);
        if (next.size !== cur.size) this.selectedIds.set(next);
      },
      error: () => {},
    });
    this.api.get<StatusResponse>('/status').subscribe({
      next: s => this.status.set(s),
      error: () => {},
    });
  }

  loadCategories(): void {
    this.api.get<CategoryConfig[]>('/config/categories').subscribe({
      next: cats => this.categories.set(cats),
      error: () => {},
    });
  }

  loadServers(): void {
    this.api.get<ServerConfigLite[]>('/config/servers').subscribe({
      next: srvs => this.servers.set(srvs),
      error: () => {},
    });
  }

  // ---- Stat-card derivations ----

  speedValue = computed(() => this.formatSpeedValue(this.status()?.speed_bps ?? 0));
  speedUnit = computed(() => this.formatSpeedUnit(this.status()?.speed_bps ?? 0));
  diskFreeValue = computed(() => this.formatBytesValue(this.status()?.disk_space_free ?? 0));
  diskFreeUnit = computed(() => this.formatBytesUnit(this.status()?.disk_space_free ?? 0));
  diskUsedPct = computed(() => 28); // No total-disk endpoint; placeholder bar.

  serversEnabled = computed(() => this.servers().filter(s => s.enabled).length);
  connsTotal = computed(() => this.servers().filter(s => s.enabled).reduce((n, s) => n + s.connections, 0));
  /**
   * Active connection count across the pool. We don't have a live "in-use"
   * endpoint, so derive a reasonable estimate: every job in `downloading`
   * state burns roughly its allocated slice. Pool sizes still cap the bar.
   */
  connsActive = computed(() => {
    const active = this.jobs().filter(j => j.status === 'downloading').length;
    const total = this.connsTotal();
    if (active === 0 || total === 0) return 0;
    // Simple: assume each active job saturates ~half the primary server's conns.
    const primary = this.servers().find(s => s.enabled && s.priority === 0);
    const primaryConns = primary?.connections ?? total;
    return Math.min(total, Math.round(primaryConns * active));
  });
  connPct = computed(() => {
    const t = this.connsTotal();
    return t === 0 ? 0 : Math.round((this.connsActive() / t) * 100);
  });

  /**
   * Projected servers with `active`/`idle` fields for the visualiser.
   * The daemon doesn't expose per-server pool state yet, so we distribute
   * `connsActive()` across enabled servers in priority order.
   */
  serversWithConns = computed(() => {
    const enabled = this.servers().filter(s => s.enabled).sort((a, b) => a.priority - b.priority);
    const disabled = this.servers().filter(s => !s.enabled);
    let remainingActive = this.connsActive();
    const out = enabled.map(s => {
      const active = Math.min(s.connections, remainingActive);
      remainingActive -= active;
      const idle = Math.min(s.connections - active, this.jobs().length > 0 ? 1 : 0);
      return { ...s, active, idle };
    });
    return [...out, ...disabled.map(s => ({ ...s, active: 0, idle: 0 }))];
  });

  gridRange(n: number): number[] { return Array.from({ length: n }, (_, i) => i); }

  etaTotal(): string {
    const speed = this.status()?.speed_bps ?? 0;
    if (speed === 0 || this.remainingBytes() === 0) return '—';
    const secs = this.remainingBytes() / speed;
    return 'ETA ' + this.formatDuration(secs);
  }

  // ---- Post-processing pipeline ----

  ppJob = computed<NzbJob | null>(() => {
    return this.jobs().find(j => this.isPostProc(j.status)) ?? null;
  });

  ppSteps = computed<PipelineStep[]>(() => {
    const job = this.ppJob();
    if (!job) return [];
    const order = ['download', 'decode', 'assemble', 'verify', 'repair', 'extract', 'cleanup'];
    const labels: Record<string, string> = {
      download: 'Download', decode: 'Decode', assemble: 'Assemble',
      verify: 'Par2 verify', repair: 'Par2 repair', extract: 'Unrar', cleanup: 'Cleanup',
    };
    const statusToIdx: Record<string, number> = {
      downloading: 0, verifying: 3, repairing: 4, extracting: 5, completed: 6,
    };
    const activeIdx = statusToIdx[job.status] ?? 0;
    return order.map((k, i) => ({
      label: labels[k],
      state: i < activeIdx ? 'done' : i === activeIdx ? 'active' : 'pending',
    }));
  });

  stepIcon(step: PipelineStep): string {
    if (step.state === 'done') return '✓';
    return String(this.ppSteps().indexOf(step) + 1);
  }

  isPostProc(status: string): boolean {
    return ['verifying', 'repairing', 'extracting'].includes(status);
  }

  // ---- Filtering ----

  filteredJobs(): NzbJob[] {
    const all = this.jobs();
    if (this.filterStatus === 'all') return all;
    if (this.filterStatus === 'active') return all.filter(j => j.status === 'downloading' || this.isPostProc(j.status));
    if (this.filterStatus === 'queued') return all.filter(j => j.status === 'queued');
    if (this.filterStatus === 'paused') return all.filter(j => j.status === 'paused');
    return all;
  }

  // ---- Add NZB ----

  onDragOver(e: DragEvent): void { e.preventDefault(); this.isDragging = true; }
  onDragLeave(_e: DragEvent): void { this.isDragging = false; }
  onDrop(e: DragEvent): void {
    e.preventDefault();
    this.isDragging = false;
    if (e.dataTransfer?.files) {
      this.selectedFiles = [...this.selectedFiles, ...Array.from(e.dataTransfer.files)];
    }
  }
  onFilesSelected(event: Event): void {
    const input = event.target as HTMLInputElement;
    if (input.files) this.selectedFiles = [...this.selectedFiles, ...Array.from(input.files)];
  }
  removeFile(file: File): void { this.selectedFiles = this.selectedFiles.filter(f => f !== file); }

  uploadFiles(): void {
    if (this.selectedFiles.length === 0 || this.uploading) return;
    this.uploading = true;
    const formData = new FormData();
    for (const file of this.selectedFiles) formData.append('file', file, file.name);
    const params: string[] = [];
    if (this.addCategory) params.push(`category=${encodeURIComponent(this.addCategory)}`);
    if (this.addPriority !== 1) params.push(`priority=${this.addPriority}`);
    const qs = params.length > 0 ? '?' + params.join('&') : '';
    const token = localStorage.getItem('access_token');
    const headers: Record<string, string> = token ? { Authorization: `Bearer ${token}` } : {};
    this.http.post(`/api/queue/add${qs}`, formData, { headers }).subscribe({
      next: () => {
        const count = this.selectedFiles.length;
        this.snackBar.open(`${count} NZB${count > 1 ? 's' : ''} added to queue`, 'Close', { duration: 3000 });
        this.selectedFiles = []; this.uploading = false; this.showAddPanel = false; this.loadQueue();
      },
      error: (err) => {
        const msg = err.error?.message || (err.status === 413 ? 'Upload too large' : err.statusText) || 'Upload failed';
        this.snackBar.open('Failed: ' + msg, 'Close', { duration: 5000 });
        this.uploading = false;
      },
    });
  }

  addFromUrl(): void {
    if (!this.addUrl || this.uploading) return;
    this.uploading = true;
    const body: { url: string; category?: string; priority?: number } = { url: this.addUrl };
    if (this.addCategory) body.category = this.addCategory;
    if (this.addPriority !== 1) body.priority = this.addPriority;
    this.api.post('/queue/add-url', body).subscribe({
      next: () => {
        this.snackBar.open('NZB added from URL', 'Close', { duration: 3000 });
        this.addUrl = ''; this.uploading = false; this.showAddPanel = false; this.loadQueue();
      },
      error: (err: any) => {
        const msg = err.error?.message || err.statusText || 'Failed';
        this.snackBar.open('Failed: ' + msg, 'Close', { duration: 5000 });
        this.uploading = false;
      },
    });
  }

  // ---- Per-job actions ----

  isActionPending(id: string): boolean {
    return this.actionPendingIds().has(id);
  }

  private withPendingJobAction(id: string, actionFactory: () => Observable<unknown>, successMessage?: string): void {
    if (this.isActionPending(id)) return;

    const pending = new Set(this.actionPendingIds());
    pending.add(id);
    this.actionPendingIds.set(pending);

    actionFactory().pipe(
      finalize(() => {
        const next = new Set(this.actionPendingIds());
        next.delete(id);
        this.actionPendingIds.set(next);
      }),
    ).subscribe({
      next: () => {
        if (successMessage) {
          this.snackBar.open(successMessage, 'Close', { duration: 2500 });
        }
        this.loadQueue();
      },
      error: (err: any) => {
        const msg = err?.error?.message || err?.message || 'Action failed. Please try again.';
        this.snackBar.open(msg, 'Close', { duration: 4000 });
        this.loadQueue();
      },
    });
  }

  pauseJob(id: string): void {
    this.withPendingJobAction(id, () => this.api.post(`/queue/${id}/pause`), 'Job paused');
  }

  resumeJob(id: string): void {
    this.withPendingJobAction(id, () => this.api.post(`/queue/${id}/resume`), 'Job resumed');
  }

  setPriority(job: NzbJob, priority: number): void {
    this.api.put(`/queue/${job.id}/priority`, { priority }).subscribe({
      next: () => this.loadQueue(),
      error: () => {},
    });
  }

  deleteJob(id: string): void {
    this.withPendingJobAction(id, () => this.api.delete(`/queue/${id}`));
  }

  // ---- Bulk ----

  toggleSelected(id: string): void {
    const next = new Set(this.selectedIds());
    if (next.has(id)) next.delete(id); else next.add(id);
    this.selectedIds.set(next);
  }
  clearSelection(): void { this.selectedIds.set(new Set()); }
  allFilteredSelected(): boolean {
    const f = this.filteredJobs();
    if (f.length === 0) return false;
    const sel = this.selectedIds();
    return f.every(j => sel.has(j.id));
  }
  toggleSelectAll(ev: Event): void {
    if ((ev.target as HTMLInputElement).checked) {
      this.selectedIds.set(new Set(this.filteredJobs().map(j => j.id)));
    } else {
      this.clearSelection();
    }
  }
  bulkResume(): void {
    Array.from(this.selectedIds()).forEach(id => this.api.post(`/queue/${id}/resume`).subscribe());
    this.clearSelection();
    setTimeout(() => this.loadQueue(), 300);
  }
  bulkPause(): void {
    Array.from(this.selectedIds()).forEach(id => this.api.post(`/queue/${id}/pause`).subscribe());
    this.clearSelection();
    setTimeout(() => this.loadQueue(), 300);
  }
  bulkDelete(): void {
    const ids = Array.from(this.selectedIds());
    if (ids.length === 0) return;
    if (!confirm(`Delete ${ids.length} job(s)?`)) return;
    ids.forEach(id => this.api.delete(`/queue/${id}`).subscribe());
    this.clearSelection();
    setTimeout(() => this.loadQueue(), 300);
  }

  // ---- Formatting ----

  percent(job: { total_bytes: number; downloaded_bytes: number }): number {
    const total = this.normalizeNonNegative(job.total_bytes);
    if (total <= 0) return 0;
    const downloaded = this.normalizeNonNegative(job.downloaded_bytes);
    return Math.max(0, Math.min(100, Math.round((downloaded / total) * 100)));
  }

  eta(job: NzbJob): string {
    const speed = this.normalizeNonNegative(job.speed_bps);
    if (speed <= 0) return '—';
    const secs = this.remainingForJob(job) / speed;
    if (!Number.isFinite(secs) || secs <= 0) return '—';
    return this.formatDuration(secs);
  }

  formatDuration(secs: number): string {
    if (!Number.isFinite(secs) || secs <= 0) return '0s';
    const h = Math.floor(secs / 3600);
    const m = Math.floor((secs % 3600) / 60);
    const s = Math.floor(secs % 60);
    if (h > 0) return `${h}h ${m}m`;
    if (m > 0) return `${m}m ${s}s`;
    return `${s}s`;
  }

  private remainingForJob(job: { total_bytes: number; downloaded_bytes: number }): number {
    return Math.max(
      0,
      this.normalizeNonNegative(job.total_bytes) - this.normalizeNonNegative(job.downloaded_bytes),
    );
  }

  private normalizeNonNegative(value: number): number {
    return Number.isFinite(value) && value > 0 ? value : 0;
  }

  priorityLabel(p: number): string {
    return ['Low', 'Normal', 'High', 'Force'][p] || 'Normal';
  }

  statusClass(status: string): string {
    if (status === 'downloading') return 's-dl';
    if (status === 'queued') return 's-q';
    if (status === 'paused') return 's-paused';
    if (status === 'completed') return 's-ok';
    if (status === 'failed') return 's-fail';
    if (this.isPostProc(status)) return 's-pp';
    return 's-q';
  }

  displayStatus(status: string): string {
    if (status === 'verifying') return 'par2 verify';
    if (status === 'repairing') return 'par2 repair';
    if (status === 'extracting') return 'unrar';
    return status;
  }

  formatSpeed(bps: number): string {
    return `${this.formatSpeedValue(bps)} ${this.formatSpeedUnit(bps)}`;
  }
  private formatSpeedValue(bps: number): string {
    if (bps === 0) return '0';
    const k = 1024;
    const i = Math.min(3, Math.floor(Math.log(bps) / Math.log(k)));
    return (bps / Math.pow(k, i)).toFixed(1);
  }
  private formatSpeedUnit(bps: number): string {
    const units = ['B/s', 'KB/s', 'MB/s', 'GB/s'];
    if (bps === 0) return 'B/s';
    return units[Math.min(3, Math.floor(Math.log(bps) / Math.log(1024)))];
  }

  formatBytes(bytes: number): string {
    return `${this.formatBytesValue(bytes)} ${this.formatBytesUnit(bytes)}`;
  }
  private formatBytesValue(bytes: number): string {
    if (bytes === 0) return '0';
    const k = 1024;
    const i = Math.min(4, Math.floor(Math.log(bytes) / Math.log(k)));
    return (bytes / Math.pow(k, i)).toFixed(1);
  }
  private formatBytesUnit(bytes: number): string {
    const units = ['B', 'KB', 'MB', 'GB', 'TB'];
    if (bytes === 0) return 'B';
    return units[Math.min(4, Math.floor(Math.log(bytes) / Math.log(1024)))];
  }
}
