import { spawn, ChildProcess } from 'child_process';
import * as path from 'path';
import * as fs from 'fs';
// eslint-disable-next-line @typescript-eslint/no-require-imports
const Database = require('better-sqlite3');

const PROJECT_ROOT = path.resolve(__dirname, '../..');

interface BackendInstance {
  process: ChildProcess;
  port: number;
  config: string;
  dataDir: string;
  dbPath: string;
}

const instances = new Map<string, BackendInstance>();

export async function startBackend(opts: {
  name: string;
  port: number;
  config: string;
  dataDir: string;
  seedFile?: string;
}): Promise<void> {
  const { name, port, config, dataDir, seedFile } = opts;
  const absoluteDataDir = path.join(PROJECT_ROOT, dataDir);
  const dbPath = path.join(absoluteDataDir, 'rustnzb.db');

  if (fs.existsSync(absoluteDataDir)) fs.rmSync(absoluteDataDir, { recursive: true });
  fs.mkdirSync(path.join(absoluteDataDir, 'incomplete'), { recursive: true });
  fs.mkdirSync(path.join(absoluteDataDir, 'complete'), { recursive: true });
  // The backend persists API/auth changes to its config file. Always run from
  // a disposable copy so an E2E run cannot modify tracked fixtures.
  const runtimeConfig = path.join(absoluteDataDir, 'config.toml');
  fs.copyFileSync(path.join(PROJECT_ROOT, config), runtimeConfig);

  if (seedFile) {
    // Phase 1: start backend briefly so it runs migrations, then stop it.
    // The queue manager loads its in-memory state from DB at startup, so we
    // must seed the DB BEFORE the final start so seeded queue jobs are visible.
    const proc1 = spawnBackend(name, port, runtimeConfig);
    await waitForHealthy(`http://localhost:${port}/api/health`, 15000);
    // Wait for WAL checkpoint before killing
    await new Promise(r => setTimeout(r, 600));
    proc1.kill('SIGTERM');
    // Wait for process to exit and WAL to flush
    await new Promise<void>(resolve => {
      proc1.once('exit', () => setTimeout(resolve, 300));
    });

    // Seed the now-migrated DB
    const sql = fs.readFileSync(seedFile, 'utf8');
    let lastErr: unknown;
    for (let attempt = 0; attempt < 5; attempt++) {
      try {
        const db = new Database(dbPath);
        db.exec(sql);
        const count = (db.prepare('SELECT count(*) as n FROM groups').get() as { n: number }).n;
        db.close();
        if (count === 0) throw new Error(`Seeding failed for ${name}: groups table is empty`);
        lastErr = null;
        break;
      } catch (err) {
        lastErr = err;
        await new Promise(r => setTimeout(r, 500));
      }
    }
    if (lastErr) throw lastErr;

    // Phase 2: restart with seeded data — queue manager will load seeded jobs
    const proc2 = spawnBackend(name, port, runtimeConfig);
    await waitForHealthy(`http://localhost:${port}/api/health`, 15000);
    instances.set(name, { process: proc2, port, config: runtimeConfig, dataDir: absoluteDataDir, dbPath });
  } else {
    const proc = spawnBackend(name, port, runtimeConfig);
    await waitForHealthy(`http://localhost:${port}/api/health`, 15000);
    instances.set(name, { process: proc, port, config: runtimeConfig, dataDir: absoluteDataDir, dbPath });
  }
}

function spawnBackend(name: string, port: number, config: string): ChildProcess {
  const binary = process.env.RUSTNZB_E2E_BINARY ?? path.join(PROJECT_ROOT, 'target/debug/rustnzb');
  const proc = spawn(binary, ['--config', config], {
    cwd: PROJECT_ROOT,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  const logDir = process.env.RUSTNZB_E2E_LOG_DIR;
  const log = logDir
    ? fs.createWriteStream(path.join(logDir, `backend-${name}.log`), { flags: 'a' })
    : null;
  proc.stdout?.on('data', (data: Buffer) => log?.write(data));
  proc.stderr?.on('data', (data: Buffer) => {
    const msg = data.toString();
    log?.write(data);
    if (msg.includes('ERROR')) process.stderr.write(`[backend:${name}] ${msg}`);
  });
  proc.once('exit', () => log?.end());
  return proc;
}

export function stopBackend(name: string): void {
  const inst = instances.get(name);
  if (inst) {
    inst.process.kill('SIGTERM');
    instances.delete(name);
  }
}

export function stopAllBackends(): void {
  for (const [name] of instances) stopBackend(name);
}

export function cleanBackendData(name: string): void {
  const inst = instances.get(name);
  if (inst && fs.existsSync(inst.dataDir)) {
    fs.rmSync(inst.dataDir, { recursive: true });
  }
}

export function cleanAllBackendData(): void {
  for (const [name] of instances) cleanBackendData(name);
  // Also clean any leftover data dirs
  const mainDir = path.join(PROJECT_ROOT, 'e2e/test-data');
  const freshDir = path.join(PROJECT_ROOT, 'e2e/test-data-fresh');
  const mockDir = path.join(PROJECT_ROOT, 'e2e/test-data-mock');
  if (fs.existsSync(mainDir)) fs.rmSync(mainDir, { recursive: true });
  if (fs.existsSync(freshDir)) fs.rmSync(freshDir, { recursive: true });
  if (fs.existsSync(mockDir)) fs.rmSync(mockDir, { recursive: true });
}

async function waitForHealthy(url: string, timeoutMs: number): Promise<void> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const r = await fetch(url);
      if (r.ok) return;
    } catch {}
    await new Promise(r => setTimeout(r, 200));
  }
  throw new Error(`Backend not healthy within ${timeoutMs}ms: ${url}`);
}
