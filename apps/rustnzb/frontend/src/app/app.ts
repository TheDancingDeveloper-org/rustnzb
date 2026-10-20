import {
  Component,
  ElementRef,
  HostListener,
  OnInit,
  OnDestroy,
  Signal,
  ViewChild,
  computed,
  signal,
  WritableSignal,
} from '@angular/core';
import { CommonModule } from '@angular/common';
import { FormsModule } from '@angular/forms';
import { NavigationEnd, Router, RouterModule } from '@angular/router';
import { filter, timeout } from 'rxjs';
import { ApiService, POLL_TIMEOUT_MS, pageHidden } from './core/services/api.service';
import { AuthService } from './core/services/auth.service';
import { StatusResponse } from './core/models/queue.model';
import { formatBytes, formatSpeed } from './core/format';
import {
  AddNzbPrefill,
  AddNzbService,
  isEditableTarget,
  nzbUploadFiles,
  pastedNzbUrl,
} from './core/services/add-nzb.service';
import { WidthModeService } from './core/services/width-mode.service';
import { PauseStateService } from './core/services/pause-state.service';
import { BrowserNotificationsService } from './core/services/browser-notifications.service';
import { ThemeService } from './core/services/theme.service';
import { AnnouncerService } from './core/services/announcer.service';
import {
  CHORD_TIMEOUT_MS,
  KeyboardShortcutsService,
  resolveShortcut,
  startsChord,
} from './core/services/keyboard-shortcuts.service';
import { IconComponent } from './shared/icon.component';
import { MatDialog } from '@angular/material/dialog';
import { showHttpError } from './core/http/http-error';
import { ToastService } from './core/services/toast.service';
import { ShortcutsDialogComponent } from './shared/shortcuts-dialog.component';

export function isDemoPath(pathname: string): boolean {
  return pathname === '/demo' || pathname.startsWith('/demo/');
}

