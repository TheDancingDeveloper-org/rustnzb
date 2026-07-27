import { readFileSync } from 'node:fs';

const [summaryPath, baselinePath] = process.argv.slice(2);
if (!summaryPath || !baselinePath) {
  throw new Error('usage: node ci/check-rust-coverage.mjs COVERAGE_JSON BASELINE');
}

const report = JSON.parse(readFileSync(summaryPath, 'utf8'));
const totals = report.data?.[0]?.totals;
const baseline = JSON.parse(readFileSync(baselinePath, 'utf8'));
const keys = ['lines', 'functions', 'regions'];
const regressions = keys.flatMap((key) => {
  const actual = totals?.[key]?.percent;
  const minimum = baseline[key];
  if (typeof actual !== 'number' || typeof minimum !== 'number') {
    return [`${key}: missing coverage metric`];
  }
  return actual + 1e-9 < minimum ? [`${key}: ${actual}% is below ${minimum}%`] : [];
});

if (regressions.length) {
  throw new Error(`Rust coverage regression:\n${regressions.join('\n')}`);
}

console.log(`Rust coverage meets baseline: ${keys.map((key) => `${key} ${totals[key].percent}%`).join(', ')}`);
