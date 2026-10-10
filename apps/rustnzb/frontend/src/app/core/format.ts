/**
 * Shared byte-size and speed formatting.
 *
 * Angular templates cannot see plain functions, so components historically
 * grew private copies of this logic. Centralising it here keeps the ladder
 * consistent across the app and fixes sizes beyond TB rendering as
 * "undefined" (BUG-120).
 */

const UNITS = ['B', 'KB', 'MB', 'GB', 'TB', 'PB', 'EB'] as const;

export interface FormattedSize {
  value: string;
  unit: string;
}

export function formatBytesParts(bytes: number | null | undefined): FormattedSize {
  const n = Number(bytes);
  if (!Number.isFinite(n) || n <= 0) {
    return { value: '0', unit: 'B' };
  }
  if (n < 1024) {
    return { value: String(Math.round(n)), unit: 'B' };
  }
  let i = Math.min(UNITS.length - 1, Math.floor(Math.log(n) / Math.log(1024)));
  return { value: (n / 1024 ** i).toFixed(1), unit: UNITS[i] };
}

export function formatBytes(bytes: number | null | undefined): string {
  const parts = formatBytesParts(bytes);
  return `${parts.value} ${parts.unit}`;
}

export function formatSpeed(bytesPerSecond: number | null | undefined): string {
  return `${formatBytes(bytesPerSecond)}/s`;
}
