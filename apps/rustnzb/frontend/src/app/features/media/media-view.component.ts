import { Component, OnInit, OnDestroy, signal, inject } from '@angular/core';
import { CommonModule } from '@angular/common';
import { HttpClient, HttpHeaders } from '@angular/common/http';
import { MatSnackBar, MatSnackBarModule } from '@angular/material/snack-bar';
import { ApiService } from '../../core/services/api.service';
import { IconComponent } from '../../shared/icon.component';

interface DavFile {
  href: string; // path from WebDAV root, e.g. /content/Release/file.mkv
  name: string;
  isDir: boolean;
  size: number;
  contentType: string;
}

interface DavQueueEntry {
  job_name: string;
  queued_at: string;
}

interface DavHistoryEntry {
  job_name: string;
  status: 'completed' | 'failed';
  fail_message: string | null;
  completed_at: string;
}

interface DavPipelineStatus {
  queue: DavQueueEntry[];
  history: DavHistoryEntry[];
}

interface StatusResponse {
  webdav_enabled: boolean;
}

// The mount point of the WebDAV router on the server.
const DAV_MOUNT = '/dav';

/**
 * Turn a PROPFIND `<D:href>` into a decoded path relative to the DAV root.
 *
 * The server emits hrefs that include the `/dav` mount prefix and are
 * percent-encoded (`/dav/content/My%20Show/`). Root-relative hrefs without the
 * prefix (`/content/...`) are accepted too. Absolute URLs are reduced to their
 * path first.
 */
function normalizeDavHref(raw: string): string {
  let path = raw.trim();
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(path)) {
    try {
      path = new URL(path).pathname;
    } catch {
      /* keep as-is */
    }
  }
  try {
    path = decodeURIComponent(path);
  } catch {
    /* malformed escape: keep the raw text */
  }
  if (path === DAV_MOUNT || path.startsWith(DAV_MOUNT + '/')) {
    path = path.slice(DAV_MOUNT.length) || '/';
  }
  return path.startsWith('/') ? path : '/' + path;
}

/** Compare two DAV-relative paths ignoring a trailing slash. */
function samePath(a: string, b: string): boolean {
  const trim = (p: string) => (p.length > 1 ? p.replace(/\/+$/, '') : p);
  return trim(a) === trim(b);
}

interface Release {
  href: string;
  name: string;
  files: DavFile[];
  expanded: boolean;
  loading: boolean;
  /** Set when the pipeline reports a failure for this job */
  failMessage: string | null;
  /** True while the item is in the DAV pipeline queue */
  queued: boolean;
}

const VIDEO_EXTS = new Set([
  'mkv',
  'mp4',
  'avi',
  'mov',
  'wmv',
  'ts',
  'm4v',
  'webm',
  'flv',
  'mpg',
  'mpeg',
]);
const AUDIO_EXTS = new Set(['mp3', 'flac', 'aac', 'ogg', 'wav', 'm4a', 'opus']);

