// V2 阶段验收环境：node scripts/devin/acc-env.mjs up|down
// up：起全新 tmpfs Postgres（容器 chat-v2-acc，127.0.0.1:45610），写 ../chat-wt/_runtime/acc.env（含随机口令，不进仓库）
// down：删容器
import { spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const RUNTIME = join(dirname(fileURLToPath(import.meta.url)), '..', '..', '..', 'chat-wt', '_runtime').replace(/\\/g, '/');
const NAME = 'chat-v2-acc';
spawnSync('wsl', ['docker', 'rm', '-f', NAME], { encoding: 'utf8' });
if (process.argv[2] === 'up') {
  const pw = randomBytes(18).toString('hex');
  const r = spawnSync('wsl', ['docker', 'run', '-d', '--rm', '--pull=never', '--name', NAME,
    '--label', 'chat.v2.slot=acc', '--publish', '127.0.0.1:45610:5432',
    '--mount', 'type=tmpfs,destination=/var/lib/postgresql/data',
    '--env', 'POSTGRES_USER=chat', '--env', `POSTGRES_PASSWORD=${pw}`, '--env', 'POSTGRES_DB=chat',
    'postgres:17-alpine'], { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(r.stderr);
  const files = `${RUNTIME}/acc/files`;
  mkdirSync(files, { recursive: true });
  writeFileSync(`${RUNTIME}/acc.env`, [
    `DATABASE_URL=postgres://chat:${pw}@127.0.0.1:45610/postgres`,
    `APP_DATABASE_URL=postgres://chat:${pw}@127.0.0.1:45610/chat`,
    'PORT=18110', 'CHAT_WEB_PORT=15110', 'CHAT_API=http://127.0.0.1:18110', 'BASE_URL=http://127.0.0.1:18110',
    `JWT_SECRET=${randomBytes(32).toString('hex')}`, `FILES_DIR=${files}`, 'BIND=127.0.0.1', '',
  ].join('\n'));
  console.log(`acceptance db up on 45610; env -> ${RUNTIME}/acc.env`);
} else {
  console.log('acceptance db removed');
}
