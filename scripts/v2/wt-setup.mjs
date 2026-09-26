// V2 并行开发：为一个阶段的任务建 worktree + 独立测试库 + 环境 + Devin 权限 + 启动提示
// 用法（仓库根目录）: node scripts/v2/wt-setup.mjs P2-1,P2-2,P2-3,P2-4
// 其他开发计划：PROG=agent BASE=main node scripts/v2/wt-setup.mjs A01（任务单在 docs/design/$PROG/tasks，分支 $PROG-<task> 基于 $BASE）
// 每个任务：分支 v2-<task>（基于当前 v2 头）、worktree ../chat-wt/<task>、tmpfs Postgres 容器 chat-v2-s<槽位>、
// .env.local（随机口令，gitignored）、.devin/config.local.json（Exec 白名单）、pnpm 离线安装、
// ../chat-wt/_runtime/devin/<task>.prompt.md（由 prompt.template.md 生成）。
import { spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const TOOL = dirname(fileURLToPath(import.meta.url));
const REPO = join(TOOL, '..', '..');
const WT_ROOT = join(REPO, '..', 'chat-wt');
const RUNTIME = join(WT_ROOT, '_runtime');
const tasks = (process.argv[2] ?? '').split(',').filter(Boolean);
const PROG = process.env.PROG ?? 'v2';
const BASE = process.env.BASE ?? 'v2';
const DOCS = `docs/design/${PROG}`;
if (!tasks.length) throw new Error('usage: node scripts/v2/wt-setup.mjs P2-1,P2-2,...');

const run = (cmd, args, opts = {}) => {
  const r = spawnSync(cmd, args, { encoding: 'utf8', ...opts });
  if (r.status !== 0) throw new Error(`${cmd} ${args.join(' ')} failed: ${r.stderr || r.stdout}`);
  return r.stdout.trim();
};

const allow = [
  'Read(**)', 'Write(**)',
  'Exec(cargo)', 'Exec(rustup)', 'Exec(rustc)', 'Exec(pnpm)', 'Exec(npm)', 'Exec(npx)', 'Exec(node)',
  'Exec(git status)', 'Exec(git diff)', 'Exec(git log)', 'Exec(git show)', 'Exec(git add)', 'Exec(git ls-files)', 'Exec(git grep)',
  'Exec(ls)', 'Exec(cat)', 'Exec(grep)', 'Exec(rg)', 'Exec(find)', 'Exec(head)', 'Exec(tail)', 'Exec(wc)', 'Exec(sed)',
  'Exec(mkdir)', 'Exec(cp)', 'Exec(mv)', 'Exec(touch)', 'Exec(echo)', 'Exec(pwd)', 'Exec(which)', 'Exec(sort)', 'Exec(diff)',
  'Exec(curl)', 'Exec(sleep)', 'Exec(taskkill)', 'Exec(tasklist)', 'Exec(netstat)',
];
const deny = [
  'Exec(git commit)', 'Exec(git push)', 'Exec(git reset)', 'Exec(git checkout)', 'Exec(git restore)', 'Exec(git clean)',
  'Exec(git stash)', 'Exec(git rebase)', 'Exec(git branch)', 'Exec(git worktree)', 'Exec(git merge)', 'Exec(git switch)',
  'Exec(cargo clean)', 'Exec(docker)', 'Exec(wsl)', 'Exec(wsl.exe)', 'Exec(sudo)',
];
const template = readFileSync(join(TOOL, 'prompt.template.md'), 'utf8');
const specs = readdirSync(join(REPO, DOCS, 'tasks'));

mkdirSync(join(RUNTIME, 'devin'), { recursive: true });
tasks.forEach((task, i) => {
  const slot = Number(process.env.SLOT_BASE ?? 0) + i + 1; // SLOT_BASE：避开仍在运行的槽位
  const t = task.toLowerCase();
  const wt = join(WT_ROOT, t);
  const spec = specs.find((f) => f.startsWith(`${task}-`));
  if (!spec) throw new Error(`no spec ${DOCS}/tasks/${task}-*.md`);
  if (existsSync(wt)) throw new Error(`worktree exists: ${wt}`);
  run('git', ['-C', REPO, 'worktree', 'add', '-b', `${PROG}-${t}`, wt, BASE]);

  const container = `chat-v2-s${slot}`;
  const pgPort = 45600 + slot; // 避开 Windows 临时端口段 49152–65535（会被随机进程占用）
  const pw = randomBytes(18).toString('hex');
  spawnSync('wsl', ['docker', 'rm', '-f', container], { encoding: 'utf8' });
  run('wsl', ['docker', 'run', '-d', '--rm', '--pull=never', '--name', container,
    '--label', `chat.v2.slot=${slot}`, '--publish', `127.0.0.1:${pgPort}:5432`,
    '--mount', 'type=tmpfs,destination=/var/lib/postgresql/data',
    '--env', 'POSTGRES_USER=chat', '--env', `POSTGRES_PASSWORD=${pw}`, '--env', 'POSTGRES_DB=chat',
    'postgres:17-alpine']);

  const fwd = (p) => p.replace(/\\/g, '/');
  const files = fwd(join(wt, 'server/target/wt/files'));
  mkdirSync(files, { recursive: true });
  writeFileSync(join(wt, '.env.local'), [
    `DATABASE_URL=postgres://chat:${pw}@127.0.0.1:${pgPort}/postgres`,
    `APP_DATABASE_URL=postgres://chat:${pw}@127.0.0.1:${pgPort}/chat`,
    `PORT=${18100 + slot}`,
    `CHAT_WEB_PORT=${15100 + slot}`,
    `CHAT_API=http://127.0.0.1:${18100 + slot}`,
    `BASE_URL=http://127.0.0.1:${18100 + slot}`,
    `JWT_SECRET=${randomBytes(32).toString('hex')}`,
    `FILES_DIR=${files}`,
    `APP_DIST=${fwd(join(wt, 'app/dist'))}`,
    '',
  ].join('\n'));
  mkdirSync(join(wt, '.devin'), { recursive: true });
  writeFileSync(join(wt, '.devin/config.local.json'), JSON.stringify({ permissions: { allow, deny } }, null, 2));
  run('pnpm', ['install', '--frozen-lockfile', '--offline'], { cwd: wt, shell: true });
  writeFileSync(join(RUNTIME, 'devin', `${task}.prompt.md`),
    template.replaceAll('{TASK}', task).replaceAll('{SPEC}', `${DOCS}/tasks/${spec}`).replaceAll('{DOCS}', DOCS));
  console.log(`${task}: slot ${slot} wt=${wt} branch=${PROG}-${t} pg=${pgPort} api=${18100 + slot} spec=${spec}`);
});