// Pages that render full-screen, without the app chrome, even when signed in.
export function isBareRoute(url: string): boolean {
  const path = url.split(/[?#]/)[0];
  return path === '/login' || path === '/welcome';
}

const MIB = 1024 * 1024;

// Width of the header menus. The CSS sets the same width (border-box), so the
// viewport clamp in anchorMenu() matches what is actually rendered.
export const MENU_WIDTH = 240;
const VIEWPORT_GUTTER = 8;

// Browser-tab name for each page. The legacy /queue and /history paths keep the
// tab names they had, so a bookmarked tab still reads the same.
const PAGE_TITLES: Record<string, string> = {
  '/downloads': 'Downloads',
  '/queue': 'Downloads',
  '/history': 'History',
  '/groups': 'Search',
  '/rss': 'RSS',
  '/media': 'Media',
  '/logs': 'Logs',
  '/statistics': 'Statistics',
  '/settings': 'Settings',
  '/welcome': 'Welcome',
  '/login': 'Sign in',
};

export interface LiveTitleState {
  paused: boolean;
  speedBps: number;
  queueSize: number;
}

/**
 * Title for the browser tab: live transfer state first, then the page name.
 * Pass `null` for live state when it is unknown or stale (offline, not signed in).
 * The Downloads page is left out of the suffix because it is the home page.
 */
export function documentTitle(url: string, live: LiveTitleState | null): string {
  const path = url.split(/[?#]/)[0];
  const parts: string[] = [];
  if (live) {
    if (live.paused) parts.push('Paused');
    else if (live.speedBps > 0) parts.push(`↓ ${formatSpeed(live.speedBps)}`);
    if (live.queueSize > 0) parts.push(`${live.queueSize} left`);
  }
  const page = PAGE_TITLES[path];
  if (page && page !== 'Downloads') parts.push(page);
  return parts.length ? `${parts.join(' · ')} – rustnzb` : 'rustnzb';
}

@Component({
  selector: 'app-root',
  standalone: true,
  imports: [CommonModule, FormsModule, RouterModule, IconComponent],
  template: `
    @if (!showChrome()) {
      <!-- Full-screen login / welcome (no chrome) -->
      <router-outlet />
    } @else {
      <div class="shell">
        <nav class="topbar">
          <div class="wrap">
            <span class="brand">rust<span>nzb</span></span>
            <span class="ver">v{{ version() }}</span>
            <div class="sep"></div>
            <a routerLink="/downloads" routerLinkActive="active">Downloads</a>
            <a routerLink="/groups" routerLinkActive="active">Search</a>
            <a routerLink="/rss" routerLinkActive="active">RSS</a>
            @if (webdavEnabled()) {
              <a routerLink="/media" routerLinkActive="active">Media</a>
            }
            <a routerLink="/logs" routerLinkActive="active">Logs</a>
            <a routerLink="/statistics" routerLinkActive="active">Statistics</a>
            <a routerLink="/settings" routerLinkActive="active">Settings</a>
            <div class="spacer"></div>
            @if (demoMode) {
              <a class="action demo-exit" href="/">Exit demo</a>
            }
            <div class="status" [class.stale]="offline()" [attr.title]="offline() ? 'Stale: showing the last known values' : null">
              @if (offline()) {
                <span class="pill err" role="status">● Offline</span>
              } @else if (!loaded()) {
                <span class="pill" role="status">—</span>
              } @else {
                <span class="pill" [class.ok]="!paused()" [class.warn]="paused()" role="status">
                  ● {{ paused() ? 'Paused' : 'Live' }}
                </span>
              }
              <div class="speed-group" (keydown.escape)="closeSpeedMenu()">
                <button
                  #speedBtn
                  type="button"
                  class="pill pill-btn"
                  (click)="toggleSpeedMenu(speedBtn)"
                  title="Set speed limit"
                  aria-haspopup="true"
                  [attr.aria-expanded]="speedMenuOpen"
                >
                  {{ loaded() ? formatSpeed(speed()) : '—' }}
                </button>
                @if (speedMenuOpen) {
                  <div
                    class="speed-menu"
                    role="menu"
                    [style.top.px]="menuAnchor.top"
                    [style.left.px]="menuAnchor.left"
                    (click)="$event.stopPropagation()"
                  >
                    <div class="pm-title">Speed limit</div>
                    <div class="pm-current">Current: {{ speedLimit() > 0 ? formatSpeed(speedLimit()) : 'off' }}</div>
                    @for (opt of speedPresets; track opt.bps) {
                      <button
                        class="pm-item"
                        [class.current]="speedLimit() === opt.bps"
                        role="menuitem"
                        (click)="setSpeedLimit(opt.bps)"
                      >
                        {{ opt.label }}
                      </button>
                    }
                    <div class="pm-custom">
                      <input
                        type="number"
                        min="0.1"
                        step="any"
                        placeholder="MB/s"
                        aria-label="Custom speed limit in MB/s"
                        [(ngModel)]="customSpeedMb"
                        (keydown.enter)="setCustomSpeedLimit()"
                      />
                      <button class="pm-go" (click)="setCustomSpeedLimit()">Set</button>
                    </div>
                  </div>
                }
              </div>
              <span class="pill">{{ loaded() ? queueCount() : '—' }} queued</span>
              <span class="pill">{{ loaded() ? formatBytes(diskFree()) : '—' }} free</span>
            </div>
            <div class="mode-toggle" role="group" aria-label="Layout width">
              <button
                type="button"
                [class.active]="widthMode.mode() === 'compact'"
                (click)="widthMode.set('compact')"
                title="Compact"
                aria-label="Compact layout"
                [attr.aria-pressed]="widthMode.mode() === 'compact'"
              >
                <svg
                  viewBox="0 0 16 16"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.5"
                  aria-hidden="true"
                >
                  <rect x="3.5" y="2.5" width="9" height="11" rx="1" />
                </svg>
              </button>
              <button
                type="button"
                [class.active]="widthMode.mode() === 'expanded'"
                (click)="widthMode.set('expanded')"
                title="Expanded"
                aria-label="Expanded layout"
                [attr.aria-pressed]="widthMode.mode() === 'expanded'"
              >
                <svg
                  viewBox="0 0 16 16"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.5"
                  aria-hidden="true"
                >
                  <rect x="1.5" y="2.5" width="13" height="11" rx="1" />
                </svg>
              </button>
            </div>
            <button class="action primary" (click)="onAddNzb()">+ Upload NZB</button>
            <div class="pause-group" (keydown.escape)="closePauseMenu()">
              <button class="action" (click)="togglePause()">
                @if (paused()) {
                  <app-icon name="play" [size]="11" /> Resume
                } @else {
                  <app-icon name="pause" [size]="11" /> Pause
                }
              </button>
              @if (!paused()) {
                <button
                  #pauseCaretBtn
                  class="action pause-caret"
                  (click)="togglePauseMenu(pauseCaretBtn)"
                  title="Pause for…"
                  aria-label="Pause for…"
                  aria-haspopup="true"
                  [attr.aria-expanded]="pauseMenuOpen"
                >
                  <app-icon name="chevron-down" [size]="11" />
                </button>
                @if (pauseMenuOpen) {
                  <div
                    class="pause-menu"
                    role="menu"
                    [style.top.px]="menuAnchor.top"
                    [style.right.px]="menuAnchor.right"
                    (click)="$event.stopPropagation()"
                  >
                    <div class="pm-title">Pause for…</div>
                    @for (opt of pauseTimerOptions; track opt.secs) {
                      <button class="pm-item" role="menuitem" (click)="pauseFor(opt.secs)">
                        {{ opt.label }}
                      </button>
                    }
                    <div class="pm-custom">
                      <input
                        type="number"
                        min="1"
                        placeholder="min"
                        aria-label="Custom pause duration in minutes"
                        [(ngModel)]="customPauseMin"
                        (keydown.enter)="pauseForCustom()"
                      />
                      <button class="pm-go" (click)="pauseForCustom()">Go</button>
                    </div>
                  </div>
                }
              }
            </div>
            <button
              class="action muted shortcuts-open"
              (click)="openShortcuts()"
              title="Keyboard shortcuts (?)"
              aria-label="Keyboard shortcuts"
            >
              ?
            </button>
            <button class="action muted" (click)="onLogout()" title="Sign out">Sign out</button>
          </div>
        </nav>

        @if (noServers()) {
          <div class="setup-banner" role="status">
            <div class="wrap">
              <span>No news servers are set up yet, so downloads will not start.</span>
              <a routerLink="/welcome">Add a server →</a>
            </div>
          </div>
        }

        <main>
          <div class="wrap">
            <router-outlet />
          </div>
        </main>
      </div>
      @if (fileDragActive()) {
        <div class="drop-overlay" aria-hidden="true">
          <div class="drop-overlay-box">Drop .nzb files to add them</div>
        </div>
      }
      <!-- Results that arrive without a focus change are spoken here. The
           regions stay mounted so the text change itself is the announcement. -->
      <div class="sr-only" role="status" aria-live="polite" aria-atomic="true">
        {{ announcer.polite() }}
      </div>
      <div class="sr-only" role="alert" aria-live="assertive" aria-atomic="true">
        {{ announcer.assertive() }}
      </div>
    }
  `,
  styles: [
    `
      /* Pointer-transparent so the drop reaches the element underneath
         (the Add NZB dropzone handles its own). */
      .drop-overlay {
        position: fixed;
        inset: 0;
        z-index: 1000;
        pointer-events: none;
        display: flex;
        align-items: center;
        justify-content: center;
        background: color-mix(in srgb, var(--bg) 70%, transparent);
      }
      .drop-overlay-box {
        padding: 32px 48px;
        border: 2px dashed var(--accent);
        border-radius: 12px;
        background: var(--bg);
        color: var(--accent);
        font-size: 18px;
        font-weight: 600;
      }
      :host {
        display: block;
        height: 100vh;
        overflow: hidden;
      }

      .shell {
        display: flex;
        flex-direction: column;
        height: 100vh;
      }

      /* ---- Width-mode wrap ----
       Header, nav, and main render full-bleed backgrounds but wrap their
       content in .wrap. Compact mode clamps .wrap to 1320px and centers it,
       so chrome and body align. Expanded mode uses the full viewport width
       with a small gutter. Mode is toggled via [data-width-mode] on <body>. */
      .wrap {
        width: 100%;
        max-width: 1320px;
        margin: 0 auto;
        padding: 0 20px;
        box-sizing: border-box;
      }
      :host-context(body[data-width-mode='expanded']) .wrap {
        max-width: none;
        padding: 0 24px;
      }

      /* ---- Combined topbar ---- */
      .brand {
        font-weight: 700;
        font-size: 15px;
        letter-spacing: 0.2px;
      }
      .brand span {
        color: var(--accent);
      }
      .ver {
        color: var(--mute);
        font-size: 11px;
        margin-left: 6px;
        font-weight: 400;
      }
      .sep {
        width: 1px;
        height: 18px;
        background: var(--line);
        margin: 0 8px;
        flex-shrink: 0;
      }
      .status {
        display: flex;
        gap: 6px;
        align-items: center;
      }
      .status.stale .pill:not(.err) { opacity: 0.6; }

      nav.topbar {
        background: var(--panel);
        border-bottom: 1px solid var(--line);
        flex-shrink: 0;
      }
      nav.topbar .wrap {
        display: flex;
        align-items: center;
        overflow-x: auto;
        padding-top: 0;
        padding-bottom: 0;
        gap: 0;
      }
      nav.topbar a {
        color: var(--mute);
        padding: 12px 14px;
        border-bottom: 2px solid transparent;
        text-decoration: none;
        font-size: 13px;
        white-space: nowrap;
        transition: color 0.15s;
      }
      nav.topbar a:hover {
        color: var(--text);
        text-decoration: none;
      }
      nav.topbar a.active {
        color: var(--text);
        border-bottom-color: var(--accent);
      }

      nav.topbar .spacer {
        flex: 1;
      }
      nav.topbar .action {
        background: none;
        border: none;
        color: var(--text);
        padding: 12px 12px;
        cursor: pointer;
        font: inherit;
        font-size: 13px;
        opacity: 0.85;
      }
      nav.topbar .action:hover {
        opacity: 1;
      }
      nav.topbar .action.primary {
        color: var(--accent2);
        font-weight: 600;
      }
      nav.topbar .action.muted {
        color: var(--mute);
        font-size: 12px;
      }
      nav.topbar .demo-exit {
        border: 1px solid var(--accent);
        border-radius: 6px;
        color: var(--accent);
        font-weight: 600;
        margin: 0 8px;
        padding: 5px 10px;
      }
      nav.topbar .demo-exit:hover {
        background: color-mix(in srgb, var(--accent) 12%, transparent);
        color: var(--text);
      }

      /* Pause split-button + dropdown */
      .pause-group {
        position: relative;
        display: flex;
        align-items: center;
      }
      .pause-caret {
        padding: 10px 6px !important;
        font-size: 11px !important;
        margin-left: -6px;
      }
      /* Speed-limit pill: a button that opens a popover under the status pills. */
      .speed-group {
        position: relative;
        display: flex;
        align-items: center;
      }
      .pill-btn {
        font: inherit;
        color: inherit;
        cursor: pointer;
      }
      .pill-btn:hover {
        border-color: var(--accent);
      }
      .pm-current {
        font-size: 12px;
        color: var(--mute);
        padding: 0 8px 6px;
      }
      .pm-item.current {
        color: var(--accent);
        font-weight: 600;
      }

      /* First-run banner: shown under the header until a server is configured. */
      .setup-banner {
        flex-shrink: 0;
        background: var(--panel2);
        border-bottom: 1px solid var(--line);
        border-left: 3px solid var(--warn);
        font-size: 13px;
      }
      .setup-banner .wrap {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        gap: 6px 16px;
        padding-top: 10px;
        padding-bottom: 10px;
      }
      .setup-banner a {
        color: var(--accent);
        font-weight: 600;
        text-decoration: none;
      }
      .setup-banner a:hover {
        text-decoration: underline;
      }

      /* Menus are fixed to the viewport, positioned from the trigger's rect on open
         (see anchorMenu). The nav scrolls horizontally, so an absolutely positioned
         menu inside it would be clipped to the nav's height. */
      .pause-menu,
      .speed-menu {
        position: fixed;
        box-sizing: border-box;
        width: 240px; /* MENU_WIDTH in TS: keep in step */
        max-width: calc(100vw - 16px);
        background: var(--panel);
        border: 1px solid var(--line);
        border-radius: 6px;
        box-shadow: 0 8px 24px rgba(0, 0, 0, 0.35);
        padding: 6px;
        min-width: 160px;
        z-index: 40;
      }
      .pm-title {
        font-size: 11px;
        color: var(--mute);
        padding: 4px 8px 6px;
        text-transform: uppercase;
        letter-spacing: 0.4px;
      }
      .pm-item {
        display: block;
        width: 100%;
        text-align: left;
        background: none;
        border: none;
        color: var(--text);
        padding: 6px 10px;
        border-radius: 4px;
        cursor: pointer;
        font: inherit;
        font-size: 13px;
      }
      .pm-item:hover {
        background: var(--panel2);
      }
      .pm-custom {
        display: flex;
        gap: 4px;
        padding: 6px 4px 2px;
        border-top: 1px solid var(--line);
        margin-top: 4px;
      }
      .pm-custom input {
        flex: 1;
        min-width: 0;
        background: var(--panel2);
        border: 1px solid var(--line);
        color: var(--text);
        padding: 5px 8px;
        border-radius: 4px;
        font: inherit;
        font-size: 12px;
        outline: none;
      }
      .pm-go {
        background: var(--accent-fill);
        color: #fff;
        border: none;
        padding: 5px 10px;
        border-radius: 4px;
        cursor: pointer;
        font: inherit;
        font-size: 12px;
      }

      /* ---- Main area ---- */
      main {
        flex: 1;
        overflow-y: auto;
      }
      main .wrap {
        padding-bottom: 28px;
      }

      /* ---- Width-mode toggle (icon-only segmented control) ---- */
      .mode-toggle {
        display: inline-flex;
        align-items: center;
        background: var(--panel2);
        border: 1px solid var(--line);
        border-radius: 6px;
        padding: 2px;
        margin: 0 6px;
        flex-shrink: 0;
      }
      .mode-toggle button {
        background: none;
        border: none;
        color: var(--mute);
        padding: 4px 6px;
        border-radius: 4px;
        cursor: pointer;
        display: inline-flex;
        align-items: center;
      }
      .mode-toggle button:hover {
        color: var(--text);
      }
      .mode-toggle button.active {
        background: var(--panel);
        color: var(--text);
        box-shadow: inset 0 0 0 1px var(--line);
      }
      .mode-toggle svg {
        width: 14px;
        height: 14px;
      }
      .shortcuts-open {
        font-family: ui-monospace, Menlo, Consolas, monospace;
        font-size: 13px;
        min-width: 26px;
      }
    `,
  ],
})
export class App implements OnInit, OnDestroy {
  readonly demoMode = isDemoPath(window.location.pathname);
  version = signal('');

  speed = signal(0);
  paused: WritableSignal<boolean>;
  queueCount = signal(0);
  diskFree = signal(0);
  // Nothing is shown as data until the first status poll succeeds. An error
  // turns the top-bar pill into Offline and keeps the last known values.
  loaded = signal(false);
  offline = signal(false);
  webdavEnabled = signal(false);
  readonly authenticated: Signal<boolean>;
  private readonly currentUrl = signal('');
  // Keyed on the route as well as the session: swapping branches rebuilds the
  // router outlet, and doing that under /login mid-submit would re-run the
  // login page's redirect and race the navigation to /welcome.
  readonly showChrome = computed(() => this.authenticated() && !isBareRoute(this.currentUrl()));
  pauseMenuOpen = false;
  customPauseMin: number | null = null;
  @ViewChild('pauseCaretBtn') pauseCaretBtn?: ElementRef<HTMLButtonElement>;
  readonly pauseTimerOptions = [
    { label: '5 minutes', secs: 5 * 60 },
    { label: '15 minutes', secs: 15 * 60 },
    { label: '30 minutes', secs: 30 * 60 },
    { label: '1 hour', secs: 60 * 60 },
    { label: '2 hours', secs: 2 * 60 * 60 },
  ];
  // Global speed limit, from the status poll. 0 means unlimited.
  speedLimit = signal(0);
  speedMenuOpen = false;
  customSpeedMb: number | null = null;
  /** First key of a two-key chord (`g d`), waiting for its second. */
  private chordPrefix = '';
  private chordTimer: ReturnType<typeof setTimeout> | null = null;
  readonly speedPresets = [
    { label: 'Unlimited', bps: 0 },
    { label: '1 MB/s', bps: 1 * MIB },
    { label: '5 MB/s', bps: 5 * MIB },
    { label: '10 MB/s', bps: 10 * MIB },
    { label: '25 MB/s', bps: 25 * MIB },
    { label: '50 MB/s', bps: 50 * MIB },
  ];
  // Null until the server list has loaded, so the banner never shows on a guess.
  readonly serverCount = signal<number | null>(null);
  readonly noServers = computed(() => this.serverCount() === 0);
  private pollTimer: ReturnType<typeof setInterval> | null = null;
  private statusInFlight = false;
  /** Permission can be revoked while the page is open; re-read it on return. */
  private onVisibilityChange = () => {
    if (!pageHidden()) {
      this.notifications.refreshPermission();
      // Poll at once rather than waiting out the interval, so a job that
      // finished while the tab was hidden shows the moment it is opened.
      if (!this.statusInFlight) this.pollStatus();
    }
  };
  private docClickHandler = (e: MouseEvent) => {
    const target = e.target as HTMLElement;
    if (this.pauseMenuOpen && !target.closest('.pause-group')) this.pauseMenuOpen = false;
    if (this.speedMenuOpen && !target.closest('.speed-group')) this.speedMenuOpen = false;
  };

  constructor(
    private api: ApiService,
    private authService: AuthService,
    private router: Router,
    private addNzbService: AddNzbService,
    public widthMode: WidthModeService,
    readonly theme: ThemeService,
    pauseState: PauseStateService,
    private toast: ToastService,
    private notifications: BrowserNotificationsService,
    readonly announcer: AnnouncerService,
    private shortcuts: KeyboardShortcutsService,
    private dialog: MatDialog,
  ) {
    this.paused = pauseState.paused;
    this.authenticated = authService.authenticated;
    this.currentUrl.set(router.url);
    router.events
      .pipe(filter((e): e is NavigationEnd => e instanceof NavigationEnd))
      .subscribe((e) => {
        this.currentUrl.set(e.urlAfterRedirects);
        this.updateTitle();
        // Re-check after each navigation so the banner clears once a server is added.
        this.loadServerCount();
      });
  }

  ngOnInit(): void {
    this.pollStatus();
    this.loadServerCount();
    document.addEventListener('visibilitychange', this.onVisibilityChange);
    this.pollTimer = setInterval(() => {
      // A background tab polls only to feed browser notifications. Alerts
      // are for a user looking at another tab, so skipping the poll while
      // hidden would mean they never fire; with alerts off the old skip
      // still applies and a hidden tab makes no requests.
      if (this.statusInFlight) return;
      if (pageHidden() && !this.notifications.enabled()) return;
      this.pollStatus();
    }, 2000);
    document.addEventListener('click', this.docClickHandler);
  }

  ngOnDestroy(): void {
    if (this.pollTimer) clearInterval(this.pollTimer);
    if (this.chordTimer) clearTimeout(this.chordTimer);
    document.removeEventListener('click', this.docClickHandler);
    document.removeEventListener('visibilitychange', this.onVisibilityChange);
  }

  pollStatus(): void {
    if (!this.authenticated()) return;
    this.statusInFlight = true;
    this.api.get<StatusResponse>('/status').pipe(timeout(POLL_TIMEOUT_MS)).subscribe({
      next: (s) => {
        this.statusInFlight = false;
        this.speed.set(s.speed_bps);
        this.speedLimit.set(s.speed_limit_bps ?? 0);
        this.paused.set(s.paused);
        this.queueCount.set(s.queue_size);
        this.diskFree.set(s.disk_space_free);
        this.webdavEnabled.set(!!s.webdav_enabled);
        if (s.version) this.version.set(s.version);
        this.loaded.set(true);
        this.offline.set(false);
        this.notifications.observeStatus(s);
        this.updateTitle();
      },
      error: () => {
        this.statusInFlight = false;
        this.offline.set(true);
        this.updateTitle();
      },
    });
  }

  loadServerCount(): void {
    if (!this.authenticated()) return;
    this.api.get<unknown[]>('/config/servers').subscribe({
      next: (servers) => this.serverCount.set(Array.isArray(servers) ? servers.length : null),
      // A failed check leaves the count unknown, so no banner is shown on a guess.
      error: () => this.serverCount.set(null),
    });
  }

  private updateTitle(): void {
    const live =
      this.loaded() && !this.offline()
        ? { paused: this.paused(), speedBps: this.speed(), queueSize: this.queueCount() }
        : null;
    document.title = documentTitle(this.currentUrl(), live);
  }

  // Viewport position for whichever menu is open. Only one menu is open at a time.
  menuAnchor = { top: 0, left: 0, right: 0 };

  /** Place a fixed menu just below its trigger, kept inside the viewport. */
  private anchorMenu(trigger: HTMLElement): void {
    const r = trigger.getBoundingClientRect();
    const maxLeft = Math.max(VIEWPORT_GUTTER, window.innerWidth - MENU_WIDTH - VIEWPORT_GUTTER);
    this.menuAnchor = {
      top: r.bottom + 4,
      left: Math.min(Math.max(VIEWPORT_GUTTER, r.left), maxLeft),
      right: Math.max(VIEWPORT_GUTTER, window.innerWidth - r.right),
    };
  }

  togglePauseMenu(trigger: HTMLElement): void {
    this.pauseMenuOpen = !this.pauseMenuOpen;
    if (this.pauseMenuOpen) this.anchorMenu(trigger);
    this.speedMenuOpen = false;
  }

  toggleSpeedMenu(trigger: HTMLElement): void {
    this.speedMenuOpen = !this.speedMenuOpen;
    if (this.speedMenuOpen) this.anchorMenu(trigger);
    this.pauseMenuOpen = false;
  }

  @HostListener('window:resize')
  onWindowResize(): void {
    // A menu anchored to the old layout would sit in the wrong place, so close it.
    this.pauseMenuOpen = false;
    this.speedMenuOpen = false;
  }

  closeSpeedMenu(): void {
    this.speedMenuOpen = false;
  }

  setSpeedLimit(bps: number): void {
    this.api.put('/config/speed-limit', { speed_limit_bps: bps }).subscribe({
      next: () => {
        this.speedLimit.set(bps);
        this.speedMenuOpen = false;
        this.toast.success(
          bps > 0 ? `Speed limit set to ${formatSpeed(bps)}` : 'Speed limit off',
        );
        this.pollStatus();
      },
      error: (err) => showHttpError(this.toast, err, 'Failed to set speed limit'),
    });
  }

  setCustomSpeedLimit(): void {
    const mb = this.customSpeedMb;
    if (!mb || mb <= 0) return;
    this.setSpeedLimit(Math.round(mb * MIB));
    this.customSpeedMb = null;
  }

  onLogout(): void {
    this.authService.logout().subscribe({
      complete: () => this.router.navigate(['/login']),
      error: () => this.router.navigate(['/login']),
    });
  }

  // ---- Drop .nzb files / paste a URL anywhere ----

  // dragenter/dragleave fire for every child crossed, so count them.
  private dragDepth = 0;
  readonly fileDragActive = signal(false);

  private dragHasFiles(e: DragEvent): boolean {
    return !!e.dataTransfer && Array.from(e.dataTransfer.types).includes('Files');
  }

  @HostListener('window:dragenter', ['$event'])
  onWindowDragEnter(e: DragEvent): void {
    if (!this.showChrome() || !this.dragHasFiles(e)) return;
    this.dragDepth++;
    this.fileDragActive.set(true);
  }

  @HostListener('window:dragover', ['$event'])
  onWindowDragOver(e: DragEvent): void {
    if (!this.showChrome() || !this.dragHasFiles(e)) return;
    // Without this the browser opens the dropped file instead of firing drop.
    e.preventDefault();
  }

  @HostListener('window:dragleave', ['$event'])
  onWindowDragLeave(e: DragEvent): void {
    if (!this.fileDragActive() || !this.dragHasFiles(e)) return;
    this.dragDepth = Math.max(0, this.dragDepth - 1);
    if (this.dragDepth === 0) this.fileDragActive.set(false);
  }

  @HostListener('window:drop', ['$event'])
  onWindowDrop(e: DragEvent): void {
    this.dragDepth = 0;
    this.fileDragActive.set(false);
    // defaultPrevented: the Add NZB panel's own dropzone already took them.
    if (!this.showChrome() || !this.dragHasFiles(e) || e.defaultPrevented) return;
    e.preventDefault();
    const files = nzbUploadFiles(e.dataTransfer?.files);
    if (files.length === 0) {
      this.toast.info('Only .nzb (or .zip/.rar/.7z/.gz) files can be added');
      this.announcer.announceError('Only .nzb (or .zip/.rar/.7z/.gz) files can be added');
      return;
    }
    this.openAddPanelWith({ files });
  }

  @HostListener('window:paste', ['$event'])
  onWindowPaste(e: ClipboardEvent): void {
    if (!this.showChrome() || isEditableTarget(e.target)) return;
    const url = pastedNzbUrl(e.clipboardData?.getData('text'));
    if (!url) return;
    e.preventDefault();
    this.openAddPanelWith({ url });
  }

  // ---- Keyboard shortcuts ----
  //
  // One window-level handler rather than per-page ones, so the bindings are
  // the same everywhere and the help sheet is the whole list. Typing in a
  // field, a modifier chord, or a dialog all leave the event alone.
  @HostListener('window:keydown', ['$event'])
  onShortcutKey(event: KeyboardEvent): void {
    if (!this.showChrome()) return;
    if (event.defaultPrevented) return;
    if (event.ctrlKey || event.metaKey || event.altKey) return;
    if (isEditableTarget(event.target)) return;
    if (this.overlayOwnsKeys()) return;

    const key = event.key.length === 1 ? event.key.toLowerCase() : event.key;
    if (event.key === 'Shift') return; // bare modifier press

    if (this.chordPrefix) {
      const prefix = this.chordPrefix;
      this.clearChord();
      const action = resolveShortcut(key, prefix);
      if (!action) return;
      event.preventDefault();
      this.runShortcut(action);
      return;
    }

    if (startsChord(key)) {
      event.preventDefault();
      this.beginChord(key);
      return;
    }

    const action = resolveShortcut(key);
    if (!action) return;
    event.preventDefault();
    this.runShortcut(action);
  }

  /**
   * True while an overlay on screen owns the keyboard (Escape, arrows, typing).
   *
   * Not "any `.cdk-overlay-pane`": a snackbar, tooltip or select panel shares
   * that container, and CDK can leave a detached pane behind, so matching the
   * container outright silently disabled every shortcut after the first toast.
   * Ask the dialog service and this shell's own menus instead, and only then
   * look for the panels that do take keys.
   */
  private overlayOwnsKeys(): boolean {
    if (this.dialog.openDialogs.length > 0) return true;
    if (this.pauseMenuOpen || this.speedMenuOpen) return true;
    return !!document.querySelector(
      [
        '.cdk-overlay-pane .mat-mdc-dialog-container',
        '.cdk-overlay-pane .mat-mdc-menu-panel',
        '.cdk-overlay-pane .mat-mdc-select-panel',
        '.cdk-overlay-pane .mat-mdc-autocomplete-panel',
      ].join(', '),
    );
  }

  private beginChord(prefix: string): void {
    this.clearChord();
    this.chordPrefix = prefix;
    this.chordTimer = setTimeout(() => this.clearChord(), CHORD_TIMEOUT_MS);
  }

  private clearChord(): void {
    this.chordPrefix = '';
    if (this.chordTimer) {
      clearTimeout(this.chordTimer);
      this.chordTimer = null;
    }
  }

  private runShortcut(action: string): void {
    switch (action) {
      case 'togglePause':
        this.togglePause();
        break;
      case 'upload':
        this.onAddNzb();
        break;
      case 'focusSearch':
        this.focusSearch();
        break;
      case 'goDownloads':
        this.router.navigate(['/downloads']);
        break;
      case 'goHistory':
        this.goHistory();
        break;
      case 'goSettings':
        this.router.navigate(['/settings']);
        break;
      case 'toggleLayout':
        this.widthMode.toggle();
        break;
      case 'showHelp':
        this.openShortcuts();
        break;
    }
  }

  /**
   * Search lives on the header-search page; on Downloads the Queue page owns
   * its add panel, so `/` goes to the page that has the box and asks it for
   * focus. The page subscribes and focuses once its view exists.
   */
  private focusSearch(): void {
    if (this.router.url.startsWith('/groups')) {
      this.shortcuts.requestSearchFocus();
    } else {
      this.router.navigate(['/groups']).then(() => this.shortcuts.requestSearchFocus());
    }
  }

  /** The queue view owns the landing: it expands the panel and scrolls to it. */
  private goHistory(): void {
    void this.router.navigate(['/downloads'], { fragment: 'history' });
  }

  openShortcuts(): void {
    this.dialog.open(ShortcutsDialogComponent, { width: '420px', autoFocus: 'dialog' });
  }

  private openAddPanelWith(prefill: AddNzbPrefill): void {
    if (!this.router.url.startsWith('/downloads')) {
      this.router.navigate(['/downloads']).then(() => this.addNzbService.prefill(prefill));
    } else {
      this.addNzbService.prefill(prefill);
    }
  }

  onAddNzb(): void {
    if (!this.router.url.startsWith('/downloads')) {
      this.router.navigate(['/downloads']).then(() => this.addNzbService.togglePanel());
    } else {
      this.addNzbService.togglePanel();
    }
  }

  togglePause(): void {
    const wasPaused = this.paused();
    const action = wasPaused ? '/queue/resume' : '/queue/pause';
    // Update every consumer immediately; the backend remains authoritative
    // and the next status poll corrects this if the request fails.
    this.paused.set(!wasPaused);
    this.api.post(action).subscribe({
      next: () => this.pollStatus(),
      error: (err) => {
        this.paused.set(wasPaused);
        this.pollStatus();
        showHttpError(
          this.toast,
          err,
          wasPaused ? 'Failed to resume downloads' : 'Failed to pause downloads',
        );
      },
    });
    this.pauseMenuOpen = false;
  }

  closePauseMenu(): void {
    if (!this.pauseMenuOpen) return;
    this.pauseMenuOpen = false;
    this.pauseCaretBtn?.nativeElement.focus();
  }

  pauseFor(secs: number): void {
    this.api.post(`/queue/pause-for?duration_secs=${secs}`).subscribe({
      next: () => this.pollStatus(),
      error: (err) => showHttpError(this.toast, err, 'Failed to pause downloads'),
    });
    this.pauseMenuOpen = false;
  }

  pauseForCustom(): void {
    const mins = this.customPauseMin;
    if (!mins || mins <= 0) return;
    this.pauseFor(Math.round(mins * 60));
    this.customPauseMin = null;
  }

  formatSpeed(bps: number): string {
    return formatSpeed(bps);
  }

  formatBytes(bytes: number): string {
    return formatBytes(bytes);
  }
}