@Component({
  selector: 'app-media-view',
  standalone: true,
  imports: [CommonModule, MatSnackBarModule, IconComponent],
  template: `
    <!-- Header bar -->
    <div class="panel">
      <h3>
        Media Library
        <span class="hint"
          >WebDAV endpoint: <code>{{ davBase }}</code></span
        >
        <button class="btn ghost sm" style="margin-left:8px" (click)="copyBase()">Copy URL</button>
        <button class="btn ghost sm" style="margin-left:4px" (click)="loadContent()">
          <app-icon name="retry" [size]="11" /> Refresh
        </button>
      </h3>
    </div>

    @if (!enabled()) {
      <div class="panel">
        <div class="body">
          <div class="empty-cell">
            Media Library is disabled. Enable it in <strong>Settings → Media Library (DAV)</strong>
            and restart rustnzb.
          </div>
        </div>
      </div>
    } @else {
      <!-- Content browser -->
      <div class="panel">
        <h3>
          Content
          <span class="hint"
            >{{ releases().length }} release{{ releases().length === 1 ? '' : 's' }}</span
          >
        </h3>
        <div class="body flush">
          @if (loading()) {
            <div class="empty-cell">Loading…</div>
          } @else if (releases().length === 0) {
            <div class="empty-cell">
              No content yet. Send items from History using the <strong>▶ media</strong> button,
              then wait a few seconds for the pipeline to process the NZB.
            </div>
          } @else {
            <table class="data content-table">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Type</th>
                  <th style="text-align:right">Size</th>
                  <th style="width:160px"></th>
                </tr>
              </thead>
              <tbody>
                @for (rel of releases(); track rel.href) {
                  <!-- Release directory row -->
                  <tr
                    class="rel-row"
                    [class.rel-failed]="rel.failMessage"
                    (click)="!rel.failMessage && toggle(rel)"
                  >
                    <td>
                      @if (!rel.failMessage) {
                        <span class="toggle-icon">
                          <app-icon [name]="rel.expanded ? 'chevron-down' : 'chevron-right'" [size]="12" />
                        </span>
                      }
                      <span class="rel-name">{{ rel.name }}</span>
                      @if (rel.loading) {
                        <span class="loading-dot">…</span>
                      }
                      @if (rel.queued) {
                        <span class="status-chip queued">processing</span>
                      }
                      @if (rel.failMessage) {
                        <span class="status-chip failed" [title]="rel.failMessage">failed</span>
                      }
                    </td>
                    <td>
                      @if (!rel.failMessage) {
                        <span class="badge dir">folder</span>
                      }
                    </td>
                    <td></td>
                    <td></td>
                  </tr>

                  <!-- Failure detail row -->
                  @if (rel.failMessage) {
                    <tr class="fail-row">
                      <td colspan="4" class="fail-msg">{{ rel.failMessage }}</td>
                    </tr>
                  }

                  <!-- File rows when expanded -->
                  @if (rel.expanded && !rel.failMessage) {
                    @for (f of rel.files; track f.href) {
                      <tr class="file-row" [class.video]="isVideo(f)" [class.audio]="isAudio(f)">
                        <td>
                          <span class="file-indent"></span>
                          <span class="file-name" [title]="f.name">{{ f.name }}</span>
                        </td>
                        <td>
                          @if (isVideo(f)) {
                            <span class="badge video">video</span>
                          } @else if (isAudio(f)) {
                            <span class="badge audio">audio</span>
                          } @else {
                            <span class="badge other">{{ ext(f.name) }}</span>
                          }
                        </td>
                        <td class="size-cell">{{ formatBytes(f.size) }}</td>
                        <td class="actions">
                          @if (isVideo(f) || isAudio(f)) {
                            <a
                              class="row-action play"
                              [href]="fileUrl(f.href)"
                              target="_blank"
                              title="Open in browser / media player"
                              ><app-icon name="play" [size]="11" /> play</a
                            >
                          }
                          <button
                            class="row-action"
                            (click)="copyFileUrl(f.href); $event.stopPropagation()"
                            title="Copy stream URL"
                          >
                            copy URL
                          </button>
                          <a
                            class="row-action"
                            [href]="fileUrl(f.href)"
                            [download]="f.name"
                            title="Download"
                            >↓</a
                          >
                        </td>
                      </tr>
                    }
                    @if (rel.files.length === 0 && !rel.loading) {
                      <tr class="file-row">
                        <td colspan="4" class="empty-sub">Still processing…</td>
                      </tr>
                    }
                  }
                }
              </tbody>
            </table>
          }
        </div>
      </div>

      <!-- Connect instructions -->
      <div class="panel">
        <h3>Connect a media client</h3>
        <div class="body">
          <p class="hint-block">
            Point any WebDAV client at <code>{{ davBase }}</code> to browse and stream directly. Use
            your rustnzb username and password. The <code>/content/</code> collection contains your
            releases.
          </p>
          <table class="data clients">
            <thead>
              <tr>
                <th>App</th>
                <th>Platform</th>
                <th>How to add</th>
              </tr>
            </thead>
            <tbody>
              <tr>
                <td><strong>Infuse</strong></td>
                <td>iOS · tvOS · macOS</td>
                <td>Settings → Add Files → WebDAV → enter URL + credentials</td>
              </tr>
              <tr>
                <td><strong>VLC</strong></td>
                <td>All</td>
                <td>Network → Open Network Stream → paste file URL, or Media → Open Network</td>
              </tr>
              <tr>
                <td><strong>Kodi</strong></td>
                <td>All</td>
                <td>Files → Add source → Browse → Add Network Location → WebDAV</td>
              </tr>
              <tr>
                <td><strong>nPlayer</strong></td>
                <td>iOS · tvOS</td>
                <td>+ → WebDAV → enter server URL + credentials</td>
              </tr>
              <tr>
                <td><strong>mpv / vlc CLI</strong></td>
                <td>Desktop</td>
                <td><code>mpv http://host/dav/content/Release/file.mkv</code></td>
              </tr>
            </tbody>
          </table>
        </div>
      </div>
    }
  `,
  styles: [
    `
      :host {
        display: block;
      }

      .empty-cell {
        text-align: center;
        padding: 36px 20px;
        color: var(--mute);
        font-size: 13px;
      }
      .empty-sub {
        color: var(--mute);
        font-size: 12px;
        padding-left: 32px !important;
      }

      code {
        background: var(--panel2);
        border: 1px solid var(--line);
        border-radius: 3px;
        padding: 1px 5px;
        font-size: 12px;
        color: var(--accent);
      }

      table.content-table {
        table-layout: fixed;
      }
      table.content-table th:first-child,
      table.content-table td:first-child {
        width: 50%;
        overflow: hidden;
        text-overflow: ellipsis;
      }

      .rel-row {
        cursor: pointer;
      }
      .rel-row:hover td {
        background: var(--panel2);
      }
      .toggle-icon {
        color: var(--mute);
        margin-right: 6px;
        font-size: 12px;
        user-select: none;
      }
      .rel-name {
        font-size: 13px;
        font-weight: 500;
      }
      .loading-dot {
        color: var(--mute);
        font-size: 12px;
        margin-left: 6px;
      }

      .file-row td {
        background: rgba(0, 0, 0, 0.15);
      }
      .file-indent {
        display: inline-block;
        width: 24px;
      }
      .file-name {
        font-size: 12px;
        color: var(--mute);
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
        display: inline-block;
        max-width: calc(100% - 28px);
        vertical-align: middle;
      }
      .file-row.video .file-name {
        color: var(--text);
      }
      .file-row.audio .file-name {
        color: var(--text);
      }

      .size-cell {
        text-align: right;
        color: var(--mute);
        font-size: 12px;
      }
      .actions {
        white-space: nowrap;
      }
      .actions a {
        text-decoration: none;
      }

      .badge {
        font-size: 10px;
        padding: 2px 6px;
        border-radius: 3px;
        font-weight: 600;
        letter-spacing: 0.3px;
        text-transform: uppercase;
      }
      .badge.dir {
        background: rgba(59, 130, 246, 0.15);
        color: var(--accent);
      }
      .badge.video {
        background: rgba(167, 139, 250, 0.15);
        color: var(--purple);
      }
      .badge.audio {
        background: rgba(16, 185, 129, 0.15);
        color: var(--accent2);
      }
      .badge.other {
        background: var(--panel2);
        color: var(--mute);
      }

      .row-action.play {
        color: var(--purple);
        border-color: var(--purple);
      }

      .rel-failed {
        opacity: 0.7;
        cursor: default;
      }
      .status-chip {
        font-size: 10px;
        padding: 2px 6px;
        border-radius: 3px;
        font-weight: 600;
        letter-spacing: 0.3px;
        text-transform: uppercase;
        margin-left: 8px;
      }
      .status-chip.queued {
        background: rgba(59, 130, 246, 0.15);
        color: var(--accent);
      }
      .status-chip.failed {
        background: rgba(239, 68, 68, 0.15);
        color: var(--danger);
      }
      .fail-row td {
        background: rgba(239, 68, 68, 0.05);
      }
      .fail-msg {
        color: var(--danger);
        font-size: 11px;
        padding: 4px 12px 6px 32px !important;
        opacity: 0.85;
      }

      .hint-block {
        color: var(--mute);
        font-size: 12px;
        margin: 0 0 12px;
        line-height: 1.6;
      }
      table.clients td {
        font-size: 13px;
      }
      table.clients td:last-child {
        color: var(--mute);
        font-size: 12px;
      }
      table.clients code {
        font-size: 11px;
      }
    `,
  ],
})
export class MediaViewComponent implements OnInit, OnDestroy {
  private http = inject(HttpClient);
  private api = inject(ApiService);
  private snack = inject(MatSnackBar);

