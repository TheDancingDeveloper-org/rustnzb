import {
  Component,
  ElementRef,
  OnInit,
  OnDestroy,
  ViewChild,
  signal,
  WritableSignal,
} from '@angular/core';
import { CommonModule } from '@angular/common';
import { FormsModule } from '@angular/forms';
import { Router, RouterModule } from '@angular/router';
import { ApiService } from './core/services/api.service';
import { AuthService } from './core/services/auth.service';
import { StatusResponse } from './core/models/queue.model';
import { AddNzbService } from './core/services/add-nzb.service';
import { WidthModeService } from './core/services/width-mode.service';
import { PauseStateService } from './core/services/pause-state.service';
import { ThemeService } from './core/services/theme.service';
import { IconComponent } from './shared/icon.component';

@Component({
  selector: 'app-root',
  standalone: true,
  imports: [CommonModule, FormsModule, RouterModule, IconComponent],
  template: `
    @if (!authenticated()) {
      <!-- Full-screen login (no chrome) -->
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
            <div class="status">
              <span class="pill" [class.ok]="!paused()" [class.warn]="paused()">
                ● {{ paused() ? 'Paused' : 'Live' }}
              </span>
              <span class="pill">{{ formatSpeed(speed()) }}</span>
              <span class="pill">{{ queueCount() }} queued</span>
              <span class="pill">{{ formatBytes(diskFree()) }} free</span>
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
                  (click)="pauseMenuOpen = !pauseMenuOpen"
                  title="Pause for…"
                  aria-label="Pause for…"
                  aria-haspopup="true"
                  [attr.aria-expanded]="pauseMenuOpen"
                >
                  <app-icon name="chevron-down" [size]="11" />
                </button>
                @if (pauseMenuOpen) {
                  <div class="pause-menu" role="menu" (click)="$event.stopPropagation()">
                    <div class="pm-title">Pause for…</div>
                    @for (opt of pauseTimerOptions; track opt.secs) {
                      <button class="pm-item" role="menuitem" (click)="pauseFor(opt.secs)">{{ opt.label }}</button>
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
            <button class="action muted" (click)="onLogout()" title="Sign out">Sign out</button>
          </div>
        </nav>

        <main>
          <div class="wrap">
            <router-outlet />
          </div>
        </main>
      </div>
    }
  `,
  styles: [
    `
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
      .pause-menu {
        position: absolute;
        top: 100%;
        right: 0;
        margin-top: 4px;
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
        background: var(--accent);
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
    `,
  ],
})
export class App implements OnInit, OnDestroy {
  version = signal('');

  speed = signal(0);
  paused: WritableSignal<boolean>;
  queueCount = signal(0);
  diskFree = signal(0);
  webdavEnabled = signal(false);
  authenticated = signal(false);
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
  private pollTimer: ReturnType<typeof setInterval> | null = null;
  private docClickHandler = (e: MouseEvent) => {
    if (!this.pauseMenuOpen) return;
    const el = (e.target as HTMLElement).closest('.pause-group');
    if (!el) this.pauseMenuOpen = false;
  };

  constructor(
    private api: ApiService,
    private authService: AuthService,
    private router: Router,
    private addNzbService: AddNzbService,
    public widthMode: WidthModeService,
    readonly theme: ThemeService,
    pauseState: PauseStateService,
  ) {
    this.paused = pauseState.paused;
  }

  ngOnInit(): void {
    this.authenticated.set(this.authService.isLoggedIn());
    this.pollStatus();
    this.pollTimer = setInterval(() => this.pollStatus(), 2000);
    document.addEventListener('click', this.docClickHandler);
  }

  ngOnDestroy(): void {
    if (this.pollTimer) clearInterval(this.pollTimer);
    document.removeEventListener('click', this.docClickHandler);
  }

  pollStatus(): void {
    this.authenticated.set(this.authService.isLoggedIn());
    if (!this.authenticated()) return;
    this.api.get<StatusResponse>('/status').subscribe({
      next: (s) => {
        this.speed.set(s.speed_bps);
        this.paused.set(s.paused);
        this.queueCount.set(s.queue_size);
        this.diskFree.set(s.disk_space_free);
        this.webdavEnabled.set(!!s.webdav_enabled);
        if (s.version) this.version.set(s.version);
      },
      error: () => {},
    });
  }

  onLogout(): void {
    this.authenticated.set(false);
    this.authService.logout().subscribe({
      complete: () => this.router.navigate(['/login']),
      error: () => this.router.navigate(['/login']),
    });
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
      error: () => {
        this.paused.set(wasPaused);
        this.pollStatus();
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
    this.api.post(`/queue/pause-for?duration_secs=${secs}`).subscribe(() => this.pollStatus());
    this.pauseMenuOpen = false;
  }

  pauseForCustom(): void {
    const mins = this.customPauseMin;
    if (!mins || mins <= 0) return;
    this.pauseFor(Math.round(mins * 60));
    this.customPauseMin = null;
  }

  formatSpeed(bps: number): string {
    if (bps === 0) return '0 B/s';
    const k = 1024;
    const sizes = ['B/s', 'KB/s', 'MB/s', 'GB/s'];
    const i = Math.floor(Math.log(bps) / Math.log(k));
    return parseFloat((bps / Math.pow(k, i)).toFixed(1)) + ' ' + sizes[i];
  }

  formatBytes(bytes: number): string {
    if (!bytes) return '0 B';
    const k = 1024;
    const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(1)) + ' ' + sizes[i];
  }
}
