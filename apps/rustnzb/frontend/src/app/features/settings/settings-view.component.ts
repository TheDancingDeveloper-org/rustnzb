import { Component, HostListener, OnInit, signal, computed } from '@angular/core';
import { CommonModule } from '@angular/common';
import { FormsModule } from '@angular/forms';
import { RouterModule } from '@angular/router';
import { MatSnackBarModule } from '@angular/material/snack-bar';
import { ApiService } from '../../core/services/api.service';
import { formatBytes, formatSpeed } from '../../core/format';
import { StatusResponse } from '../../core/models/queue.model';
import { ConfirmService } from '../../shared/confirm.service';
import { IconComponent } from '../../shared/icon.component';
import { AppTheme, ThemeService } from '../../core/services/theme.service';
import { I18nService } from '../../core/i18n/i18n.service';
import { TranslatePipe } from '../../core/i18n/translate.pipe';
import { BrowserNotificationsService } from '../../core/services/browser-notifications.service';
import { httpErrorMessage, showHttpError } from '../../core/http/http-error';
import { ToastService } from '../../core/services/toast.service';
import { HasUnsavedChanges } from '../../core/guards/unsaved-changes.guard';

interface ServerConfig {
  id: string;
  name: string;
  host: string;
  port: number;
  ssl: boolean;
  ssl_verify: boolean;
  username: string | null;
  password: string | null;
  connections: number;
  priority: number;
  enabled: boolean;
  retention: number;
  pipelining: number;
  optional: boolean;
  compress: boolean;
  ramp_up_delay_ms: number;
  proxy_url: string | null;
  connect_timeout_secs: number;
}

interface CategoryConfig {
  name: string;
  output_dir: string | null;
  post_processing: number;
  cleanup_patterns?: string[];
  unwanted_extensions?: string[];
}

interface ServerStats {
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

type Tab =
  | 'servers'
  | 'rss-cfg'
  | 'categories'
  | 'postproc'
  | 'paths'
  | 'schedule'
  | 'dav'
  | 'general'
  | 'api'
  | 'telemetry'
  | 'backup'
  | 'about';

type ScheduleAction = 'pause' | 'speed_limit' | 'disable_servers';

/** `MM:SS`-free 24-hour clock, with `24:00` allowed as the end of the day. */
const CLOCK_RE = /^([01]\d|2[0-3]):[0-5]\d$|^24:00$/;

const SHORT_DAYS: Record<string, string> = {
  mon: 'mon',
  monday: 'mon',
  tue: 'tue',
  tues: 'tue',
  tuesday: 'tue',
  wed: 'wed',
  weds: 'wed',
  wednesday: 'wed',
  thu: 'thu',
  thur: 'thu',
  thurs: 'thu',
  thursday: 'thu',
  fri: 'fri',
  friday: 'fri',
  sat: 'sat',
  saturday: 'sat',
  sun: 'sun',
  sunday: 'sun',
};

function shortDay(day: string): string {
  return SHORT_DAYS[day.trim().toLowerCase()] ?? '';
}

/** One time-of-day window, as `GET/PUT /api/config/schedule` exchanges it. */
interface ScheduleRule {
  id: string;
  name: string;
  days: string[];
  start: string;
  end: string;
  action: ScheduleAction;
  speed_limit_bps: number;
  servers: string[];
}

/** The saved schedule plus what it is doing right now. */
interface ScheduleState {
  enabled: boolean;
  rules: ScheduleRule[];
  active_rule_ids: string[];
  paused: boolean;
  speed_limit_bps: number | null;
  disabled_servers: string[];
}

interface DavConfig {
  enabled: boolean;
  auto_send_all: boolean;
  category_rules: string[];
  username: string | null;
  password: string | null;
  api_key: string | null;
}

interface GeneralDirs {
  data_dir: string;
  incomplete_dir: string;
  complete_dir: string;
  watch_dir: string | null;
  listen_addr?: string;
  port?: number;
  direct_unpack?: boolean;
  max_nested_archive_depth?: number;
  max_post_processing_jobs?: number;
  max_repair_workers?: number;
  max_extract_workers?: number;
  scripts_dir?: string | null;
  script_success?: string | null;
  script_failure?: string | null;
  script_timeout_secs?: number;
  script_max_output_bytes?: number;
}

interface Par2Settings {
  defer_volumes: boolean;
  trust_inflight_verify: boolean;
}

interface GeneralUpdateResponse {
  status: boolean;
  restart_required: string[];
}

export interface PathsForm {
  complete_dir: string;
  incomplete_dir: string;
  watch_dir: string;
}

export interface PostprocForm {
  direct_unpack: boolean;
  max_nested_archive_depth: number;
  max_post_processing_jobs: number;
  max_repair_workers: number;
  max_extract_workers: number;
  scripts_dir: string;
  script_success: string;
  script_failure: string;
  script_timeout_secs: number;
  script_max_output_bytes: number;
  par2_defer_volumes: boolean;
  par2_trust_inflight_verify: boolean;
}

export interface NetworkForm {
  listen_addr: string;
  port: number;
}

/** Split a comma- or newline-separated list, dropping blanks. */
export function splitList(text: string): string[] {
  return text
    .split(/[,\n]/)
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
}

interface SabApiKeyResponse {
  api_key: string | null;
}

function emptyServer(): ServerConfig {
  return {
    id: '',
    name: '',
    host: '',
    port: 563,
    ssl: true,
    ssl_verify: true,
    username: null,
    password: null,
    connections: 8,
    priority: 0,
    enabled: true,
    retention: 0,
    pipelining: 4,
    optional: false,
    compress: false,
    ramp_up_delay_ms: 50,
    proxy_url: null,
    connect_timeout_secs: 30,
  };
}

/** GET/PUT /api/config/debug-logging: the effective log level and whether an
 * environment or command-line override beats the saved choice. */
interface DebugLoggingState {
  enabled: boolean;
  effective: string;
  overridden: boolean;
}

/** What `GET /api/config/backup` and the preview/restore routes report. */
interface BackupSummary {
  server_count: number;
  category_count: number;
  rss_feed_count: number;
  secrets_kept: number;
  warnings: string[];
}

interface BackupReport {
  status: boolean;
  restored: boolean;
  backup: {
    backup_version: number;
    created_at: string;
    app_version: string;
    secrets_included: boolean;
  };
  summary: BackupSummary;
  restart_required: string[];
}

/** Each setting that Save writes back. Save stays blocked until its own GET succeeds. */
type SettingsKey =
  | 'servers'
  | 'categories'
  | 'speedLimit'
  | 'maxActive'
  | 'historyRetention'
  | 'diskGuards'
  | 'schedule'
  | 'debugLogging'
  | 'dav'
  | 'sabApiKey'
  | 'paths'
  | 'postproc'
  | 'network';

/** Settings edited in place and written by their own Save button. Toggles that save on change are not here. */
type FormKey =
  | 'speedLimit'
  | 'maxActive'
  | 'historyRetention'
  | 'diskGuards'
  | 'schedule'
  | 'dav'
  | 'paths'
  | 'postproc'
  | 'network';

const FORM_KEYS: readonly FormKey[] = [
  'speedLimit',
  'maxActive',
  'historyRetention',
  'diskGuards',
  'schedule',
  'dav',
  'paths',
  'postproc',
  'network',
];

/** Forms filled from the one `GET /config` document and saved through `PUT /config/general`. */
const CONFIG_FORM_KEYS: readonly FormKey[] = ['paths', 'postproc', 'network'];

function isFormKey(key: SettingsKey): key is FormKey {
  return (FORM_KEYS as readonly string[]).includes(key);
}

/** Mirrors the API's cap: a backup is a few kilobytes, not a disk image. */
export const MAX_BACKUP_BYTES = 4 * 1024 * 1024;

/** The server's `attachment; filename="..."` name, or null when it sent none. */
export function filenameFromDisposition(disposition: string | null): string | null {
  if (!disposition) return null;
  const quoted = /filename="([^"]+)"/i.exec(disposition);
  if (quoted) return quoted[1];
  const bare = /filename=([^;]+)/i.exec(disposition);
  return bare ? bare[1].trim() : null;
}

/** Save `text` as a download. Does nothing where the browser cannot (jsdom). */
export function saveText(filename: string, text: string): void {
  if (
    typeof document === 'undefined' ||
    typeof URL === 'undefined' ||
    typeof URL.createObjectURL !== 'function'
  ) {
    return;
  }
  const url = URL.createObjectURL(new Blob([text], { type: 'application/toml' }));
  const link = document.createElement('a');
  link.href = url;
  link.download = filename;
  document.body.appendChild(link);
  link.click();
  link.remove();
  URL.revokeObjectURL(url);
}

type LoadState = { status: 'loading' } | { status: 'loaded' } | { status: 'failed'; message: string };

function allLoading(): Record<SettingsKey, LoadState> {
  return {
    servers: { status: 'loading' },
    categories: { status: 'loading' },
    speedLimit: { status: 'loading' },
    maxActive: { status: 'loading' },
    historyRetention: { status: 'loading' },
    diskGuards: { status: 'loading' },
    schedule: { status: 'loading' },
    debugLogging: { status: 'loading' },
    dav: { status: 'loading' },
    sabApiKey: { status: 'loading' },
    paths: { status: 'loading' },
    postproc: { status: 'loading' },
    network: { status: 'loading' },
  };
}

function emptyCategory(): CategoryConfig {
  return { name: '', output_dir: null, post_processing: 3 };
}

