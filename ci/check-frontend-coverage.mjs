import { readFileSync } from 'node:fs';

const [summaryPath, baselinePath] = process.argv.slice(2);
if (!summaryPath || !baselinePath) {
  throw new Error('usage: node ci/check-frontend-coverage.mjs COVERAGE_SUMMARY BASELINE');
}

const summary = JSON.parse(readFileSync(summaryPath, 'utf8')).total;
const baseline = JSON.parse(readFileSync(baselinePath, 'utf8'));
const keys = ['statements', 'branches', 'functions', 'lines'];
const regressions = keys.flatMap((key) => {
  const actual = summary[key]?.pct;
  const minimum = baseline[key];
  if (typeof actual !== 'number' || typeof minimum !== 'number') {
    return [`${key}: missing coverage metric`];
  }
  return actual + 1e-9 < minimum ? [`${key}: ${actual}% is below ${minimum}%`] : [];
});

if (regressions.length) {
  throw new Error(`frontend coverage regression:\n${regressions.join('\n')}`);
}

console.log(`frontend coverage meets baseline: ${keys.map((key) => `${key} ${summary[key].pct}%`).join(', ')}`);
