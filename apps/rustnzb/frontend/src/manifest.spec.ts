import { existsSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';

// Specs run from the frontend directory, so these paths are relative to it.
const publicDir = resolve('public');
const manifest = JSON.parse(readFileSync(resolve(publicDir, 'manifest.webmanifest'), 'utf8'));
const indexHtml = readFileSync(resolve('src/index.html'), 'utf8');

/** Width and height from a PNG's IHDR chunk. */
function pngSize(file: string): string {
  const buf = readFileSync(file);
  return `${buf.readUInt32BE(16)}x${buf.readUInt32BE(20)}`;
}

describe('PWA manifest', () => {
  it('is installable: name, standalone display and relative start_url', () => {
    expect(manifest.name).toBe('rustnzb');
    expect(manifest.short_name).toBeTruthy();
    expect(manifest.display).toBe('standalone');
    // Relative, so the app installs correctly behind a reverse-proxy subpath.
    expect(manifest.start_url).toBe('./');
    expect(manifest.scope).toBe('./');
  });

  it('has 192 and 512 PNG icons whose files match their declared sizes', () => {
    const sizes = manifest.icons.map((i: { sizes: string }) => i.sizes);
    expect(sizes).toContain('192x192');
    expect(sizes).toContain('512x512');
    for (const icon of manifest.icons) {
      const file = resolve(publicDir, icon.src);
      expect(existsSync(file), icon.src).toBe(true);
      expect(icon.type).toBe('image/png');
      expect(pngSize(file)).toBe(icon.sizes);
    }
  });

  it('matches the default dark theme background', () => {
    const styles = readFileSync(resolve('src/styles.scss'), 'utf8');
    const bg = /--bg:\s*(#[0-9a-f]{6})/i.exec(styles)?.[1];
    expect(manifest.theme_color).toBe(bg);
    expect(manifest.background_color).toBe(bg);
  });

  it('is linked from index.html with a matching theme-color', () => {
    expect(indexHtml).toContain('<link rel="manifest" href="manifest.webmanifest" />');
    expect(indexHtml).toContain(`<meta name="theme-color" content="${manifest.theme_color}" />`);
  });
});