@Component({
  selector: 'app-settings-view',
  standalone: true,
  imports: [CommonModule, FormsModule, RouterModule, MatSnackBarModule, IconComponent, TranslatePipe],
  template: `
    <div class="settings-shell">
      <!-- Sidebar -->
      <aside class="settings-side">
        <div class="sg">Connection</div>
        <button [class.active]="tab === 'servers'" (click)="tab = 'servers'">News servers</button>
        <button [class.active]="tab === 'rss-cfg'" (click)="tab = 'rss-cfg'">RSS feeds</button>

        <div class="sg">Downloads</div>
        <button [class.active]="tab === 'categories'" (click)="tab = 'categories'">
          Categories
        </button>
        <button [class.active]="tab === 'postproc'" (click)="tab = 'postproc'">
          Post-processing
        </button>
        <button [class.active]="tab === 'paths'" (click)="tab = 'paths'">Paths &amp; disk</button>
        <button [class.active]="tab === 'schedule'" (click)="tab = 'schedule'">Scheduler</button>
        @if (webdavAvailable()) {
          <button [class.active]="tab === 'dav'" (click)="tab = 'dav'">Media Library (DAV)</button>
        }

        <div class="sg">System</div>
        <button [class.active]="tab === 'general'" (click)="tab = 'general'">General</button>
        <button [class.active]="tab === 'api'" (click)="tab = 'api'">
          API &amp; SABnzbd compat
        </button>
        <button [class.active]="tab === 'telemetry'" (click)="tab = 'telemetry'">
          Logging &amp; telemetry
        </button>
        <button [class.active]="tab === 'backup'" (click)="tab = 'backup'">
          Backup &amp; restore
        </button>
        <button [class.active]="tab === 'about'" (click)="tab = 'about'">About</button>
      </aside>

      <div class="settings-main">
        @if (restartPending().length) {
          <div class="restart-banner" role="status">
            <b>Restart pending.</b> Saved changes to
            @for (key of restartPending(); track key; let last = $last) {
              <code>{{ key }}</code>{{ last ? '' : ', ' }}
            }
            take effect after rustnzb restarts.
          </div>
        }
        <!-- =========== SERVERS =========== -->
        @if (tab === 'servers') {
          @if (loadFailure('servers'); as message) {
            <div class="load-error" role="alert">
              {{ message }}. Saving is paused until the list loads.
              <button class="btn sm" (click)="loadServers()">Retry</button>
            </div>
          }
          <div class="section-head">
            <div>
              <h2>News servers</h2>
              <div class="sub">Priority 0 is tried first; higher priorities fill gaps.</div>
            </div>
            @if (!editingServer) {
              <button class="btn primary" (click)="addServer()" [disabled]="!canSave('servers')">+ Add server</button>
            }
          </div>

          <!-- Server list -->
          <div class="panel">
            @for (s of servers(); track s.id) {
              <div class="srv-row">
                <div class="drag"><app-icon name="drag-handle" [size]="14" /></div>
                <div>
                  <div class="title" [class.dim]="!s.enabled">
                    {{ s.name || s.host }}
                    <span
                      class="pill"
                      [class.ok]="s.enabled"
                      [class.warn]="!s.enabled"
                      style="margin-left:6px"
                    >
                      ● {{ s.enabled ? 'enabled' : 'disabled' }}
                    </span>
                    @if (s.optional) {
                      <span class="tag" style="margin-left:4px">backup</span>
                    }
                  </div>
                  <div class="host">
                    {{ s.ssl ? 'NNTPS' : 'NNTP' }} · {{ s.host }}:{{ s.port }}
                    @if (s.username) {
                      · user <code>{{ s.username }}</code>
                    }
                    · {{ s.connections }} conns · pipeline {{ s.pipelining }}
                    @if (s.ssl) {
                      · TLS 1.3
                    }
                  </div>
                  <div class="meters">
                    <span
                      >priority <b>{{ s.priority }}</b></span
                    >
                    <span
                      >retention <b>{{ s.retention }} d</b></span
                    >
                    <span
                      >ramp-up <b>{{ s.ramp_up_delay_ms }} ms</b></span
                    >
                    <span
                      >timeout <b>{{ s.connect_timeout_secs }} s</b></span
                    >
                    @if (s.compress) {
                      <span><b>compression</b></span>
                    }
                    @let st = serverStats()[s.id];
                    @if (st && st.total_bytes > 0) {
                      <span style="color:var(--accent)"
                        >↓ {{ fmtBytes(st.total_bytes) }} total</span
                      >
                    }
                  </div>
                </div>
                <div class="actions">
                  <button class="btn sm" (click)="testServer(s.id)">Test</button>
                  <button class="btn sm" (click)="toggleServerEnabled(s)">
                    {{ s.enabled ? 'Disable' : 'Enable' }}
                  </button>
                  <button class="btn sm" (click)="editServer(s)">Edit</button>
                  <button class="btn sm" (click)="cloneServer(s)">Clone</button>
                  <button class="btn sm danger" (click)="deleteServer(s.id)">Remove</button>
                  <button
                    class="btn sm"
                    [class.active]="expandedStatsId === s.id"
                    (click)="toggleStats(s.id)"
                  >
                    Stats
                  </button>
                </div>
              </div>
              @if (expandedStatsId === s.id) {
                @let st = serverStats()[s.id];
                <div class="srv-stats-panel">
                  @if (st) {
                    <div class="srv-stats-grid">
                      <div class="srv-stats-col">
                        <div class="srv-stats-heading">Bandwidth</div>
                        <div class="srv-stats-row">
                          <span>Total</span><b>{{ fmtBytes(st.total_bytes) }}</b>
                        </div>
                        <div class="srv-stats-row">
                          <span>24 hours</span><b>{{ fmtBytes(st.today_bytes) }}</b>
                        </div>
                        <div class="srv-stats-row">
                          <span>7 days</span><b>{{ fmtBytes(st.week_bytes) }}</b>
                        </div>
                        <div class="srv-stats-row">
                          <span>30 days</span><b>{{ fmtBytes(st.month_bytes) }}</b>
                        </div>
                      </div>
                      <div class="srv-stats-col">
                        <div class="srv-stats-heading">Article availability</div>
                        <div class="srv-stats-row">
                          <span>Total</span><b>{{ fmtAvail(st.total_ok, st.total_fail) }}</b>
                        </div>
                        <div class="srv-stats-row">
                          <span>24 hours</span><b>{{ fmtAvail(st.today_ok, st.today_fail) }}</b>
                        </div>
                        <div class="srv-stats-row">
                          <span>7 days</span><b>{{ fmtAvail(st.week_ok, st.week_fail) }}</b>
                        </div>
                        <div class="srv-stats-row">
                          <span>30 days</span><b>{{ fmtAvail(st.month_ok, st.month_fail) }}</b>
                        </div>
                      </div>
                    </div>
                    @if (st.last_active) {
                      <div class="help" style="margin-top:6px">
                        Last activity: {{ st.last_active }}
                      </div>
                    }
                  } @else {
                    <div class="help">
                      No data yet — stats accumulate as downloads complete.
                    </div>
                  }
                </div>
              }
            }
            @if (servers().length === 0 && !editingServer && canSave('servers')) {
              <div class="empty">
                No servers configured. Click <b>+ Add server</b> to get started.
              </div>
            }
          </div>

          <!-- Edit form -->
          @if (editingServer) {
            <div class="panel">
              <h3>{{ editingServerId ? 'Edit server' : 'Add server' }}</h3>
              <div class="body">
                <div class="form">
                  <label>Name</label>
                  <input type="text" [(ngModel)]="editingServer.name" placeholder="news-primary" />

                  <label>Host</label>
                  <input
                    type="text"
                    [(ngModel)]="editingServer.host"
                    placeholder="news.example.com"
                  />

                  <label>Port</label>
                  <div class="inline">
                    <input type="number" [(ngModel)]="editingServer.port" />
                    <label class="check"
                      ><input type="checkbox" [(ngModel)]="editingServer.ssl" /> SSL (NNTPS)</label
                    >
                    <label class="check"
                      ><input type="checkbox" [(ngModel)]="editingServer.ssl_verify" /> Verify
                      cert</label
                    >
                  </div>

                  <label>Username</label>
                  <input
                    type="text"
                    [(ngModel)]="editingServer.username"
                    placeholder="(optional)"
                  />

                  <label>Password</label>
                  <input
                    type="password"
                    [(ngModel)]="editingServer.password"
                    placeholder="(optional)"
                  />

                  <label>Connections</label>
                  <div class="inline">
                    <input type="number" [(ngModel)]="editingServer.connections" min="1" />
                    <label class="check"
                      ><input type="checkbox" [(ngModel)]="editingServer.enabled" /> Enabled</label
                    >
                    <label class="check"
                      ><input type="checkbox" [(ngModel)]="editingServer.optional" /> Optional (skip
                      on failure)</label
                    >
                  </div>

                  <label>Priority</label>
                  <div class="inline">
                    <input type="number" [(ngModel)]="editingServer.priority" min="0" />
                    <span class="help"
                      >0 = primary, higher = fallback</span
                    >
                  </div>

                  <label>Pipelining</label>
                  <div class="inline">
                    <input type="number" [(ngModel)]="editingServer.pipelining" min="0" />
                    <span class="help"
                      >Max inflight ARTICLE commands per conn</span
                    >
                  </div>

                  <label>Ramp-up delay</label>
                  <div class="inline">
                    <input type="number" [(ngModel)]="editingServer.ramp_up_delay_ms" min="0" />
                    <span class="help">ms between opening conns</span>
                  </div>

                  <label>Connection timeout</label>
                  <div class="inline">
                    <input
                      type="number"
                      [(ngModel)]="editingServer.connect_timeout_secs"
                      min="1"
                    />
                    <span class="help"
                      >seconds to wait before treating this server as unreachable</span
                    >
                  </div>

                  <label>Retention</label>
                  <div class="inline">
                    <input type="number" [(ngModel)]="editingServer.retention" min="0" />
                    <span class="help">days (informational)</span>
                  </div>

                  <label>Compression</label>
                  <div class="check">
                    <input type="checkbox" [(ngModel)]="editingServer.compress" /> Enable header
                    compression (XZVER)
                  </div>

                  <label>Proxy URL</label>
                  <input
                    type="text"
                    [(ngModel)]="editingServer.proxy_url"
                    placeholder="socks5://user:pass@host:1080 (optional)"
                  />
                </div>

                <div class="form-actions">
                  <button class="btn primary" (click)="saveServer()" [disabled]="!canSave('servers')">Save</button>
                  <button class="btn" (click)="testEditingServer()">Test connection</button>
                  <button class="btn" (click)="cancelServerEdit()">Cancel</button>
                </div>
              </div>
            </div>
          }
        }

        <!-- =========== RSS GLOBAL OPTIONS =========== -->
        @if (tab === 'rss-cfg') {
          <div class="section-head">
            <div>
              <h2>RSS feeds</h2>
              <div class="sub">
                Manage feeds themselves on the <a routerLink="/rss">RSS page</a>.
              </div>
            </div>
          </div>

          <div class="panel">
            <h3>About RSS configuration</h3>
            <div class="body">
              <p class="prose-secondary" style="margin:0">
                Feed URLs, regex filters, poll intervals, and auto-enqueue are configured per feed
                on the
                <a routerLink="/rss">RSS page</a>. This section is reserved for global defaults
                (backoff, duplicate guard, User-Agent) and will move here once the backend exposes
                the corresponding endpoints.
              </p>
            </div>
          </div>
        }

        <!-- =========== CATEGORIES =========== -->
        @if (tab === 'categories') {
          @if (loadFailure('categories'); as message) {
            <div class="load-error" role="alert">
              {{ message }}. Saving is paused until the list loads.
              <button class="btn sm" (click)="loadCategories()">Retry</button>
            </div>
          }
          <div class="section-head">
            <div>
              <h2>Categories</h2>
              <div class="sub">
                Bucket downloads into folders; each has its own post-processing level.
              </div>
            </div>
            @if (!editingCategory) {
              <button class="btn primary" (click)="addCategory()" [disabled]="!canSave('categories')">+ Add category</button>
            }
          </div>

          <div class="panel">
            <div class="body flush">
              <table class="data">
                <thead>
                  <tr>
                    <th>Name</th>
                    <th>Output dir</th>
                    <th>Post-processing</th>
                    <th style="width:120px"></th>
                  </tr>
                </thead>
                <tbody>
                  @for (c of categories(); track c.name) {
                    <tr>
                      <td>
                        <span class="tag cat">{{ c.name }}</span>
                      </td>
                      <td>
                        <code>{{ c.output_dir || '(default)' }}</code>
                      </td>
                      <td>{{ ppLabel(c.post_processing) }}</td>
                      <td>
                        <button class="row-action" (click)="editCategory(c)">edit</button>
                        <button class="row-action danger" (click)="deleteCategory(c.name)">
                          del
                        </button>
                      </td>
                    </tr>
                  }
                  @if (categories().length === 0 && !editingCategory && canSave('categories')) {
                    <tr>
                      <td colspan="4" class="empty-cell">No categories configured.</td>
                    </tr>
                  }
                </tbody>
              </table>
            </div>
          </div>

          @if (editingCategory) {
            <div class="panel">
              <h3>{{ editingCategoryOriginalName ? 'Edit' : 'Add' }} category</h3>
              <div class="body">
                <div class="form">
                  <label>Name</label>
                  <input type="text" [(ngModel)]="editingCategory.name" placeholder="movies" />

                  <label>Output dir</label>
                  <div>
                    <input
                      type="text"
                      [(ngModel)]="editingCategory.output_dir"
                      placeholder="(optional — uses default if blank)"
                    />
                    <div class="setting-description">
                      Absolute, or relative to the complete directory. It must already exist in the
                      container and be writable; rustnzb never creates it. New jobs only.
                    </div>
                    @if (categoryOutputChanged()) {
                      <div class="path-warn" role="note">
                        Sonarr, Radarr and the other *arrs import from this folder. Update their
                        download-client path or remote path mapping to match, or imports will fail.
                      </div>
                    }
                  </div>

                  <label>Post-processing</label>
                  <select [(ngModel)]="editingCategory.post_processing">
                    <option [ngValue]="0">None</option>
                    <option [ngValue]="1">Repair (par2)</option>
                    <option [ngValue]="2">Unpack</option>
                    <option [ngValue]="3">Repair + Unpack</option>
                  </select>

                  <label>Delete after unpack</label>
                  <div>
                    <input
                      type="text"
                      [(ngModel)]="categoryCleanupText"
                      placeholder="*.nfo, Sample/*"
                    />
                    <div class="setting-description">File or relative-path glob patterns, comma separated.</div>
                  </div>

                  <label>Unwanted extensions</label>
                  <div>
                    <input type="text" [(ngModel)]="categoryExtText" placeholder="exe, lnk" />
                    <div class="setting-description">Removed after unpacking, comma separated.</div>
                  </div>
                </div>
                <div class="form-actions">
                  <button class="btn primary" (click)="saveCategory()" [disabled]="!canSave('categories')">Save</button>
                  <button class="btn" (click)="cancelCategoryEdit()">Cancel</button>
                </div>
              </div>
            </div>
          }
        }

        <!-- =========== POST-PROCESSING =========== -->
        @if (tab === 'postproc') {
          @if (loadFailure('postproc'); as message) {
            <div class="load-error" role="alert">
              {{ message }}. Saving is paused until the settings load.
              <button class="btn sm" (click)="loadDirs()">Retry</button>
            </div>
          }
          <div class="section-head">
            <div>
              <h2>Post-processing</h2>
              <div class="sub">Par2 repair (native Rust), unrar / 7z extraction, cleanup and scripts.</div>
            </div>
          </div>

          <div class="panel">
            <h3>Repair &amp; unpack</h3>
            <div class="body">
              <div class="form">
                <label>Direct unpack</label>
                <label class="toggle">
                  <input type="checkbox" [(ngModel)]="postprocForm.direct_unpack" />
                  <span>Extract RAR volumes while the job downloads <span class="restart-tag">restart required</span></span>
                </label>

                <label>Nested archive depth</label>
                <div class="inline">
                  <input type="number" [(ngModel)]="postprocForm.max_nested_archive_depth" min="0" max="10" />
                  <span class="help">layers inside the outer archive · 0 = outer only</span>
                  <span class="restart-tag">restart required</span>
                </div>

                <label>Jobs in post-processing</label>
                <div class="inline">
                  <input type="number" [(ngModel)]="postprocForm.max_post_processing_jobs" min="1" max="64" />
                  <span class="restart-tag">restart required</span>
                </div>

                <label>Repair workers</label>
                <div class="inline">
                  <input type="number" [(ngModel)]="postprocForm.max_repair_workers" min="1" max="64" />
                  <span class="restart-tag">restart required</span>
                </div>

                <label>Extract workers</label>
                <div class="inline">
                  <input type="number" [(ngModel)]="postprocForm.max_extract_workers" min="1" max="64" />
                  <span class="restart-tag">restart required</span>
                </div>

                <label>Defer par2 volumes</label>
                <label class="toggle">
                  <input type="checkbox" [(ngModel)]="postprocForm.par2_defer_volumes" />
                  <span>Fetch recovery volumes only when damage needs them</span>
                </label>

                <label>Trust in-flight verify</label>
                <label class="toggle">
                  <input type="checkbox" [(ngModel)]="postprocForm.par2_trust_inflight_verify" />
                  <span>Skip the disk verify pass when the download already proved every file</span>
                </label>
              </div>
            </div>
          </div>

          <div class="panel">
            <h3>Scripts</h3>
            <div class="body">
              <div class="form">
                <label>Scripts directory</label>
                <input type="text" [(ngModel)]="postprocForm.scripts_dir" placeholder="(none)" />

                <label>On success</label>
                <input type="text" [(ngModel)]="postprocForm.script_success" placeholder="(none)" />

                <label>On failure</label>
                <input type="text" [(ngModel)]="postprocForm.script_failure" placeholder="(none)" />

                <label>Timeout</label>
                <div class="inline">
                  <input type="number" [(ngModel)]="postprocForm.script_timeout_secs" min="1" />
                  <span class="help">seconds</span>
                </div>

                <label>Output kept</label>
                <div class="inline">
                  <input type="number" [(ngModel)]="postprocForm.script_max_output_bytes" min="0" />
                  <span class="help">bytes</span>
                </div>
              </div>
            </div>
          </div>

          <div class="panel">
            <h3>Cleanup</h3>
            <div class="body prose-secondary">
              On success, .rar / .par2 are removed from the output directory. File-deletion rules
              (glob patterns and unwanted extensions) are set per category under
              <a (click)="tab = 'categories'" style="cursor:pointer">Categories</a>. On failure,
              partial files are kept for retry.
            </div>
          </div>

          <div class="form-actions">
            <button class="btn primary" (click)="savePostproc()" [disabled]="!canSave('postproc')">
              Save post-processing
            </button>
          </div>
        }

        <!-- =========== PATHS & DISK =========== -->
        @if (tab === 'paths') {
          @if (loadFailure('diskGuards'); as message) {
            <div class="load-error" role="alert">
              {{ message }}. Saving is paused until the setting loads.
              <button class="btn sm" (click)="loadDiskGuards()">Retry</button>
            </div>
          }
          @if (loadFailure('paths'); as message) {
            <div class="load-error" role="alert">
              {{ message }}. Saving is paused until the paths load.
              <button class="btn sm" (click)="loadDirs()">Retry</button>
            </div>
          }
          <div class="section-head">
            <div>
              <h2>Paths &amp; disk</h2>
              <div class="sub">
                Where rustnzb reads and writes. A changed path applies to new jobs only: queued and
                history entries keep their paths, and nothing is moved.
              </div>
            </div>
          </div>

          <div class="panel">
            <h3>Directories</h3>
            <div class="body">
              <div class="form">
                <label for="complete-dir">Complete (downloads)</label>
                <div>
                  <input id="complete-dir" type="text" [(ngModel)]="pathsForm.complete_dir" />
                  <div class="setting-description">Finished jobs land here, in a folder per category.</div>
                </div>

                <label for="incomplete-dir">Incomplete / temp</label>
                <div>
                  <input id="incomplete-dir" type="text" [(ngModel)]="pathsForm.incomplete_dir" />
                  <div class="setting-description">Working area for jobs while they download.</div>
                </div>

                <label for="watch-dir">Watch dir</label>
                <div>
                  <input id="watch-dir" type="text" [(ngModel)]="pathsForm.watch_dir" placeholder="(not configured)" />
                  <div class="setting-description">
                    <code>.nzb</code> files dropped here are enqueued. <span class="restart-tag">restart required</span>
                  </div>
                </div>

                <label>Data dir</label>
                <div>
                  <code>{{ dirs()?.data_dir ?? '…' }}</code>
                  <div class="setting-description">
                    Read-only here: holds the database and this config. Set it with
                    <code>RUSTNZB_DATA_DIR</code> or <code>--data-dir</code>.
                  </div>
                </div>
              </div>
              <div class="dim" style="margin-top:10px">
                Each directory must already exist inside the container and be writable by rustnzb;
                it is checked on save and never created. Category folders are set under
                <a (click)="tab = 'categories'" style="cursor:pointer">Categories</a>.
              </div>
              <div class="form-actions">
                <button class="btn primary" (click)="savePaths()" [disabled]="!canSave('paths')">Save paths</button>
              </div>
            </div>
          </div>

          <div class="panel">
            <h3>
              Disk guards
              <span class="hint"
                >history retention is editable under
                <a (click)="tab = 'general'" style="cursor:pointer">General</a></span
              >
            </h3>
            <div class="body">
              <div class="form">
                <label>Min free space</label>
                <div class="inline">
                  <input type="number" [(ngModel)]="minFreeSpaceGB" min="0" step="0.1" />
                  <span style="color:var(--mute)">GB · 0 = disabled · restart to apply</span>
                  <button class="btn sm" (click)="saveDiskGuards()" [disabled]="!canSave('diskGuards')">Save</button>
                </div>
                <label>Abort hopeless</label>
                <div class="inline">
                  <label class="toggle">
                    <input
                      type="checkbox"
                      [(ngModel)]="abortHopeless"
                      (change)="saveDiskGuards()"
                    />
                    <span>Abort downloads that cannot possibly complete</span>
                  </label>
                </div>
              </div>
            </div>
          </div>
        }

        <!-- =========== SCHEDULER =========== -->
        @if (tab === 'schedule') {
          @if (loadFailure('schedule'); as message) {
            <div class="load-error" role="alert">
              {{ message }}. Saving is paused until the schedule loads.
              <button class="btn sm" (click)="loadSchedule()">Retry</button>
            </div>
          }
          <div class="section-head">
            <div>
              <h2>Scheduler</h2>
              <div class="sub">
                Pause downloads, cap the speed, or take servers out of rotation on a
                time-of-day window. Times use the server's local clock.
              </div>
            </div>
            <button class="btn primary" (click)="addScheduleRule()" [disabled]="!canSave('schedule')">
              + Add rule
            </button>
          </div>

          <div class="panel">
            <h3>
              Status
              @if (scheduleActiveNow()) {
                <span class="pill ok">● active now</span>
              }
            </h3>
            <div class="body">
              <div class="form">
                <label>Enable schedule</label>
                <div class="inline">
                  <label class="toggle">
                    <input type="checkbox" [(ngModel)]="schedule.enabled" />
                    <span>Apply these rules; turning this off lifts only the schedule's own pause</span>
                  </label>
                </div>
                <label>Right now</label>
                <div class="schedule-now">
                  @if (!schedule.enabled) {
                    <span class="dim">The schedule is off.</span>
                  } @else if (!scheduleActiveNow()) {
                    <span class="dim">No rule is active.</span>
                  } @else {
                    @for (rule of activeScheduleRules(); track rule.id) {
                      <span class="pill">{{ rule.name || 'Unnamed rule' }} · {{ actionLabel(rule.action) }}</span>
                    }
                    @if (schedule.paused) {
                      <span class="pill warn">paused by schedule</span>
                    }
                    @if (schedule.speed_limit_bps !== null) {
                      <span class="pill">cap {{ formatSpeedValue(schedule.speed_limit_bps) }}</span>
                    }
                    @if (schedule.disabled_servers.length) {
                      <span class="pill warn">{{ disabledServerNames() }} out of rotation</span>
                    }
                  }
                </div>
                <div class="dim">
                  The schedule only lifts a pause it applied. A manual pause, a disk-space hold, or
                  a paused job stays paused when a window ends.
                </div>
              </div>
            </div>
          </div>

          @if (!schedule.rules.length) {
            <div class="panel">
              <div class="body empty">No rules yet. Add one to give downloads a quiet hours window.</div>
            </div>
          }

          @for (rule of schedule.rules; track rule.id; let i = $index) {
            <div class="panel">
              <h3>
                {{ rule.name || 'New rule' }}
                @if (isRuleActive(rule)) {
                  <span class="pill ok">● active now</span>
                }
                <span class="hint">
                  <button class="btn sm danger" type="button" (click)="removeScheduleRule(i)">Remove</button>
                </span>
              </h3>
              <div class="body">
                <div class="form">
                  <label>Name</label>
                  <div class="inline">
                    <input type="text" [(ngModel)]="rule.name" placeholder="Quiet hours" />
                  </div>

                  <label>Days</label>
                  <div class="schedule-days" role="group" aria-label="Days this rule starts on">
                    @for (day of weekdays; track day.value) {
                      <label class="schedule-day">
                        <input
                          type="checkbox"
                          [checked]="rule.days.includes(day.value)"
                          (change)="toggleScheduleDay(rule, day.value, $event)"
                        />
                        <span>{{ day.label }}</span>
                      </label>
                    }
                  </div>

                  <label>Window</label>
                  <div class="inline">
                    <input
                      type="text"
                      class="schedule-clock"
                      [(ngModel)]="rule.start"
                      aria-label="Start time"
                      placeholder="HH:MM"
                      inputmode="numeric"
                      autocomplete="off"
                    />
                    <span class="dim">to</span>
                    <input
                      type="text"
                      class="schedule-clock"
                      [(ngModel)]="rule.end"
                      aria-label="End time"
                      placeholder="HH:MM"
                      inputmode="numeric"
                      autocomplete="off"
                    />
                    <span class="dim">24-hour · end earlier than start runs overnight</span>
                  </div>

                  <label>Action</label>
                  <div class="inline">
                    <select [(ngModel)]="rule.action" aria-label="Action">
                      <option value="pause">Pause downloads</option>
                      <option value="speed_limit">Limit speed</option>
                      <option value="disable_servers">Disable servers</option>
                    </select>
                  </div>

                  @if (rule.action === 'speed_limit') {
                    <label>Speed cap</label>
                    <div class="inline">
                      <input type="number" [(ngModel)]="rule.speed_limit_bps" min="0" step="1000" />
                      <span class="dim">bytes/sec · 0 = unlimited (overrides other caps)</span>
                    </div>
                  }

                  @if (rule.action === 'disable_servers') {
                    <label>Servers</label>
                    <div class="schedule-servers">
                      @for (server of servers(); track server.id) {
                        <label class="schedule-day">
                          <input
                            type="checkbox"
                            [checked]="rule.servers.includes(server.id)"
                            (change)="toggleScheduleServer(rule, server.id, $event)"
                          />
                          <span>{{ server.name || server.host }}</span>
                        </label>
                      } @empty {
                        <span class="dim">No news servers configured.</span>
                      }
                    </div>
                  }

                  @if (ruleProblem(rule); as problem) {
                    <div class="schedule-problem" role="alert">{{ problem }}</div>
                  }
                </div>
              </div>
            </div>
          }

          <div class="form-actions">
            <button
              class="btn primary"
              (click)="saveSchedule()"
              [disabled]="!canSave('schedule') || !!scheduleProblem()"
            >
              Save schedule
            </button>
            <button class="btn" (click)="reloadSchedule()" [disabled]="!canSave('schedule')">Discard changes</button>
          </div>
        }

        <!-- =========== MEDIA LIBRARY (DAV) =========== -->
        @if (tab === 'dav') {
          @if (loadFailure('dav'); as message) {
            <div class="load-error" role="alert">
              {{ message }}. Saving is paused until the settings load.
              <button class="btn sm" (click)="loadDavConfig()">Retry</button>
            </div>
          }
          <div class="section-head">
            <div>
              <h2>Media Library (DAV)</h2>
              <div class="sub">Stream completed downloads directly via WebDAV.</div>
            </div>
          </div>

          <div class="panel">
            <h3>Runtime</h3>
            <div class="body">
              <div class="form">
                <label>Enable Media Library</label>
                <div class="inline">
                  <label class="toggle">
                    <input type="checkbox" [(ngModel)]="davConfig.enabled" />
                    <span>Mount <code>/dav</code> and show the Media page</span>
                  </label>
                </div>
                <div class="dim">
                  Changing this flag requires a rustnzb restart. Docker deployments can also enable
                  it with <code>RUSTNZB_DAV_ENABLED=1</code>.
                </div>
              </div>

              @if (davRestartRequired()) {
                <div class="dav-warn">
                  Restart required: the saved DAV setting does not match the running service yet.
                </div>
              }

              <div class="form-actions">
                <button class="btn primary" (click)="saveDavConfig()" [disabled]="!canSave('dav')">Save</button>
              </div>
            </div>
          </div>

          @if (davConfig.enabled) {
            <div class="panel">
              <h3>WebDAV access</h3>
              <div class="body">
                @if (!davAuthConfigured()) {
                  <div class="dav-warn">
                    ⚠ <b>WebDAV is currently unauthenticated.</b> Anyone who can reach
                    <code>{{ davBaseUrl() }}</code> can stream your media. Set a username and
                    password (or an API key) below.
                  </div>
                }

                <div class="form">
                  <label>DAV username</label>
                  <div class="inline">
                    <input
                      type="text"
                      [(ngModel)]="davConfig.username"
                      placeholder="e.g. plex"
                      autocomplete="off"
                    />
                  </div>

                  <label>DAV password</label>
                  <div class="inline">
                    <input
                      [type]="showDavPassword ? 'text' : 'password'"
                      [(ngModel)]="davConfig.password"
                      autocomplete="new-password"
                    />
                    <button
                      class="btn sm"
                      (click)="showDavPassword = !showDavPassword"
                      type="button"
                    >
                      {{ showDavPassword ? 'Hide' : 'Show' }}
                    </button>
                  </div>

                  <label>DAV API key</label>
                  <div class="inline">
                    <input
                      [type]="showDavApiKey ? 'text' : 'password'"
                      [(ngModel)]="davConfig.api_key"
                      autocomplete="off"
                      placeholder="X-Api-Key header value (optional)"
                    />
                    <button class="btn sm" (click)="showDavApiKey = !showDavApiKey" type="button">
                      {{ showDavApiKey ? 'Hide' : 'Show' }}
                    </button>
                    <button class="btn sm" (click)="generateDavApiKey()" type="button">
                      Generate
                    </button>
                  </div>
                </div>

                <div class="form-actions">
                  <button class="btn primary" (click)="saveDavConfig()" [disabled]="!canSave('dav')">Save</button>
                </div>
              </div>
            </div>

            <div class="panel">
              <h3>WebDAV URLs</h3>
              <div class="body">
                <div class="dim" style="margin-bottom: 8px;">
                  Point a WebDAV client (Plex, Infuse, davfs2, rclone) at the root URL. Browseable
                  subpaths are listed for reference — clients only need the root.
                </div>
                <div class="dir-table">
                  <div class="dir-row">
                    <div><b>Root</b></div>
                    <div class="url-cell">
                      <code>{{ davBaseUrl() }}</code>
                      <button class="btn sm" (click)="copy(davBaseUrl())" type="button">
                        Copy
                      </button>
                    </div>
                  </div>
                  <div class="dir-row">
                    <div>Content</div>
                    <div class="url-cell">
                      <code>{{ davBaseUrl() }}/content</code>
                      <button
                        class="btn sm"
                        (click)="copy(davBaseUrl() + '/content')"
                        type="button"
                      >
                        Copy
                      </button>
                    </div>
                  </div>
                  <div class="dir-row">
                    <div>NZBs</div>
                    <div class="url-cell">
                      <code>{{ davBaseUrl() }}/nzbs</code>
                      <button class="btn sm" (click)="copy(davBaseUrl() + '/nzbs')" type="button">
                        Copy
                      </button>
                    </div>
                  </div>
                  <div class="dir-row">
                    <div>Completed symlinks</div>
                    <div class="url-cell">
                      <code>{{ davBaseUrl() }}/completed-symlinks</code>
                      <button
                        class="btn sm"
                        (click)="copy(davBaseUrl() + '/completed-symlinks')"
                        type="button"
                      >
                        Copy
                      </button>
                    </div>
                  </div>
                </div>
                <div class="dim dav-note">
                  Note: WebDAV clients must use the root URL <b>without a trailing slash</b>
                  (Axum nest quirk). Append-paths shown above already follow this rule.
                </div>
              </div>
            </div>

            <div class="panel">
              <h3>Auto-send to Media Library</h3>
              <div class="body">
                <div class="form">
                  <label>Send all downloads</label>
                  <div class="inline">
                    <label class="toggle">
                      <input
                        type="checkbox"
                        [(ngModel)]="davConfig.auto_send_all"
                        (change)="onAutoSendAllChange()"
                      />
                      <span
                        >Automatically queue every completed download into the Media Library</span
                      >
                    </label>
                  </div>

                  @if (!davConfig.auto_send_all) {
                    <label>Auto-send categories</label>
                    <div class="dav-cats">
                      @if (categories().length === 0) {
                        <span class="dim">No categories configured — add categories first.</span>
                      }
                      @for (cat of categories(); track cat.name) {
                        <label class="check">
                          <input
                            type="checkbox"
                            [checked]="davConfig.category_rules.includes(cat.name)"
                            (change)="toggleDavCategory(cat.name, $event)"
                          />
                          {{ cat.name }}
                        </label>
                      }
                    </div>
                  }
                </div>
                <div class="form-actions">
                  <button class="btn primary" (click)="saveDavConfig()" [disabled]="!canSave('dav')">Save</button>
                </div>
              </div>
            </div>
          } @else {
            <div class="panel">
              <div class="body">
                <div class="dim">
                  DAV access, Media navigation, and automatic media-library ingest stay disabled
                  until the runtime flag above is enabled and the service is restarted.
                </div>
              </div>
            </div>
          }
        }

        <!-- =========== GENERAL =========== -->
        @if (tab === 'general') {
          @if (loadFailure('speedLimit') ?? loadFailure('maxActive') ?? loadFailure('historyRetention') ?? loadFailure('debugLogging'); as message) {
            <div class="load-error" role="alert">
              {{ message }}. Saving is paused until the settings load.
              <button class="btn sm" (click)="loadGeneralSettings()">Retry</button>
            </div>
          }
          <div class="section-head">
            <div>
              <h2>{{ 'settings.general.title' | t }}</h2>
              <div class="sub">{{ 'settings.general.sub' | t }}</div>
            </div>
          </div>

          <div class="panel">
            <h3>{{ 'settings.appearance.title' | t }}</h3>
            <div class="body">
              <div class="theme-grid" role="radiogroup" [attr.aria-label]="'settings.appearance.themeLabel' | t">
                <button
                  type="button"
                  class="theme-option"
                  [class.active]="theme.followsSystem()"
                  role="radio"
                  [attr.aria-checked]="theme.followsSystem()"
                  (click)="followSystemTheme()"
                >
                  <span class="theme-swatch system" aria-hidden="true">⌂</span>
                  <span class="theme-copy">
                    <b>{{ 'settings.appearance.system' | t }}</b>
                    <span class="setting-description">{{ 'settings.appearance.systemDescription' | t }}</span>
                  </span>
                </button>
                @for (option of theme.options; track option.id) {
                  <button
                    type="button"
                    class="theme-option"
                    [class.active]="!theme.followsSystem() && theme.current() === option.id"
                    role="radio"
                    [attr.aria-checked]="!theme.followsSystem() && theme.current() === option.id"
                    (click)="setTheme(option.id)"
                  >
                    <span class="theme-swatch" [attr.data-preview-theme]="option.id">
                      <i></i><i></i><i></i>
                    </span>
                    <span class="theme-copy">
                      <b>{{ option.name }}</b>
                      <span class="setting-description">{{ option.description }}</span>
                    </span>
                  </button>
                }
              </div>
            </div>
          </div>

          <div class="panel">
            <h3>{{ 'settings.language.title' | t }}</h3>
            <div class="body">
              <div class="form">
                <label for="ui-locale">{{ 'settings.language.label' | t }}</label>
                <div>
                  <select
                    id="ui-locale"
                    [ngModel]="i18n.locale()"
                    (ngModelChange)="i18n.set($event)"
                  >
                    @for (option of i18n.locales; track option.id) {
                      <option [value]="option.id" [attr.lang]="option.id">{{ option.name }}</option>
                    }
                  </select>
                  <div class="setting-description">{{ 'settings.language.description' | t }}</div>
                </div>
              </div>
            </div>
          </div>

          <div class="panel">
            <h3>Browser notifications</h3>
            <div class="body">
              <div class="form">
                <label>Desktop alerts</label>
                <div class="inline">
                  <label class="toggle">
                    <input
                      type="checkbox"
                      [checked]="notifications.enabled()"
                      [disabled]="!notifications.supported"
                      (change)="toggleNotifications($event)"
                    />
                    <span>Alert when a download completes or fails, or downloads pause for low disk space</span>
                  </label>
                </div>
                <div class="dim">
                  Stored in this browser only. Alerts appear while this tab is in the background.
                  @if (!notifications.supported) {
                    This browser does not support notifications.
                  } @else if (notifications.permission() === 'denied') {
                    The browser is blocking notifications for this site.
                  }
                </div>
              </div>
            </div>
          </div>

          <div class="panel">
            <h3>Speed &amp; concurrency</h3>
            <div class="body">
              <div class="form">
                <label>Global speed limit</label>
                <div class="inline">
                  <input type="number" [(ngModel)]="speedLimit" min="0" />
                  <span style="color:var(--mute)">bytes/sec · 0 = unlimited</span>
                  <button class="btn sm" (click)="saveSpeedLimit()" [disabled]="!canSave('speedLimit')">Save</button>
                </div>

                <label>Concurrent jobs</label>
                <div class="inline">
                  <input type="number" [(ngModel)]="maxActiveDownloads" min="1" />
                  <span class="help"
                    >Max jobs in Downloading state</span
                  >
                  <button class="btn sm" (click)="saveMaxActive()" [disabled]="!canSave('maxActive')">Save</button>
                </div>

                <label>History retention</label>
                <div class="inline">
                  <input type="number" [(ngModel)]="historyRetention" min="0" />
                  <span style="color:var(--mute)">days · blank = keep all</span>
                  <button class="btn sm" (click)="saveRetention()" [disabled]="!canSave('historyRetention')">Save</button>
                </div>
              </div>
            </div>
          </div>

          <div class="panel">
            <h3>Debug logging</h3>
            <div class="body">
              <div class="form">
                <label>Verbose download tracing</label>
                <div class="inline">
                  <label class="toggle">
                    <input
                      type="checkbox"
                      [(ngModel)]="debugLogging"
                      [disabled]="debugLoggingOverridden || !canSave('debugLogging')"
                      (change)="saveDebugLogging()"
                    />
                    <span>Log every stage of a download at debug level</span>
                  </label>
                </div>
                @if (debugLoggingOverridden) {
                  <div class="dim">
                    Overridden by the environment: this process runs
                    <code>{{ debugLoggingEffective }}</code>. RUST_LOG, --log-level or
                    RUSTNZB_LOG_LEVEL beats the saved choice, so this switch does not change it.
                  </div>
                } @else {
                  <div class="dim">
                    Effective level: <code>{{ debugLoggingEffective }}</code>. Applies at
                    once and is kept across restarts; turning it off restores the previous
                    level. It raises the log volume sharply while a queue is running, so
                    turn it off again once you have what you need.
                  </div>
                }
              </div>
            </div>
          </div>
        }

        <!-- =========== API / SABnzbd =========== -->
        @if (tab === 'api') {
          @if (loadFailure('sabApiKey'); as message) {
            <div class="load-error" role="alert">
              {{ message }}. Generating or rotating the key is paused until it loads.
              <button class="btn sm" (click)="loadSabApiKey()">Retry</button>
            </div>
          }
          <div class="section-head">
            <div>
              <h2>API &amp; SABnzbd compatibility</h2>
              <div class="sub">Native REST + drop-in SABnzbd API for Sonarr / Radarr / Lidarr.</div>
            </div>
          </div>

          <div class="panel">
            <h3>SABnzbd endpoint</h3>
            <div class="body">
              <p style="margin:0 0 10px">
                Point Sonarr/Radarr at this host — category matching is done by category name.
              </p>
              <div class="url-cell">
                <code>{{ sabnzbdExample }}</code>
                <button class="btn sm" (click)="copy(sabnzbdExample)" type="button">Copy</button>
              </div>
              <div class="form" style="margin-top:16px">
                <label>Supported modes</label>
                <ul class="code-list">
                  @for (mode of sabnzbdModes; track mode) {
                    <li><code>{{ mode }}</code></li>
                  }
                </ul>
              </div>
            </div>
          </div>

          <div class="panel">
            <h3>SABnzbd API key</h3>
            <div class="body">
              <p class="dim" style="margin-top:0">
                This key grants access to the SABnzbd-compatible endpoint. Reveal it only when
                configuring an integration; rotating it immediately invalidates the previous key.
              </p>
              <div class="inline">
                <input
                  [type]="showSabApiKey ? 'text' : 'password'"
                  [value]="sabApiKey || ''"
                  readonly
                  autocomplete="off"
                  placeholder="No SABnzbd API key configured"
                />
                <button class="btn sm" (click)="showSabApiKey = !showSabApiKey" type="button">
                  {{ showSabApiKey ? 'Hide' : 'Reveal' }}
                </button>
                <button class="btn sm" [disabled]="!sabApiKey" (click)="copy(sabApiKey!)" type="button">
                  Copy
                </button>
                <button class="btn sm danger" (click)="rotateSabApiKey()" [disabled]="!canSave('sabApiKey')" type="button">
                  {{ sabApiKey ? 'Rotate' : 'Generate' }}
                </button>
              </div>
            </div>
          </div>

          <div class="panel">
            <h3>Network <span class="restart-tag">restart required</span></h3>
            <div class="body">
              @if (loadFailure('network'); as message) {
                <div class="load-error" role="alert">
                  {{ message }}. Saving is paused until the settings load.
                  <button class="btn sm" (click)="loadDirs()">Retry</button>
                </div>
              }
              <div class="form">
                <label for="listen-addr">Listen address</label>
                <input id="listen-addr" type="text" [(ngModel)]="networkForm.listen_addr" placeholder="0.0.0.0" />

                <label for="listen-port">Port</label>
                <input id="listen-port" type="number" [(ngModel)]="networkForm.port" min="1" max="65535" />
              </div>
              <div class="dim" style="margin-top:10px">
                Read when rustnzb starts. In Docker, the container port mapping must follow a port
                change. A <code>--port</code>, <code>--listen-addr</code> or environment override still
                wins over the saved value. Serving under a base URL (sub-path) is not supported; put
                rustnzb at the root of its host.
              </div>
              <div class="form-actions">
                <button class="btn primary" (click)="saveNetwork()" [disabled]="!canSave('network')">Save network</button>
              </div>
            </div>
          </div>

          <div class="panel">
            <h3>OpenAPI / Swagger</h3>
            <div class="body">
              <a href="/swagger-ui" target="_blank">Open <code>/swagger-ui</code></a>
              — generated by <code>utoipa</code> from the live route handlers.
            </div>
          </div>
        }

        <!-- =========== TELEMETRY =========== -->
        @if (tab === 'telemetry') {
          <div class="section-head">
            <div>
              <h2>Logging &amp; telemetry</h2>
              <div class="sub">tracing filters, file rotation, OpenTelemetry OTLP.</div>
            </div>
          </div>

          <div class="panel">
            <h3>Tracing</h3>
            <div class="body">
              Configured via env at startup: <code>RUSTNZB_LOG_LEVEL</code> (default
              <code>info</code>) or <code>RUST_LOG</code> for fine-grained per-target filters (e.g.
              <code>nzb_nntp=debug,nzb_web=info</code>). Live logs are on the
              <a routerLink="/logs">Logs page</a>.
            </div>
          </div>

          <div class="panel">
            <h3>OpenTelemetry (OTLP gRPC)</h3>
            <div class="body">
              Enabled via <code>OTEL_ENABLED=true</code>, <code>OTEL_EXPORTER_OTLP_ENDPOINT</code>,
              <code>OTEL_SERVICE_NAME</code>. A mutable UI for these will land once the backend
              exposes a config endpoint.
            </div>
          </div>
        }

        <!-- =========== ABOUT =========== -->
        <!-- =========== BACKUP & RESTORE =========== -->
        @if (tab === 'backup') {
          <div class="section-head">
            <div>
              <h2>Backup &amp; restore</h2>
              <div class="sub">
                Export every setting as one TOML file, or restore one onto this install.
              </div>
            </div>
          </div>

          <div class="panel">
            <h3>
              Export
              <span class="hint">a file you can keep, or share</span>
            </h3>
            <div class="body">
              <div class="form">
                <label for="backup-secrets">Include secrets</label>
                <div class="inline">
                  <input
                    id="backup-secrets"
                    type="checkbox"
                    [(ngModel)]="backupIncludeSecrets"
                  />
                  <span>
                    Write real credentials into the file. Leave this off unless you are
                    moving the install: a masked file is safe to attach to an issue.
                  </span>
                </div>
              </div>
              @if (backupIncludeSecrets) {
                <div class="dav-warn" role="alert">
                  This file will contain your API key, news-server passwords, proxy
                  credentials and notification URLs in clear text. Store it like a password
                  and delete it once the move is done.
                </div>
              }
              <div class="form-actions">
                <button class="btn primary" [disabled]="backupBusy" (click)="downloadBackup()">
                  Download backup
                </button>
                <span class="dim">
                  {{ backupIncludeSecrets ? 'Includes secrets.' : 'Secrets are masked.' }}
                </span>
              </div>
            </div>
          </div>

          <div class="panel">
            <h3>
              Restore
              <span class="hint">checked before anything is written</span>
            </h3>
            <div class="body">
              <div class="form">
                <label for="backup-file">Backup file</label>
                <div class="inline">
                  <input
                    id="backup-file"
                    type="file"
                    accept=".toml,application/toml,text/plain"
                    (change)="chooseBackupFile($event)"
                  />
                  <span class="dim">{{ backupFileName || 'No file chosen' }}</span>
                </div>
              </div>
              <div class="form-actions">
                <button
                  class="btn"
                  [disabled]="backupBusy || !backupText"
                  (click)="previewBackup()"
                >
                  Preview
                </button>
                <button
                  class="btn primary"
                  [disabled]="backupBusy || !backupPreviewMatchesFile"
                  (click)="restoreBackup()"
                >
                  Restore
                </button>
              </div>

              @if (backupError) {
                <div class="load-error" role="alert">{{ backupError }}</div>
              }

              @if (backupPreview; as report) {
                <div class="backup-summary">
                  <div class="backup-line">
                    <b>{{ report.backup.app_version || 'unknown build' }}</b>
                    @if (report.backup.created_at) {
                      <span class="dim">exported {{ report.backup.created_at }}</span>
                    }
                    @if (report.backup.secrets_included) {
                      <span class="pill warn">includes secrets</span>
                    } @else {
                      <span class="pill ok">masked</span>
                    }
                  </div>
                  <div class="backup-line">
                    {{ report.summary.server_count }} server(s) ·
                    {{ report.summary.category_count }} category(ies) ·
                    {{ report.summary.rss_feed_count }} feed(s) ·
                    {{ report.summary.secrets_kept }} secret(s) kept from this install
                  </div>
                  @if (report.summary.warnings.length) {
                    <div class="dav-warn" role="alert">
                      <b>Some masked secrets have nothing to keep here.</b>
                      <ul>
                        @for (warning of report.summary.warnings; track warning) {
                          <li>{{ warning }}</li>
                        }
                      </ul>
                    </div>
                  }
                  @if (report.restart_required.length) {
                    <div class="backup-line">
                      Takes effect after a restart:
                      @for (key of report.restart_required; track key) {
                        <code>{{ key }}</code>
                      }
                    </div>
                  } @else {
                    <div class="dim">Every changed setting applies immediately.</div>
                  }
                  <div class="dim">
                    Restoring replaces the saved configuration: a setting missing from the
                    file returns to its default. Jobs, history and the database are not touched.
                  </div>
                </div>
              }
            </div>
          </div>
        }

        @if (tab === 'about') {
          <div class="section-head">
            <div>
              <h2>About</h2>
              <div class="sub">Build info, versions, license.</div>
            </div>
          </div>

          <div class="panel">
            <div class="body">
              <div class="form">
                <label>Version</label>
                <div>{{ status()?.version || '—' }}</div>
                <label>Rust edition</label>
                <div>2024</div>
                <label>Web framework</label>
                <div>Axum 0.8 + Tower</div>
                <label>TLS</label>
                <div>rustls 0.23 (ring)</div>
                <label>Database</label>
                <div>SQLite · WAL mode · bundled</div>
                <label>License</label>
                <div>MIT</div>
                <label>Source</label>
                <div>
                  <a href="https://github.com/TheDancingDeveloper-org/rustnzb" target="_blank" rel="noopener"
                    >github.com/TheDancingDeveloper-org/rustnzb</a
                  >
                </div>
              </div>
            </div>
          </div>
        }
      </div>
    </div>
  `,
  styles: [
    `
      :host {
        display: block;
      }

      .settings-shell {
        display: grid;
        grid-template-columns: 220px 1fr;
        gap: 16px;
        align-items: start;
      }

      .settings-side {
        background: var(--panel);
        border: 1px solid var(--line);
        border-radius: 8px;
        padding: 8px;
        width: 100%;
        box-sizing: border-box;
        align-self: start;
        position: static;
        max-height: none;
        overflow: visible;
      }
      .settings-side .sg {
        color: var(--mute);
        font-size: 12px;
        font-weight: 600;
        text-transform: uppercase;
        letter-spacing: 0.7px;
        padding: 14px 12px 4px;
        margin-top: 2px;
        border-top: 1px solid var(--line);
      }
      .settings-side .sg:first-child {
        border-top: none;
        padding-top: 6px;
        margin-top: 0;
      }
      .settings-side button {
        display: block;
        width: 100%;
        text-align: left;
        background: none;
        border: none;
        color: var(--text);
        padding: 7px 12px;
        border-radius: 5px;
        cursor: pointer;
        font: inherit;
        font-size: 13px;
        opacity: 0.7;
      }
      .settings-side button:hover {
        opacity: 1;
        background: var(--panel2);
      }
      .settings-side button.active {
        opacity: 1;
        background: var(--panel2);
        box-shadow: inset 2px 0 0 var(--accent);
      }

      .settings-main {
        min-width: 0;
        padding-top: 10px;
      }

      /* Server rows */
      .srv-row {
        display: grid;
        grid-template-columns: 28px 1fr auto;
        gap: 12px;
        align-items: center;
        padding: 12px 14px;
        border-bottom: 1px solid var(--line);
      }
      .srv-row:last-child {
        border: none;
      }
      .drag {
        color: var(--mute);
        cursor: grab;
        text-align: center;
      }
      .title {
        font-weight: 600;
      }
      .title.dim {
        color: var(--mute);
        /* Dim only: keep the size of the enabled row (overrides the global .dim size) */
        font-size: inherit;
      }
      .host {
        color: var(--mute);
        font-size: 12px;
        margin-top: 2px;
      }
      .meters {
        display: flex;
        gap: 18px;
        align-items: center;
        font-size: 12px;
        color: var(--mute);
        margin-top: 6px;
        flex-wrap: wrap;
      }
      .meters b {
        color: var(--text);
        font-weight: 600;
      }
      .actions {
        display: flex;
        gap: 4px;
        flex-wrap: wrap;
      }
      .btn.sm.active {
        background: var(--accent-fill);
        color: #fff;
        border-color: var(--accent-fill);
      }

      /* Server stats panel */
      .srv-stats-panel {
        grid-column: 1 / -1;
        padding: 12px 14px 14px 42px;
        background: var(--surface2, rgba(255, 255, 255, 0.03));
        border-bottom: 1px solid var(--line);
      }
      .srv-stats-grid {
        display: flex;
        gap: 32px;
        flex-wrap: wrap;
      }
      .srv-stats-col {
        min-width: 200px;
      }
      .srv-stats-heading {
        font-size: 11px;
        font-weight: 600;
        text-transform: uppercase;
        letter-spacing: 0.05em;
        color: var(--mute);
        margin-bottom: 6px;
      }
      .srv-stats-row {
        display: flex;
        justify-content: space-between;
        align-items: baseline;
        font-size: 12px;
        color: var(--mute);
        padding: 2px 0;
        gap: 16px;
      }
      .srv-stats-row b {
        color: var(--text);
        font-weight: 600;
        white-space: nowrap;
      }

      .empty {
        padding: 24px;
        color: var(--mute);
        text-align: center;
        font-size: 13px;
      }
      .empty-cell {
        text-align: center;
        padding: 28px !important;
        color: var(--mute);
        font-size: 13px;
      }

      .form-actions {
        margin-top: 14px;
        display: flex;
        gap: 8px;
      }

      .dav-cats {
        display: flex;
        flex-direction: column;
        gap: 6px;
        margin-top: 4px;
      }
      .dim {
        color: var(--mute);
        font-size: 12px;
      }

      .btn:disabled {
        opacity: 0.5;
        cursor: not-allowed;
      }
      .backup-summary {
        display: flex;
        flex-direction: column;
        gap: 8px;
        margin-top: 14px;
        padding-top: 14px;
        border-top: 1px solid var(--line);
        font-size: 13px;
      }
      .backup-line {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        gap: 8px;
      }
      .backup-summary code {
        background: var(--panel2);
        padding: 1px 5px;
        border-radius: 3px;
        font-size: 12px;
      }
      .backup-summary ul {
        margin: 6px 0 0;
        padding-left: 18px;
      }
      .backup-summary .dav-warn {
        margin-bottom: 0;
      }
      .restart-banner {
        background: rgba(234, 179, 8, 0.1);
        border: 1px solid var(--warn, #eab308);
        border-radius: 6px;
        padding: 8px 12px;
        margin-bottom: 14px;
        font-size: 13px;
      }
      .restart-tag {
        display: inline-block;
        font-size: 11px;
        padding: 0 6px;
        border-radius: 4px;
        border: 1px solid var(--line);
        color: var(--mute);
        white-space: nowrap;
      }
      .path-warn {
        margin-top: 6px;
        font-size: 12px;
        color: var(--warn, #b45309);
      }
      .load-error {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        gap: 10px;
        background: rgba(239, 68, 68, 0.08);
        border: 1px solid var(--danger);
        color: var(--text);
        border-radius: 6px;
        padding: 10px 12px;
        margin-bottom: 14px;
        font-size: 13px;
        line-height: 1.5;
      }
      .dav-warn {
        background: rgba(255, 180, 0, 0.08);
        border: 1px solid rgba(255, 180, 0, 0.35);
        color: var(--text);
        border-radius: 6px;
        padding: 10px 12px;
        margin-bottom: 14px;
        font-size: 13px;
        line-height: 1.5;
      }
      .dav-warn code {
        background: rgba(0, 0, 0, 0.25);
        padding: 1px 4px;
        border-radius: 3px;
      }
      .url-cell {
        display: flex;
        gap: 8px;
        align-items: center;
        flex-wrap: wrap;
      }
      .url-cell code {
        background: var(--panel2, rgba(255, 255, 255, 0.04));
        padding: 3px 7px;
        border-radius: 4px;
        font-size: 12px;
        word-break: break-all;
      }

      /* Scheduler */
      .schedule-days,
      .schedule-servers {
        display: flex;
        flex-wrap: wrap;
        gap: 6px 12px;
        align-items: center;
      }
      .schedule-day {
        display: inline-flex;
        align-items: center;
        gap: 5px;
        font-size: 13px;
        color: var(--text);
        background: var(--panel2);
        border: 1px solid var(--line);
        border-radius: var(--radius-md);
        padding: 4px 9px;
        cursor: pointer;
      }
      .schedule-day:has(input:checked) {
        border-color: var(--accent);
        box-shadow: inset 0 0 0 1px var(--accent);
      }
      .schedule-day input {
        margin: 0;
        accent-color: var(--accent);
      }
      .schedule-now {
        display: flex;
        flex-wrap: wrap;
        gap: 6px;
        align-items: center;
      }
      .schedule-problem {
        grid-column: 2;
        color: var(--danger);
        font-size: 12px;
      }
      .form .schedule-clock {
        max-width: 90px;
        font-variant-numeric: tabular-nums;
      }
      @media (max-width: 720px) {
        .form { grid-template-columns: 1fr; }
        .form .help,
        .schedule-problem { grid-column: 1; }
      }

      .dir-table {
        display: flex;
        flex-direction: column;
        gap: 0;
        font-size: 13px;
      }
      .dir-row {
        display: grid;
        grid-template-columns: 100px 1fr;
        gap: 8px 12px;
        align-items: baseline;
        padding: 7px 0;
        border-bottom: 1px solid var(--line);
      }
      .dir-row:last-child {
        border: none;
      }
      .dir-label {
        font-weight: 600;
        white-space: nowrap;
      }
      .dir-val {
        grid-column: 2;
        font-size: 12px;
      }
      .dir-hint {
        grid-column: 2;
        color: var(--mute);
        font-size: 12px;
        margin-top: -2px;
      }
      .settings-main .sub,
      .settings-main .host,
      .settings-main .meters span,
      .settings-main .setting-description,
      .settings-main .form .inline > span {
        font-family: inherit;
        font-style: normal;
      }
      .theme-grid {
        display: grid;
        grid-template-columns: repeat(4, minmax(0, 1fr));
        gap: 10px;
      }
      .theme-option {
        display: flex;
        align-items: center;
        gap: 10px;
        min-width: 0;
        padding: 10px;
        text-align: left;
        color: var(--text);
        background: var(--panel2);
        border: 1px solid var(--line);
        border-radius: 7px;
        font: inherit;
        cursor: pointer;
      }
      @media (max-width: 1100px) {
        .theme-grid { grid-template-columns: repeat(2, minmax(0, 1fr)); }
      }
      .theme-option:hover,
      .theme-option.active { border-color: var(--accent); }
      .theme-option.active { box-shadow: inset 0 0 0 1px var(--accent); }
      .theme-swatch {
        width: 42px;
        height: 32px;
        border-radius: 5px;
        padding: 5px;
        flex: 0 0 auto;
        display: flex;
        gap: 3px;
        align-items: flex-end;
        background: #0f1419;
        border: 1px solid #2a3441;
      }
      .theme-swatch[data-preview-theme='midnight'] { background: #080d1a; border-color: #263552; }
      .theme-swatch[data-preview-theme='light'] { background: #f3f6fa; border-color: #d2d9e3; }
      .theme-swatch.system {
        align-items: center;
        justify-content: center;
        background: linear-gradient(110deg, #0f1419 50%, #f3f6fa 50%);
        border-color: var(--line-strong);
        color: var(--text);
        font-size: 14px;
      }
      .theme-swatch i { display: block; width: 8px; background: #3b82f6; border-radius: 2px; }
      .theme-swatch i:nth-child(1) { height: 11px; }
      .theme-swatch i:nth-child(2) { height: 20px; }
      .theme-swatch i:nth-child(3) { height: 15px; }
      .theme-swatch[data-preview-theme='midnight'] i { background: #8b7cf6; }
      .theme-swatch[data-preview-theme='light'] i { background: #2563eb; }
      .theme-copy { min-width: 0; display: flex; flex-direction: column; }
      .theme-copy b { font-size: 12px; }
      .setting-description { color: var(--mute); font-size: 12px; margin-top: 2px; }
      .help { color: var(--mute); font-size: 12px; }
      .prose-secondary { color: var(--mute); font-size: 13px; }
      .dav-note { margin-top: 10px; }
      @media (max-width: 850px) {
        .theme-grid { grid-template-columns: 1fr; }
      }
      @media (max-width: 720px) {
        .settings-shell { grid-template-columns: 1fr; }
      }
    `,
  ],
})
export class SettingsViewComponent implements OnInit, HasUnsavedChanges {
  tab: Tab = 'servers';