  davBase = `${window.location.origin}/dav`;
  enabled = signal(true);
  releases = signal<Release[]>([]);
  loading = signal(false);

  private pollTimer: ReturnType<typeof setInterval> | null = null;
  private pipelineStatus: DavPipelineStatus = { queue: [], history: [] };

  ngOnInit(): void {
    this.api.get<StatusResponse>('/status').subscribe({
      next: (status) => {
        this.enabled.set(status.webdav_enabled);
        if (!status.webdav_enabled) {
          this.loading.set(false);
          return;
        }
        this.loadContent();
        this.pollTimer = setInterval(() => this.loadContent(), 10000);
      },
      error: () => {
        this.enabled.set(false);
        this.loading.set(false);
      },
    });
  }

  ngOnDestroy(): void {
    if (this.pollTimer) clearInterval(this.pollTimer);
  }

  loadContent(): void {
    if (!this.enabled()) {
      this.loading.set(false);
      return;
    }
    this.loading.set(this.releases().length === 0);
    Promise.all([
      this.propfind('/content', 1),
      this.api
        .get<DavPipelineStatus>('/dav/status')
        .toPromise()
        .catch(() => null),
    ])
      .then(([items, status]) => {
        if (status) this.pipelineStatus = status;
        const existing = this.releases();
        const dirs = items.filter((i) => i.isDir && !samePath(i.href, '/content'));
        const updated = dirs.map((d) => {
          const prev = existing.find((r) => r.href === d.href);
          const base = prev ?? {
            href: d.href,
            name: d.name,
            files: [],
            expanded: false,
            loading: false,
            failMessage: null,
            queued: false,
          };
          base.failMessage = this.lookupFailMessage(d.name);
          base.queued = this.isQueued(d.name);
          return base;
        });
        for (const rel of updated) {
          if (rel.expanded && rel.files.length === 0 && !rel.failMessage) this.loadFiles(rel);
        }
        this.releases.set(updated);
        this.loading.set(false);
      })
      .catch(() => this.loading.set(false));
  }

