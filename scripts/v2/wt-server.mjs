// 在当前 worktree 里后台起 / 停本槽位的后端，供 Devin 浏览器实测用（非交互模式下 `cargo run` 会被拒绝）。
// 用法（worktree 根目录，环境变量已注入）：
//   node C:/Users/xie_f/projects/other/chat/scripts/v2/wt-server.mjs start   # 先 `cd app && npm run build`、`cd server && cargo build`
//   node C:/Users/xie_f/projects/other/chat/scripts/v2/wt-server.mjs stop
// 后端托管 app/dist，访问 http://127.0.0.1:$PORT；日志在 server/target/wt/server.log。
import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, openSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const WT = process.cwd();
const dir = join(WT, 'server/target/wt');
const pidFile = join(dir, 'server.pid');
mkdirSync(dir, { recursive: true });
const e = process.env;
for (const k of ['PORT', 'APP_DATABASE_URL', 'JWT_SECRET', 'FILES_DIR']) {
  if (!e[k]) throw new Error(`missing env ${k}（应由 run-devin.sh 注入）`);
}

if (process.argv[2] === 'stop') {
  if (existsSync(pidFile)) {
    try { process.kill(Number(readFileSync(pidFile, 'utf8'))); } catch {}
    rmSync(pidFile);
  }
  console.log('stopped');
} else {
  const exe = join(WT, 'server/target/debug/chat-server.exe');
  if (!existsSync(exe)) throw new Error('先在 server/ 下 cargo build');
  const out = openSync(join(dir, 'server.log'), 'a');
  const child = spawn(exe, [], {
    cwd: WT,
    env: { ...e, DATABASE_URL: e.APP_DATABASE_URL, BIND: '127.0.0.1', APP_DIST: e.APP_DIST ?? join(WT, 'app/dist') },
    stdio: ['ignore', out, out],
    detached: true,
  });
  child.unref();
  writeFileSync(pidFile, String(child.pid));
  const base = `http://127.0.0.1:${e.PORT}`;
  const deadline = Date.now() + 30000;
  let ok = false;
  while (Date.now() < deadline) {
    try { const r = await fetch(`${base}/v1/server-info`); if (r.status < 500) { ok = true; break; } } catch {}
    await new Promise((r) => setTimeout(r, 300));
  }
  console.log(ok ? `started pid ${child.pid} on ${base}` : `pid ${child.pid} started but ${base} not ready in 30s (see server/target/wt/server.log)`);
}