  readonly sabnzbdModes = [
    'addfile', 'addurl', 'queue', 'history', 'config', 'fullstatus',
    'version', 'pause', 'resume', 'delete', 'retry',
  ];
  readonly sabnzbdExample = `${typeof location !== 'undefined' ? location.origin : 'http://host:9090'}/sabnzbd/api?apikey=...&mode=queue`;

  // Servers
  servers = signal<ServerConfig[]>([]);
  serverStats = signal<Record<string, ServerStats>>({});
  expandedStatsId: string | null = null;
  editingServer: ServerConfig | null = null;
  editingServerId: string | null = null;

  // Categories
  categories = signal<CategoryConfig[]>([]);
  editingCategory: CategoryConfig | null = null;
  editingCategoryOriginalName: string | null = null;

  // General
  speedLimit = 0;
  maxActiveDownloads = 3;
  historyRetention: number | null = null;
  debugLogging = false;
  debugLoggingOverridden = false;
  debugLoggingEffective = '';

  // Disk guards
  minFreeSpaceGB = 1;
  abortHopeless = true;

  // Scheduler
  schedule: ScheduleState = {
    enabled: false,
    rules: [],
    active_rule_ids: [],
    paused: false,
    speed_limit_bps: null,
    disabled_servers: [],
  };
  readonly weekdays = [
    { value: 'mon', label: 'Mon' },
    { value: 'tue', label: 'Tue' },
    { value: 'wed', label: 'Wed' },
    { value: 'thu', label: 'Thu' },
    { value: 'fri', label: 'Fri' },
    { value: 'sat', label: 'Sat' },
    { value: 'sun', label: 'Sun' },
  ];

