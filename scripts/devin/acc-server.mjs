// V2 阶段验收后端：node scripts/devin/acc-server.mjs start|stop
// 读 ../chat-wt/_runtime/acc.env；二进制用 server/target/v2-acc/debug/chat-server.exe
// （先 `cd server && CARGO_TARGET_DIR=target/v2-acc cargo build`；独立 target 目录是为了不和
//   V1 遗留在 18082 上运行的 server/target/debug/chat-server.exe 抢文件锁）
import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, openSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const RUNTIME = join(REPO, '..', 'chat-wt', '_runtime');
const PIDF = join(RUNTIME, 'acc', 'server.pid');
const env = Object.fromEntries(readFileSync(join(RUNTIME, 'acc.env'), 'utf8').split('\n').filter(Boolean).map((l) => {
  const i = l.indexOf('='); return [l.slice(0, i), l.slice(i + 1)];
}));
mkdirSync(join(RUNTIME, 'acc'), { recursive: true });

if (process.argv[2] === 'stop') {
  if (existsSync(PIDF)) {
    try { process.kill(Number(readFileSync(PIDF, 'utf8'))); } catch {}
    rmSync(PIDF);
  }
  console.log('stopped');
} else {
  const out = openSync(join(RUNTIME, 'acc', 'server.log'), 'a');
  const child = spawn(join(REPO, 'server/target/v2-acc/debug/chat-server.exe'), [], {
    cwd: REPO,
    env: { ...process.env, DATABASE_URL: env.APP_DATABASE_URL, JWT_SECRET: env.JWT_SECRET, BIND: '127.0.0.1',
      PORT: env.PORT, APP_DIST: join(REPO, 'app/dist'), FILES_DIR: env.FILES_DIR },
    stdio: ['ignore', out, out], detached: true,
  });
  child.unref();
  writeFileSync(PIDF, String(child.pid));
  const deadline = Date.now() + 30000;
  while (Date.now() < deadline) {
    try { const r = await fetch(`${env.BASE_URL}/v1/server-info`); if (r.status < 500) break; } catch {}
    await new Promise((r) => setTimeout(r, 300));
  }
  console.log(`started pid ${child.pid} on ${env.BASE_URL}`);
}
