import { formatBytes, formatBytesParts, formatSpeed } from './format';

describe('formatBytes', () => {
  it('walks the unit ladder up to EB (BUG-120)', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(1)).toBe('1 B');
    expect(formatBytes(1023)).toBe('1023 B');
    expect(formatBytes(1024)).toBe('1.0 KB');
    expect(formatBytes(1024 ** 2)).toBe('1.0 MB');
    expect(formatBytes(1024 ** 3)).toBe('1.0 GB');
    expect(formatBytes(1024 ** 4)).toBe('1.0 TB');
    expect(formatBytes(1024 ** 5)).toBe('1.0 PB');
    expect(formatBytes(1024 ** 6)).toBe('1.0 EB');
  });

  it('clamps at EB instead of returning undefined (BUG-120)', () => {
    expect(formatBytes(4096 * 1024 ** 6)).toBe('4096.0 EB');
  });

  it('promotes when a value rounds up to the next unit', () => {
    expect(formatBytes(1024 ** 2 - 1)).toBe('1024.0 KB');
    expect(formatBytes(1024 ** 3 - 1)).toBe('1024.0 MB');
  });

  it('treats non-finite and non-positive sizes as zero', () => {
    expect(formatBytes(Number.NaN)).toBe('0 B');
    expect(formatBytes(Number.POSITIVE_INFINITY)).toBe('0 B');
    expect(formatBytes(Number.NEGATIVE_INFINITY)).toBe('0 B');
    expect(formatBytes(-5)).toBe('0 B');
    expect(formatBytes(null)).toBe('0 B');
    expect(formatBytes(undefined)).toBe('0 B');
  });

  it('splits value and unit for template interpolation', () => {
    expect(formatBytesParts(1536)).toEqual({ value: '1.5', unit: 'KB' });
  });
});

describe('formatSpeed', () => {
  it('appends /s to the formatted size', () => {
    expect(formatSpeed(0)).toBe('0 B/s');
    expect(formatSpeed(1024)).toBe('1.0 KB/s');
    expect(formatSpeed(5 * 1024 ** 4)).toBe('5.0 TB/s');
    expect(formatSpeed(Number.NaN)).toBe('0 B/s');
  });
});