  // Status / feature flags
  status = signal<StatusResponse | null>(null);
  webdavAvailable = computed(() => this.status()?.webdav_available ?? false);
  davRuntimeEnabled = computed(() => this.status()?.webdav_enabled ?? false);

  // DAV config
  davConfig: DavConfig = {
    enabled: false,
    auto_send_all: false,
    category_rules: [],
    username: null,
    password: null,
    api_key: null,
  };
  showDavPassword = false;
  showDavApiKey = false;
  sabApiKey: string | null = null;
  showSabApiKey = false;

  // Directory paths (from /api/config)
  dirs = signal<GeneralDirs | null>(null);
  pathsForm: PathsForm = { complete_dir: '', incomplete_dir: '', watch_dir: '' };
  postprocForm: PostprocForm = {
    direct_unpack: true,
    max_nested_archive_depth: 5,
    max_post_processing_jobs: 2,
    max_repair_workers: 1,
    max_extract_workers: 1,
    scripts_dir: '',
    script_success: '',
    script_failure: '',
    script_timeout_secs: 300,
    script_max_output_bytes: 65536,
    par2_defer_volumes: true,
    par2_trust_inflight_verify: true,
  };
  networkForm: NetworkForm = { listen_addr: '', port: 8080 };
  /** Saved settings that take effect only after a restart. */
  restartPending = signal<string[]>([]);
  categoryCleanupText = '';
  categoryExtText = '';
  private editingCategoryOriginalOutput: string | null = null;

