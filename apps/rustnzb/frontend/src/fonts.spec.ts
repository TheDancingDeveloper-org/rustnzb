import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';

// Specs run from the frontend directory.
const publicDir = resolve('public');
const indexHtml = readFileSync(resolve('src/index.html'), 'utf8');
const styles = readFileSync(resolve('src/styles.scss'), 'utf8');

describe('bundled fonts', () => {
  it('index.html makes no request to a third-party font host', () => {
    expect(indexHtml).not.toContain('fonts.googleapis.com');
    expect(indexHtml).not.toContain('fonts.gstatic.com');
    expect(indexHtml).not.toContain('preconnect');
  });

  it('declares Inter locally at the weights the UI uses', () => {
    expect(styles).toContain("font-family: 'Inter'");
    expect(styles).toMatch(/font-weight:\s*400 700/);
    // The family must resolve to a file that ships, not to a CDN URL.
    const urls = [...styles.matchAll(/url\('([^']+\.woff2)'\)/g)].map((m) => m[1]);
    expect(urls.length).toBeGreaterThan(0);
    for (const url of urls) {
      expect(url.startsWith('/fonts/'), url).toBe(true);
      expect(existsSync(resolve(publicDir, url.slice(1))), url).toBe(true);
    }
  });

  it('ships the licence next to the font files', () => {
    expect(existsSync(resolve(publicDir, 'fonts/OFL.txt'))).toBe(true);
    expect(readFileSync(resolve(publicDir, 'fonts/OFL.txt'), 'utf8')).toContain(
      'SIL Open Font License',
    );
    // Only the subsets the stylesheet references; no stray full-family dumps.
    const files = readdirSync(resolve(publicDir, 'fonts')).filter((f) => f.endsWith('.woff2'));
    for (const file of files) expect(styles).toContain(`/fonts/${file}`);
  });

  it('serves woff2, which every supported browser accepts', () => {
    const buf = readFileSync(resolve(publicDir, 'fonts/inter-latin.woff2'));
    expect(buf.subarray(0, 4).toString('ascii')).toBe('wOF2');
  });
});

describe('motion preference', () => {
  it('collapses animation and transitions under prefers-reduced-motion', () => {
    expect(styles).toContain('@media (prefers-reduced-motion: reduce)');
    const block = styles.slice(styles.indexOf('@media (prefers-reduced-motion: reduce)'));
    expect(block).toContain('animation-duration: 0.01ms !important');
    expect(block).toContain('transition-duration: 0.01ms !important');
  });

  it('provides a screen-reader-only region class for live announcements', () => {
    expect(styles).toMatch(/\.sr-only\s*\{[^}]*clip:\s*rect\(0, 0, 0, 0\)/s);
  });
});
