// 按各 worktree 的 .env.local 重建测试库容器（口令、端口不变），用于 WSL / Docker 重启后恢复。
// 用法（仓库根目录）: node scripts/devin/wt-db-up.mjs P3-1,P3-2,P3-3
// 注意：WSL 在没有前台会话时会自动关机，容器随之消失。跑并行开发期间要常驻一个保活进程：
//   wsl -d Debian -- sh -c "exec sleep 86400"   （后台运行）
import { spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const WT_ROOT = join(dirname(fileURLToPath(import.meta.url)), '..', '..', '..', 'chat-wt');
const tasks = (process.argv[2] ?? '').split(',').filter(Boolean);
tasks.forEach((task, i) => {
  const slot = i + 1;
  const env = readFileSync(join(WT_ROOT, task.toLowerCase(), '.env.local'), 'utf8');
  const m = /DATABASE_URL=postgres:\/\/chat:([^@]+)@127\.0\.0\.1:(\d+)\//.exec(env);
  if (!m) throw new Error(`${task}: DATABASE_URL not found in .env.local`);
  const [, pw, port] = m;
  const name = `chat-v2-s${slot}`;
  spawnSync('wsl', ['docker', 'rm', '-f', name], { encoding: 'utf8' });
  const r = spawnSync('wsl', ['docker', 'run', '-d', '--rm', '--pull=never', '--name', name,
    '--label', `chat.v2.slot=${slot}`, '--publish', `127.0.0.1:${port}:5432`,
    '--mount', 'type=tmpfs,destination=/var/lib/postgresql/data',
    '--env', 'POSTGRES_USER=chat', '--env', `POSTGRES_PASSWORD=${pw}`, '--env', 'POSTGRES_DB=chat',
    'postgres:17-alpine'], { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`${task}: ${r.stderr}`);
  console.log(`${task}: ${name} up on ${port}`);
});