  // Backup & restore
  backupIncludeSecrets = false;
  backupBusy = false;
  backupFileName = '';
  backupText = '';
  backupPreview: BackupReport | null = null;
  /** The exact text the current preview checked, so a stale preview cannot be restored. */
  private backupPreviewText = '';
  backupError = '';

  // Load state per setting. A form's values are only defaults until its GET succeeds,
  // so Save stays blocked until then; otherwise it would write those defaults over the real config.
  loads = signal<Record<SettingsKey, LoadState>>(allLoading());

  // What each form held when it was last loaded or saved; a form differing from it has unsaved edits.
  private baselines: Partial<Record<FormKey, string>> = {};

  constructor(
    private api: ApiService,
    private toast: ToastService,
    private confirmSvc: ConfirmService,
    public readonly theme: ThemeService,
    public readonly notifications: BrowserNotificationsService,
    public readonly i18n: I18nService = new I18nService(),
  ) {}

  ngOnInit(): void {
    this.loadServers();
    this.loadCategories();
    this.loadGeneralSettings();
    this.loadSchedule();
    this.loadSabApiKey();
    this.loadStatus();
    this.loadDirs();
  }

  canSave(key: SettingsKey): boolean {
    return this.loads()[key].status === 'loaded';
  }

  loadFailure(key: SettingsKey): string | null {
    const state = this.loads()[key];
    return state.status === 'failed' ? state.message : null;
  }

