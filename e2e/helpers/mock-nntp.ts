import { ChildProcess, spawn } from 'child_process';
import * as net from 'net';
import * as path from 'path';
import * as fs from 'fs';

const PROJECT_ROOT = path.resolve(__dirname, '../..');
let instance: ChildProcess | null = null;

export async function startMockNntp(listen = '127.0.0.1:19119'): Promise<void> {
  if (instance) return;

  const binary = process.env.RUSTNZB_E2E_MOCK_BINARY
    ?? path.join(PROJECT_ROOT, 'target/debug/mock-nntp-server');
  if (!fs.existsSync(binary)) {
    throw new Error(`mock NNTP binary not found: ${binary}. Build it with cargo build --bin mock-nntp-server.`);
  }

  instance = spawn(binary, ['--listen', listen], {
    cwd: PROJECT_ROOT,
    stdio: ['ignore', 'pipe', 'pipe'],
  });

  const logDir = process.env.RUSTNZB_E2E_LOG_DIR;
  const log = logDir
    ? fs.createWriteStream(path.join(logDir, 'mock-nntp.log'), { flags: 'a' })
    : null;
  instance.stdout?.on('data', (data: Buffer) => log?.write(data));

  instance.stderr?.on('data', (data: Buffer) => {
    log?.write(data);
    process.stderr.write(`[mock-nntp] ${data.toString()}`);
  });
  instance.once('exit', () => log?.end());

  const [host, portString] = listen.split(':');
  await waitForPort(host, Number(portString), 10000);
}

export function stopMockNntp(): void {
  if (!instance) return;
  instance.kill('SIGTERM');
  instance = null;
}

async function waitForPort(host: string, port: number, timeoutMs: number): Promise<void> {
  const startedAt = Date.now();
  while (Date.now() - startedAt < timeoutMs) {
    const ready = await new Promise<boolean>((resolve) => {
      const socket = net.createConnection({ host, port }, () => {
        socket.end();
        resolve(true);
      });
      socket.on('error', () => {
        socket.destroy();
        resolve(false);
      });
    });
    if (ready) return;
    await new Promise((resolve) => setTimeout(resolve, 200));
  }

  throw new Error(`mock NNTP server did not open ${host}:${port} within ${timeoutMs}ms`);
}
