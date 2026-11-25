import { Component, Input } from '@angular/core';

export type IconName =
  | 'close'
  | 'play'
  | 'pause'
  | 'retry' | 'key'
  | 'chevron-down'
  | 'chevron-right'
  | 'drag-handle';

/**
 * Small inline SVG icon set, replacing the ad-hoc Unicode glyphs (✕ ▶ ❚❚ ↻
 * ▸ ▾ ⋮⋮) scattered across row actions — glyph weight/size varies by OS
 * font, these render identically everywhere.
 */
@Component({
  selector: 'app-icon',
  standalone: true,
  template: `
    <svg
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      stroke-width="1.6"
      [style.width.px]="size"
      [style.height.px]="size"
      aria-hidden="true"
    >
      @switch (name) {
        @case ('close') {
          <path d="M4.5 4.5l7 7M11.5 4.5l-7 7" stroke-linecap="round" />
        }
        @case ('play') {
          <path d="M5 3.5l8 4.5-8 4.5V3.5z" fill="currentColor" stroke="none" />
        }
        @case ('pause') {
          <path d="M4.5 3.5h2.3v9H4.5zM9.2 3.5h2.3v9H9.2z" fill="currentColor" stroke="none" />
        }
        @case ('retry') {
          <path
            d="M12.5 8A4.5 4.5 0 1 1 11 4.6M12.5 2v3.2h-3.2"
            stroke-linecap="round"
            stroke-linejoin="round"
          />
        }
        @case ('key') {
          <circle cx="5" cy="8" r="2.5" />
          <path d="M7.5 8H14M11.5 8v2.5" stroke-linecap="round" />
        }
        @case ('chevron-down') {
          <path d="M4 6l4 4 4-4" stroke-linecap="round" stroke-linejoin="round" />
        }
        @case ('chevron-right') {
          <path d="M6 4l4 4-4 4" stroke-linecap="round" stroke-linejoin="round" />
        }
        @case ('drag-handle') {
          <circle cx="5" cy="4" r="1" fill="currentColor" stroke="none" />
          <circle cx="11" cy="4" r="1" fill="currentColor" stroke="none" />
          <circle cx="5" cy="8" r="1" fill="currentColor" stroke="none" />
          <circle cx="11" cy="8" r="1" fill="currentColor" stroke="none" />
          <circle cx="5" cy="12" r="1" fill="currentColor" stroke="none" />
          <circle cx="11" cy="12" r="1" fill="currentColor" stroke="none" />
        }
      }
    </svg>
  `,
  styles: [
    `
      :host {
        display: inline-flex;
        align-items: center;
        vertical-align: -2px;
      }
    `,
  ],
})
export class IconComponent {
  @Input() name!: IconName;
  @Input() size = 14;
}