  private formSnapshot(key: FormKey): string {
    switch (key) {
      case 'speedLimit':
        return JSON.stringify(this.speedLimit);
      case 'maxActive':
        return JSON.stringify(this.maxActiveDownloads);
      case 'historyRetention':
        return JSON.stringify(this.historyRetention);
      case 'diskGuards':
        return JSON.stringify([this.minFreeSpaceGB, this.abortHopeless]);
      case 'schedule':
        return JSON.stringify({ enabled: this.schedule.enabled, rules: this.schedule.rules });
      case 'dav':
        return JSON.stringify(this.davConfig);
      case 'paths':
        return JSON.stringify(this.pathsForm);
      case 'postproc':
        return JSON.stringify(this.postprocForm);
      case 'network':
        return JSON.stringify(this.networkForm);
    }
  }

  /** Names of the sections with edits that have not been saved, in page order. */
  unsavedSections(): string[] {
    const out: string[] = [];
    if (this.editingServer) out.push('server');
    if (this.editingCategory) out.push('category');
    for (const key of FORM_KEYS) {
      const base = this.baselines[key];
      if (base !== undefined && base !== this.formSnapshot(key)) out.push(key);
    }
    return out;
  }

  hasUnsavedChanges(): boolean {
    return this.unsavedSections().length > 0;
  }

  // Covers reloads and closing the tab, which the router guard can't see.
  @HostListener('window:beforeunload', ['$event'])
  onBeforeUnload(event: BeforeUnloadEvent): void {
    if (this.hasUnsavedChanges()) event.preventDefault();
  }

  /** PUT one form, and on success make what was sent its new baseline (later edits stay unsaved). */
  private saveForm(key: FormKey, path: string, body: unknown, saved: string, fallback: string): void {
    if (!this.canSave(key)) return;
    const sent = this.formSnapshot(key);
    this.api.put(path, body).subscribe({
      next: () => {
        this.baselines[key] = sent;
        this.toast.success(saved);
      },
      error: (err) => showHttpError(this.toast, err, fallback),
    });
  }

  private setLoad(key: SettingsKey, state: LoadState): void {
    this.loads.update((all) => ({ ...all, [key]: state }));
  }

  /** GET one setting. `apply` runs only on success; a failure is recorded and shown, and keeps Save blocked. */
  private loadSetting<T>(key: SettingsKey, path: string, fallback: string, apply: (r: T) => void): void {
    this.setLoad(key, { status: 'loading' });
    this.api.get<T>(path).subscribe({
      next: (r) => {
        apply(r);
        if (isFormKey(key)) this.baselines[key] = this.formSnapshot(key);
        this.setLoad(key, { status: 'loaded' });
      },
      error: (err) => this.setLoad(key, { status: 'failed', message: httpErrorMessage(err, fallback) }),
    });
  }

  loadStatus(): void {
    this.api.get<StatusResponse>('/status').subscribe({
      next: (s) => {
        this.status.set(s);
        if (s.webdav_available) this.loadDavConfig();
      },
      error: (err) => showHttpError(this.toast, err, 'Failed to load server status'),
    });
  }

  /** GET the config document behind the paths, post-processing and network forms. */
  loadDirs(): void {
    for (const key of CONFIG_FORM_KEYS) this.setLoad(key, { status: 'loading' });
    this.api.get<{ general: GeneralDirs; par2?: Par2Settings }>('/config').subscribe({
      next: (cfg) => {
        if (!cfg?.general) {
          for (const key of CONFIG_FORM_KEYS) this.setLoad(key, { status: 'failed', message: 'Failed to load settings' });
          return;
        }
        this.applyConfig(cfg.general, cfg.par2);
        for (const key of CONFIG_FORM_KEYS) {
          this.baselines[key] = this.formSnapshot(key);
          this.setLoad(key, { status: 'loaded' });
        }
      },
      error: (err) => {
        const message = httpErrorMessage(err, 'Failed to load settings');
        for (const key of CONFIG_FORM_KEYS) this.setLoad(key, { status: 'failed', message });
      },
    });
    this.loadRestartPending();
  }

  private applyConfig(g: GeneralDirs, par2?: Par2Settings): void {
    this.dirs.set(g);
    this.pathsForm = {
      complete_dir: g.complete_dir ?? '',
      incomplete_dir: g.incomplete_dir ?? '',
      watch_dir: g.watch_dir ?? '',
    };
    const p = this.postprocForm;
    this.postprocForm = {
      direct_unpack: g.direct_unpack ?? p.direct_unpack,
      max_nested_archive_depth: g.max_nested_archive_depth ?? p.max_nested_archive_depth,
      max_post_processing_jobs: g.max_post_processing_jobs ?? p.max_post_processing_jobs,
      max_repair_workers: g.max_repair_workers ?? p.max_repair_workers,
      max_extract_workers: g.max_extract_workers ?? p.max_extract_workers,
      scripts_dir: g.scripts_dir ?? '',
      script_success: g.script_success ?? '',
      script_failure: g.script_failure ?? '',
      script_timeout_secs: g.script_timeout_secs ?? p.script_timeout_secs,
      script_max_output_bytes: g.script_max_output_bytes ?? p.script_max_output_bytes,
      par2_defer_volumes: par2?.defer_volumes ?? p.par2_defer_volumes,
      par2_trust_inflight_verify: par2?.trust_inflight_verify ?? p.par2_trust_inflight_verify,
    };
    this.networkForm = { listen_addr: g.listen_addr ?? '', port: g.port ?? this.networkForm.port };
  }

  loadRestartPending(): void {
    this.api.get<{ restart_required: string[] }>('/config/restart-pending').subscribe({
      next: (r) => this.restartPending.set(r?.restart_required ?? []),
      error: () => this.restartPending.set([]),
    });
  }

  /** PUT part of the general settings; the server checks paths and reports what waits for a restart. */
  private saveGeneral(key: FormKey, body: Record<string, unknown>, saved: string, fallback: string): void {
    if (!this.canSave(key)) return;
    const sent = this.formSnapshot(key);
    this.api.put<GeneralUpdateResponse>('/config/general', body).subscribe({
      next: (r) => {
        this.baselines[key] = sent;
        this.restartPending.set(r?.restart_required ?? []);
        this.toast.success(saved);
      },
      error: (err) => showHttpError(this.toast, err, fallback),
    });
  }