  private lookupFailMessage(name: string): string | null {
    const h = this.pipelineStatus.history.find((e) => e.status === 'failed' && e.job_name === name);
    return h?.fail_message ?? null;
  }

  private isQueued(name: string): boolean {
    return this.pipelineStatus.queue.some((q) => q.job_name === name);
  }

  toggle(rel: Release): void {
    if (rel.failMessage) return;
    rel.expanded = !rel.expanded;
    if (rel.expanded && rel.files.length === 0 && !rel.loading) {
      this.loadFiles(rel);
    }
    this.releases.set([...this.releases()]);
  }

  private loadFiles(rel: Release): void {
    rel.loading = true;
    this.propfind(rel.href, 1)
      .then((items) => {
        rel.files = items.filter((i) => !i.isDir && !samePath(i.href, rel.href));
        rel.loading = false;
        this.releases.set([...this.releases()]);
      })
      .catch(() => {
        rel.loading = false;
        this.releases.set([...this.releases()]);
      });
  }

  // davRelPath: decoded WebDAV-relative path (no /dav prefix), e.g. '/content' or '/content/Release/'
  private propfind(davRelPath: string, depth: number): Promise<DavFile[]> {
    const url = this.fileUrl(davRelPath);
    const headers = new HttpHeaders({ Depth: String(depth) });
    return this.http
      .request('PROPFIND', url, {
        headers,
        responseType: 'text',
      })
      .toPromise()
      .then((xml) => this.parseMultiStatus(xml ?? ''));
  }

  private parseMultiStatus(xml: string): DavFile[] {
    const doc = new DOMParser().parseFromString(xml, 'application/xml');
    const ns = 'DAV:';
    const items: DavFile[] = [];
    for (const resp of Array.from(doc.getElementsByTagNameNS(ns, 'response'))) {
      const rawHref = resp.getElementsByTagNameNS(ns, 'href')[0]?.textContent ?? '';
      const href = normalizeDavHref(rawHref);
      const displayName = resp.getElementsByTagNameNS(ns, 'displayname')[0]?.textContent ?? '';
      const contentType = resp.getElementsByTagNameNS(ns, 'getcontenttype')[0]?.textContent ?? '';
      const sizeStr = resp.getElementsByTagNameNS(ns, 'getcontentlength')[0]?.textContent ?? '0';
      const rtype = resp.getElementsByTagNameNS(ns, 'resourcetype')[0];
      const isDir = !!rtype?.getElementsByTagNameNS(ns, 'collection').length;
      const name = displayName || href.split('/').filter(Boolean).pop() || href;
      items.push({
        href,
        name,
        isDir,
        size: parseInt(sizeStr) || 0,
        contentType,
      });
    }
    return items;
  }

  // href: decoded WebDAV-relative path (as produced by normalizeDavHref).
  fileUrl(href: string): string {
    const encoded = href.split('/').map(encodeURIComponent).join('/');
    return `${window.location.origin}/dav${encoded}`;
  }

  isVideo(f: DavFile): boolean {
    return VIDEO_EXTS.has(this.ext(f.name));
  }

  isAudio(f: DavFile): boolean {
    return AUDIO_EXTS.has(this.ext(f.name));
  }

  ext(name: string): string {
    return (name.split('.').pop() ?? '').toLowerCase();
  }

  formatBytes(b: number): string {
    if (!b) return '';
    const k = 1024;
    const s = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.min(4, Math.floor(Math.log(b) / Math.log(k)));
    return (b / Math.pow(k, i)).toFixed(1) + ' ' + s[i];
  }

  copyBase(): void {
    navigator.clipboard.writeText(this.davBase).then(
      () => this.snack.open('URL copied', 'Close', { duration: 2000 }),
      () => this.snack.open(this.davBase, 'Close', { duration: 4000 }),
    );
  }

  copyFileUrl(href: string): void {
    const url = this.fileUrl(href);
    navigator.clipboard.writeText(url).then(
      () => this.snack.open('Stream URL copied', 'Close', { duration: 2000 }),
      () => this.snack.open(url, 'Close', { duration: 4000 }),
    );
  }
}