  savePaths(): void {
    const f = this.pathsForm;
    const complete = f.complete_dir.trim();
    const incomplete = f.incomplete_dir.trim();
    if (!complete || !incomplete) {
      this.toast.info('The complete and incomplete directories are required');
      return;
    }
    this.saveGeneral(
      'paths',
      { complete_dir: complete, incomplete_dir: incomplete, watch_dir: f.watch_dir.trim() },
      'Paths saved. New jobs use them; existing jobs keep their paths.',
      'Failed to save paths',
    );
  }

  savePostproc(): void {
    const f = this.postprocForm;
    this.saveGeneral(
      'postproc',
      {
        direct_unpack: f.direct_unpack,
        max_nested_archive_depth: Number(f.max_nested_archive_depth),
        max_post_processing_jobs: Number(f.max_post_processing_jobs),
        max_repair_workers: Number(f.max_repair_workers),
        max_extract_workers: Number(f.max_extract_workers),
        scripts_dir: f.scripts_dir.trim(),
        script_success: f.script_success.trim(),
        script_failure: f.script_failure.trim(),
        script_timeout_secs: Number(f.script_timeout_secs),
        script_max_output_bytes: Number(f.script_max_output_bytes),
        par2_defer_volumes: f.par2_defer_volumes,
        par2_trust_inflight_verify: f.par2_trust_inflight_verify,
      },
      'Post-processing settings saved',
      'Failed to save post-processing settings',
    );
  }

  saveNetwork(): void {
    const port = Number(this.networkForm.port);
    if (!Number.isInteger(port) || port < 1 || port > 65535) {
      this.toast.info('Port must be between 1 and 65535');
      return;
    }
    this.saveGeneral(
      'network',
      { listen_addr: this.networkForm.listen_addr.trim(), port },
      'Network settings saved. They apply after a restart.',
      'Failed to save network settings',
    );
  }

  // ======================== SERVERS ========================

  loadServers(): void {
    this.loadSetting<ServerConfig[]>('servers', '/config/servers', 'Failed to load news servers', (r) =>
      this.servers.set(r),
    );
    this.loadServerStats();
  }

  loadServerStats(): void {
    this.api.get<ServerStats[]>('/config/servers/stats').subscribe({
      next: (r) => {
        const map: Record<string, ServerStats> = {};
        for (const s of r) map[s.server_id] = s;
        this.serverStats.set(map);
      },
      error: (err) => showHttpError(this.toast, err, 'Failed to load server stats'),
    });
  }

  toggleStats(id: string): void {
    this.expandedStatsId = this.expandedStatsId === id ? null : id;
    if (this.expandedStatsId) this.loadServerStats();
  }

  fmtBytes(bytes: number): string {
    return formatBytes(bytes);
  }

  fmtAvail(ok: number, fail: number): string {
    const total = ok + fail;
    if (total === 0) return '— (no data)';
    const pct = Math.round((ok / total) * 100);
    return `${pct}% of ${this.fmtCount(total)} articles`;
  }

  fmtCount(n: number): string {
    if (n < 1000) return `${n}`;
    if (n < 1_000_000) return `${(n / 1000).toFixed(0)}K`;
    return `${(n / 1_000_000).toFixed(1)}M`;
  }

  addServer(): void {
    this.editingServer = emptyServer();
    this.editingServerId = null;
  }

  editServer(s: ServerConfig): void {
    this.editingServer = { ...s, password: '' };
    this.editingServerId = s.id;
  }

  cloneServer(s: ServerConfig): void {
    this.editingServer = { ...s, id: '', name: `${s.name} (copy)`, password: '' };
    this.editingServerId = null;
  }

  cancelServerEdit(): void {
    this.editingServer = null;
    this.editingServerId = null;
  }

  saveServer(): void {
    if (!this.editingServer || !this.canSave('servers')) return;
    const server = { ...this.editingServer };
    if (!server.host.trim()) {
      this.toast.info('Host is required');
      return;
    }
    if (!server.username) server.username = null;
    if (this.editingServerId) {
      server.password = server.password ? server.password : '********';
    } else if (!server.password) {
      server.password = null;
    }

    if (this.editingServerId) {
      this.api.put(`/config/servers/${this.editingServerId}`, server).subscribe({
        next: () => {
          this.toast.success('Server updated');
          this.cancelServerEdit();
          this.loadServers();
        },
        error: (err) => showHttpError(this.toast, err, 'Failed to update server'),
      });
    } else {
      server.id = '';
      this.api.post('/config/servers', server).subscribe({
        next: () => {
          this.toast.success('Server added');
          this.cancelServerEdit();
          this.loadServers();
        },
        error: (err) => showHttpError(this.toast, err, 'Failed to add server'),
      });
    }
  }

  testServer(id: string): void {
    this.api.post<{ success: boolean; message: string }>(`/config/servers/${id}/test`).subscribe({
      next: (r) => this.toast.info(r.message),
      error: (err) => showHttpError(this.toast, err, 'Test failed'),
    });
  }

  // Test the current (possibly unsaved) form values against the NNTP server.
  // Uses the inline test endpoint so users don't have to Save before verifying edits.
  testEditingServer(): void {
    if (!this.editingServer) return;
    const body = { ...this.editingServer };
    if (!body.username) body.username = null;
    if (this.editingServerId && !body.password) body.password = '********';
    else if (!body.password) body.password = null;
    this.toast.open('Testing…', '', { duration: 1500 });
    this.api
      .post<{ success: boolean; message: string }>(`/config/servers/test-config`, body)
      .subscribe({
        next: (r) => this.toast.info(r.message, 4000),
        error: (err) => showHttpError(this.toast, err, 'Test failed'),
      });
  }

  toggleServerEnabled(s: ServerConfig): void {
    const updated = { ...s, enabled: !s.enabled };
    if (!updated.username) updated.username = null;
    if (!updated.password) updated.password = '********';
    this.api.put(`/config/servers/${s.id}`, updated).subscribe({
      next: () => {
        this.loadServers();
        this.toast.success(updated.enabled ? 'Server enabled' : 'Server disabled');
      },
      error: (err) => showHttpError(this.toast, err, 'Failed to update server'),
    });
  }

  deleteServer(id: string): void {
    this.confirmSvc
      .confirm({
        title: 'Remove this server?',
        message: 'Downloads in progress on this server may be interrupted.',
        confirmLabel: 'Remove',
        danger: true,
      })
      .subscribe((ok) => {
        if (!ok) return;
        this.api.delete(`/config/servers/${id}`).subscribe({
          next: () => {
            this.loadServers();
            this.toast.success('Server removed');
          },
          error: (err) => showHttpError(this.toast, err, 'Failed to delete server'),
        });
      });
  }

  // ======================== CATEGORIES ========================

  loadCategories(): void {
    this.loadSetting<CategoryConfig[]>('categories', '/config/categories', 'Failed to load categories', (r) =>
      this.categories.set(r),
    );
  }

  addCategory(): void {
    this.editingCategory = emptyCategory();
    this.editingCategoryOriginalName = null;
    this.editingCategoryOriginalOutput = null;
    this.categoryCleanupText = '';
    this.categoryExtText = '';
  }

  editCategory(c: CategoryConfig): void {
    this.editingCategory = { ...c };
    this.editingCategoryOriginalName = c.name;
    this.editingCategoryOriginalOutput = c.output_dir || null;
    this.categoryCleanupText = (c.cleanup_patterns ?? []).join(', ');
    this.categoryExtText = (c.unwanted_extensions ?? []).join(', ');
  }

  /** True when the edited category's folder differs from the saved one, so the *arr warning shows. */
  categoryOutputChanged(): boolean {
    if (!this.editingCategory) return false;
    const next = this.editingCategory.output_dir?.trim() || null;
    return next !== this.editingCategoryOriginalOutput;
  }

  cancelCategoryEdit(): void {
    this.editingCategory = null;
    this.editingCategoryOriginalName = null;
  }

  saveCategory(): void {
    if (!this.editingCategory || !this.canSave('categories')) return;
    const cat = { ...this.editingCategory };
    cat.output_dir = cat.output_dir?.trim() || null;
    cat.cleanup_patterns = splitList(this.categoryCleanupText);
    cat.unwanted_extensions = splitList(this.categoryExtText);

    if (this.editingCategoryOriginalName) {
      const encoded = encodeURIComponent(this.editingCategoryOriginalName);
      this.api.put(`/config/categories/${encoded}`, cat).subscribe({
        next: () => {
          this.toast.success('Category updated');
          this.cancelCategoryEdit();
          this.loadCategories();
        },
        error: (err) => showHttpError(this.toast, err, 'Failed to update category'),
      });
    } else {
      this.api.post('/config/categories', cat).subscribe({
        next: () => {
          this.toast.success('Category added');
          this.cancelCategoryEdit();
          this.loadCategories();
        },
        error: (err) => showHttpError(this.toast, err, 'Failed to add category'),
      });
    }
  }

  deleteCategory(name: string): void {
    this.confirmSvc
      .confirm({
        title: `Delete category "${name}"?`,
        message: 'Jobs already assigned to this category keep their setting; new jobs can no longer use it.',
        confirmLabel: 'Delete',
        danger: true,
      })
      .subscribe((ok) => {
        if (!ok) return;
        const encoded = encodeURIComponent(name);
        this.api.delete(`/config/categories/${encoded}`).subscribe({
          next: () => {
            this.loadCategories();
            this.toast.success('Category removed');
          },
          error: (err) => showHttpError(this.toast, err, 'Failed to delete category'),
        });
      });
  }

  ppLabel(level: number): string {
    switch (level) {
      case 0:
        return 'None';
      case 1:
        return 'Repair';
      case 2:
        return 'Unpack';
      case 3:
        return 'Repair + Unpack';
      default:
        return 'Unknown';
    }
  }

  // ======================== GENERAL ========================

  async toggleNotifications(event: Event): Promise<void> {
    const input = event.target as HTMLInputElement;
    const on = await this.notifications.setEnabled(input.checked);
    if (input.checked && !on) {
      input.checked = false;
      this.toast.open(
        this.notifications.supported
          ? 'Notifications are blocked. Allow them for this site in the browser settings.'
          : 'This browser does not support notifications.',
        'OK',
        { duration: 6000 },
      );
    }
  }

  followSystemTheme(): void {
    this.theme.followSystem();
  }
  setTheme(theme: AppTheme): void {
    this.theme.set(theme);
  }

  loadGeneralSettings(): void {
    this.loadSetting<{ speed_limit_bps: number }>(
      'speedLimit',
      '/config/speed-limit',
      'Failed to load speed limit',
      (r) => (this.speedLimit = r.speed_limit_bps),
    );
    this.loadSetting<{ max_active_downloads: number }>(
      'maxActive',
      '/config/max-active-downloads',
      'Failed to load max downloads',
      (r) => (this.maxActiveDownloads = r.max_active_downloads),
    );
    this.loadSetting<{ retention: number | null }>(
      'historyRetention',
      '/config/history-retention',
      'Failed to load history retention',
      (r) => (this.historyRetention = r.retention),
    );
    this.loadDiskGuards();
    this.loadSetting<DebugLoggingState>('debugLogging', '/config/debug-logging', 'Failed to load debug logging', (r) =>
      this.applyDebugLogging(r),
    );
  }

  loadDiskGuards(): void {
    this.loadSetting<{ min_free_space_bytes: number; abort_hopeless: boolean }>(
      'diskGuards',
      '/config/disk-guards',
      'Failed to load disk guards',
      (r) => {
        this.minFreeSpaceGB = r.min_free_space_bytes / 1024 ** 3;
        this.abortHopeless = r.abort_hopeless;
      },
    );
  }

  loadSabApiKey(): void {
    this.loadSetting<SabApiKeyResponse>(
      'sabApiKey',
      '/config/sabnzbd-api-key',
      'Failed to load SABnzbd API key',
      (response) => (this.sabApiKey = response.api_key),
    );
  }

  rotateSabApiKey(): void {
    // Without a loaded key the button reads "Generate", and rotating then would replace a real key.
    if (!this.canSave('sabApiKey')) return;
    const action = this.sabApiKey ? 'Rotate' : 'Generate';
    this.confirmSvc
      .confirm({
        title: `${action} SABnzbd API key?`,
        message: this.sabApiKey
          ? 'The current key will stop working immediately. Update Sonarr, Radarr, Lidarr, and any other integrations with the new key.'
          : 'This creates the credential used by Sonarr, Radarr, Lidarr, and other SABnzbd-compatible clients.',
        confirmLabel: `${action} key`,
        danger: !!this.sabApiKey,
      })
      .subscribe((ok) => {
        if (!ok) return;
        this.api.post<SabApiKeyResponse>('/config/sabnzbd-api-key/rotate', {}).subscribe({
          next: (response) => {
            this.sabApiKey = response.api_key;
            this.showSabApiKey = true;
            this.toast.success(`SABnzbd API key ${action.toLowerCase()}d`, 2500);
          },
          error: (err) => showHttpError(this.toast, err, `Failed to ${action.toLowerCase()} SABnzbd API key`),
        });
      });
  }

  saveDiskGuards(): void {
    this.saveForm(
      'diskGuards',
      '/config/disk-guards',
      { min_free_space_bytes: Math.round(this.minFreeSpaceGB * 1024 ** 3), abort_hopeless: this.abortHopeless },
      'Disk guards saved',
      'Failed to save disk guards',
    );
  }

  saveSpeedLimit(): void {
    this.saveForm(
      'speedLimit',
      '/config/speed-limit',
      { speed_limit_bps: this.speedLimit },
      'Speed limit saved',
      'Failed to save speed limit',
    );
  }

  private applyDebugLogging(r: DebugLoggingState): void {
    this.debugLogging = r.enabled;
    this.debugLoggingOverridden = r.overridden;
    this.debugLoggingEffective = r.effective;
  }

  saveDebugLogging(): void {
    if (!this.canSave('debugLogging')) return;
    this.api
      .put<DebugLoggingState>('/config/debug-logging', { enabled: this.debugLogging })
      .subscribe({
        next: (r) => {
          this.applyDebugLogging(r);
          this.toast.success(this.debugLogging ? 'Debug logging on' : 'Debug logging off');
        },
        error: (err) => showHttpError(this.toast, err, 'Failed to change debug logging'),
      });
  }

  saveMaxActive(): void {
    this.saveForm(
      'maxActive',
      '/config/max-active-downloads',
      { max_active_downloads: this.maxActiveDownloads },
      'Max downloads saved',
      'Failed to save max downloads',
    );
  }

  saveRetention(): void {
    this.saveForm(
      'historyRetention',
      '/config/history-retention',
      { retention: this.historyRetention },
      'History retention saved',
      'Failed to save retention',
    );
  }

  // ======================== SCHEDULER ========================

  loadSchedule(): void {
    this.loadSetting<ScheduleState>('schedule', '/config/schedule', 'Failed to load the schedule', (state) => {
      const rules = Array.isArray(state.rules) ? state.rules : [];
      this.schedule = {
        ...this.schedule,
        ...state,
        rules: rules.map((rule) => ({
          ...rule,
          // A hand-edited config may spell days out in full; the checkboxes
          // work in the short form the API writes.
          days: [...new Set((rule.days ?? []).map((day) => shortDay(day)))].filter(Boolean),
        })),
      };
    });
  }

  /** Re-read the schedule, dropping any unsaved edits. */
  reloadSchedule(): void {
    this.loadSchedule();
  }

  scheduleActiveNow(): boolean {
    return this.schedule.enabled && this.schedule.active_rule_ids.length > 0;
  }

  activeScheduleRules(): ScheduleRule[] {
    return this.schedule.rules.filter((rule) => this.isRuleActive(rule));
  }

  isRuleActive(rule: ScheduleRule): boolean {
    return this.schedule.enabled && !!rule.id && this.schedule.active_rule_ids.includes(rule.id);
  }

  disabledServerNames(): string {
    const ids = this.schedule.disabled_servers;
    const names = ids.map((id) => {
      const server = this.servers().find((candidate) => candidate.id === id);
      return server ? server.name || server.host : id;
    });
    return names.join(', ');
  }

  actionLabel(action: ScheduleAction): string {
    switch (action) {
      case 'pause':
        return 'pause';
      case 'speed_limit':
        return 'speed cap';
      case 'disable_servers':
        return 'servers out';
    }
  }

  formatSpeedValue(bps: number | null): string {
    // `null` is "no scheduled cap" and `0` is an explicit unlimited window.
    // Both read as unlimited; rendering 0 through formatSpeed would say
    // "0 B/s", which looks like a cap that stops every download.
    return bps === null || bps <= 0 ? 'unlimited' : formatSpeed(bps);
  }

  addScheduleRule(): void {
    if (!this.canSave('schedule')) return;
    this.schedule.rules = [
      ...this.schedule.rules,
      {
        id: '',
        name: '',
        days: ['mon', 'tue', 'wed', 'thu', 'fri'],
        start: '01:00',
        end: '06:00',
        action: 'pause',
        speed_limit_bps: 0,
        servers: [],
      },
    ];
  }

  removeScheduleRule(index: number): void {
    this.schedule.rules = this.schedule.rules.filter((_, i) => i !== index);
  }

  toggleScheduleDay(rule: ScheduleRule, day: string, event: Event): void {
    const checked = (event.target as HTMLInputElement).checked;
    rule.days = checked
      ? [...rule.days, day]
      : rule.days.filter((candidate) => candidate !== day);
  }

  toggleScheduleServer(rule: ScheduleRule, id: string, event: Event): void {
    const checked = (event.target as HTMLInputElement).checked;
    rule.servers = checked
      ? [...rule.servers, id]
      : rule.servers.filter((candidate) => candidate !== id);
  }

  /** The first thing wrong with a draft rule, worded for the row. */
  ruleProblem(rule: ScheduleRule): string | null {
    if (!rule.days.length) return 'Pick at least one day.';
    if (!CLOCK_RE.test(rule.start.trim())) return 'Start must be HH:MM (00:00 to 23:59).';
    if (!CLOCK_RE.test(rule.end.trim())) return 'End must be HH:MM (00:00 to 24:00).';
    if (rule.start.trim() === rule.end.trim()) return 'Start and end must differ.';
    if (rule.action === 'disable_servers' && !rule.servers.length) {
      return 'Pick at least one server to take out of rotation.';
    }
    return null;
  }

  /** The first problem across every draft rule, or `null` when the form can be saved. */
  scheduleProblem(): string | null {
    for (let i = 0; i < this.schedule.rules.length; i++) {
      const problem = this.ruleProblem(this.schedule.rules[i]);
      if (problem) return `Rule ${i + 1}: ${problem}`;
    }
    return null;
  }

  saveSchedule(): void {
    if (!this.canSave('schedule')) return;
    const problem = this.scheduleProblem();
    if (problem) {
      this.toast.error(problem, 4000);
      return;
    }
    const sent = this.formSnapshot('schedule');
    const body = {
      enabled: this.schedule.enabled,
      rules: this.schedule.rules.map((rule) => ({
        ...rule,
        name: rule.name.trim(),
        days: [...rule.days],
        servers: [...rule.servers],
      })),
    };
    this.api.put<ScheduleState>('/config/schedule', body).subscribe({
      next: (state) => {
        // The response carries the ids the server assigned and what the
        // schedule is doing now. If the form was edited while the save was
        // in flight, keep that draft (dirty) instead of overwriting it; the
        // server assigns ids on the next save.
        if (this.formSnapshot('schedule') === sent) {
          const rules = Array.isArray(state.rules) ? state.rules : [];
          this.schedule = { ...state, rules: rules.map((rule) => ({ ...rule })) };
        }
        this.baselines.schedule = this.formSnapshot('schedule');
        this.toast.success('Schedule saved');
      },
      error: (err) => showHttpError(this.toast, err, 'Failed to save the schedule'),
    });
  }

  // ======================== DAV CONFIG ========================

  loadDavConfig(): void {
    this.loadSetting<DavConfig>('dav', '/config/dav', 'Failed to load Media Library settings', (cfg) => {
      this.davConfig = { ...cfg };
    });
  }

  onAutoSendAllChange(): void {
    if (this.davConfig.auto_send_all) {
      this.davConfig.category_rules = [];
    }
  }

  toggleDavCategory(name: string, event: Event): void {
    const checked = (event.target as HTMLInputElement).checked;
    if (checked) {
      if (!this.davConfig.category_rules.includes(name)) {
        this.davConfig.category_rules = [...this.davConfig.category_rules, name];
      }
    } else {
      this.davConfig.category_rules = this.davConfig.category_rules.filter((r) => r !== name);
    }
  }

  saveDavConfig(): void {
    if (!this.canSave('dav')) return;
    // Send empty strings as null so the backend treats blanks as "unset".
    const payload: DavConfig = {
      ...this.davConfig,
      username: this.davConfig.username?.trim() || null,
      password: this.davConfig.password?.trim() || null,
      api_key: this.davConfig.api_key?.trim() || null,
    };
    const restartRequired = payload.enabled !== this.davRuntimeEnabled();
    const sent = this.formSnapshot('dav');
    this.api.put('/config/dav', payload).subscribe({
      next: () => {
        this.baselines.dav = sent;
        this.toast.success(
          restartRequired
            ? 'Media Library settings saved. Restart required.'
            : 'Media Library settings saved',
          2500,
        );
      },
      error: (err) => showHttpError(this.toast, err, 'Failed to save Media Library settings'),
    });
  }

  davBaseUrl(): string {
    // Use the browser origin so the URL also works behind a reverse proxy.
    return `${window.location.origin}/dav`;
  }

  davRestartRequired(): boolean {
    return this.webdavAvailable() && this.davConfig.enabled !== this.davRuntimeEnabled();
  }

  davAuthConfigured(): boolean {
    const c = this.davConfig;
    return !!(c.username?.trim() || c.password?.trim() || c.api_key?.trim());
  }

  generateDavApiKey(): void {
    const regenerating = !!this.davConfig.api_key?.trim();
    const doGenerate = () => {
      const bytes = new Uint8Array(32);
      crypto.getRandomValues(bytes);
      this.davConfig.api_key = Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
      this.showDavApiKey = true;
    };
    if (!regenerating) {
      doGenerate();
      return;
    }
    this.confirmSvc
      .confirm({
        title: 'Generate a new API key?',
        message:
          'This replaces the current key once you save. Any WebDAV client using the old key will stop working until you update it.',
        confirmLabel: 'Generate new key',
        danger: true,
      })
      .subscribe((ok) => {
        if (ok) doGenerate();
      });
  }

  // -------------------------------------------------------------------------
  // Backup & restore
  // -------------------------------------------------------------------------

  /** A preview only permits the Restore button while it describes this exact file. */
  get backupPreviewMatchesFile(): boolean {
    return !!this.backupPreview && this.backupPreviewText === this.backupText;
  }

  async downloadBackup(): Promise<void> {
    if (this.backupBusy) return;
    this.backupBusy = true;
    this.backupError = '';
    const includeSecrets = this.backupIncludeSecrets;
    this.api
      .getText(
        '/config/backup',
        includeSecrets ? { secrets: 'true' } : undefined,
      )
      .subscribe({
        next: (response) => {
          this.backupBusy = false;
          const body = response.body ?? '';
          const filename =
            filenameFromDisposition(response.headers.get('Content-Disposition')) ||
            `rustnzb-backup-${new Date().toISOString().slice(0, 10)}.toml`;
          saveText(filename, body);
          this.toast.info(
            includeSecrets ? 'Backup downloaded — it contains secrets' : 'Backup downloaded',
          );
        },
        error: (err) => {
          this.backupBusy = false;
          showHttpError(this.toast, err, 'Failed to download backup');
        },
      });
  }

  /** Read the chosen file once; a new file invalidates any preview of the old one. */
  chooseBackupFile(event: Event): void {
    const input = event.target as HTMLInputElement;
    const file = input.files?.[0];
    this.backupPreview = null;
    this.backupPreviewText = '';
    this.backupError = '';
    this.backupText = '';
    this.backupFileName = file?.name ?? '';
    if (!file) return;
    if (file.size > MAX_BACKUP_BYTES) {
      this.backupError = `That file is larger than ${MAX_BACKUP_BYTES / (1024 * 1024)} MiB; it is not a rustnzb backup.`;
      return;
    }
    const reader = new FileReader();
    reader.onerror = () => {
      this.backupError = 'Could not read that file.';
    };
    reader.onload = () => {
      const text = typeof reader.result === 'string' ? reader.result : '';
      if (!text.trim()) {
        this.backupError = 'That file is empty.';
        return;
      }
      this.backupText = text;
    };
    reader.readAsText(file);
  }

  previewBackup(): void {
    if (this.backupBusy || !this.backupText) return;
    const text = this.backupText;
    this.backupBusy = true;
    this.backupError = '';
    this.api.postText<BackupReport>('/config/backup/preview', text).subscribe({
      next: (report) => {
        this.backupBusy = false;
        this.backupPreview = report;
        this.backupPreviewText = text;
      },
      error: (err) => {
        this.backupBusy = false;
        this.backupPreview = null;
        this.backupPreviewText = '';
        this.backupError = httpErrorMessage(err, 'This backup was refused');
        showHttpError(this.toast, err, 'This backup was refused');
      },
    });
  }

  restoreBackup(): void {
    if (this.backupBusy || !this.backupPreviewMatchesFile) return;
    const report = this.backupPreview!;
    const text = this.backupText;
    const consequences = [
      'Every saved setting is replaced; anything missing from the file returns to its default.',
      report.summary.warnings.length
        ? `${report.summary.warnings.length} masked secret(s) have nothing to keep on this install and will be left unset.`
        : '',
      report.restart_required.length
        ? `A restart is needed for: ${report.restart_required.join(', ')}.`
        : '',
    ]
      .filter(Boolean)
      .join(' ');

    this.confirmSvc
      .confirm({
        title: 'Restore this configuration?',
        message: consequences,
        confirmLabel: report.backup.secrets_included
          ? 'Restore, including secrets'
          : 'Restore settings',
        danger: true,
      })
      .subscribe((ok) => {
        if (!ok) return;
        this.backupBusy = true;
        this.backupError = '';
        this.api.postText<BackupReport>('/config/backup/restore', text).subscribe({
          next: (restored) => {
            this.backupBusy = false;
            this.backupPreview = restored;
            this.backupPreviewText = text;
            this.reloadAfterRestore();
            if (restored.restart_required.length) {
              this.toast.info(
                'Configuration restored. Restart to apply: ' +
                  restored.restart_required.join(', '),
                6000,
              );
            } else {
              this.toast.success('Configuration restored');
            }
          },
          error: (err) => {
            this.backupBusy = false;
            this.backupError = httpErrorMessage(err, 'Restore failed');
            showHttpError(this.toast, err, 'Restore failed');
          },
        });
      });
  }

  /** Refetch every form from the settings the restore just wrote. */
  private reloadAfterRestore(): void {
    this.loadServers();
    this.loadCategories();
    this.loadGeneralSettings();
    this.loadSabApiKey();
    this.loadDirs();
  }

  copy(text: string): void {
    navigator.clipboard.writeText(text).then(
      () => this.toast.success('Copied', 1500),
      () => this.toast.error('Copy failed'),
    );
  }
}
