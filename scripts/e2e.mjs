// E2E 真实用户旅程测试 —— 直接打运行中的部署（默认 docker :8080）
// 用法:
//   node scripts/e2e.mjs            # phase1: 完整用户旅程
//   node scripts/e2e.mjs phase2     # phase2: 重启后持久化验证（读 .e2e-state.json）
// 环境: BASE_URL=http://localhost:8080
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const BASE = (process.env.BASE_URL || 'http://localhost:8080') + '/v1';
const WS_BASE = BASE.replace(/^http/, 'ws');
const STATE = join(dirname(fileURLToPath(import.meta.url)), '.e2e-state.json');
const PHASE = process.argv[2] || 'phase1';

let pass = 0, fail = 0;
const ok = (name, cond, extra = '') => {
  if (cond) { pass++; console.log(`  ✓ ${name}`); }
  else { fail++; console.log(`  ✗ ${name} ${extra}`); }
};

async function api(token, method, path, body) {
  const res = await fetch(BASE + path, {
    method,
    headers: {
      'content-type': 'application/json',
      ...(token ? { authorization: `Bearer ${token}` } : {}),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  let json = null;
  try { json = await res.json(); } catch {}
  return { status: res.status, json };
}

class Ws {
  constructor() { this.queue = []; this.waiters = []; }
  async connect(token) {
    const { json } = await api(token, 'GET', '/ws/ticket');
    this.ws = new WebSocket(`${WS_BASE}/ws?ticket=${json.ticket}`);
    this.ws.onmessage = (e) => {
      const m = JSON.parse(e.data);
      const i = this.waiters.findIndex((w) => w.pred(m));
      if (i >= 0) { const w = this.waiters.splice(i, 1)[0]; clearTimeout(w.t); w.res(m); }
      else this.queue.push(m);
    };
    await new Promise((res, rej) => { this.ws.onopen = res; this.ws.onerror = rej; });
  }
  send(o) { this.ws.send(JSON.stringify(o)); }
  close() { this.ws.close(); }
  waitFor(pred, ms = 8000) {
    const i = this.queue.findIndex(pred);
    if (i >= 0) return Promise.resolve(this.queue.splice(i, 1)[0]);
    return new Promise((res, rej) => {
      const t = setTimeout(() => rej(new Error('ws frame timeout')), ms);
      this.waiters.push({ pred, res, t });
    });
  }
  waitType(type, extra = () => true, ms) {
    return this.waitFor((m) => m.type === type && extra(m), ms);
  }
}

const rid = Math.random().toString(36).slice(2, 8);
const mk = (n) => ({ username: `e2e${rid}${n}`, password: 'pass123456' });

async function phase1() {
  const [A, B, C] = [mk('a'), mk('b'), mk('c')];
  console.log(`users: ${A.username} ${B.username} ${C.username}`);

  // --- 注册/登录/资料 ---
  for (const u of [A, B, C]) {
    const r = await api(null, 'POST', '/auth/register', { username: u.username, password: u.password });
    ok(`register ${u.username}`, r.status === 200 && r.json.access_token, `got ${r.status}`);
    Object.assign(u, r.json);
  }
  {
    const r = await api(null, 'POST', '/auth/login', { username: A.username, password: A.password });
    ok('login alice', r.status === 200 && r.json.user.username === A.username);
    Object.assign(A, r.json);
  }
  {
    const r = await api(A.access_token, 'PATCH', '/users/me', { display_name: 'AliceA' });
    ok('update display_name', r.status === 200 && r.json.display_name === 'AliceA');
  }
  {
    const r = await api(null, 'POST', '/auth/login', { username: A.username, password: 'wrong' });
    ok('wrong password rejected', r.status === 401 || r.status === 403);
  }

  // --- 好友流 ---
  const reqRes = await api(A.access_token, 'POST', '/friends/requests', { username: B.username, message: 'hi' });
  ok('friend request A->B', reqRes.status === 200 || reqRes.status === 201, `got ${reqRes.status}`);
  {
    const r = await api(B.access_token, 'GET', '/friends/requests');
    const incoming = (r.json.incoming || []).find((x) => x.user.username === A.username);
    ok('B sees incoming request', !!incoming);
    const acc = await api(B.access_token, 'POST', `/friends/requests/${incoming.id}/accept`);
    ok('B accepts', acc.status === 200 || acc.status === 204);
  }
  {
    const r = await api(A.access_token, 'GET', '/friends');
    ok('A friends list has B', (r.json || []).some((f) => f.user.username === B.username));
    const convs = await api(A.access_token, 'GET', '/conversations');
    const dm = (convs.json || []).find((c) => c.kind === 'dm' && c.peer?.username === B.username);
    ok('DM conversation auto-created', !!dm);
    A.dmId = dm.id; B.dmId = dm.id;
  }
  {
    const r = await api(A.access_token, 'GET', `/users/search?q=${B.username.slice(0, 6)}`);
    ok('user search partial match', r.status === 200 && r.json.some((u) => u.id === B.user.id), `got ${r.status}`);
    const r2 = await api(A.access_token, 'GET', `/users/search?q=${encodeURIComponent(B.user.display_name)}`);
    ok('user search by display_name', r2.json.some((u) => u.id === B.user.id));
  }

  // --- 非好友隔离 ---
  {
    const r = await api(C.access_token, 'GET', `/conversations/${A.dmId}/messages`);
    ok('non-member cannot read DM', r.status === 403 || r.status === 404, `got ${r.status}`);
    const d = await api(C.access_token, 'GET', `/conversations/${A.dmId}`);
    ok('non-member cannot view DM detail', d.status === 403 || d.status === 404, `got ${d.status}`);
  }

  // --- WS 连接/presence ---
  const wsa = new Ws(), wsb = new Ws();
  await wsa.connect(A.access_token);
  await wsa.waitType('presence_init');
  await wsb.connect(B.access_token);
  await wsb.waitType('presence_init');
  {
    const p = await wsa.waitType('presence', (m) => m.user_id === B.user.id && m.online === true);
    ok('A sees B online', !!p);
  }

  // --- 私聊消息/ack/typing/read ---
  wsa.send({ type: 'send_message', conversation_id: A.dmId, client_msg_id: 'm1', content: 'hi bob' });
  {
    const ack = await wsa.waitType('message_ack');
    ok('A gets message_ack seq=1', ack.message.seq === 1 && ack.client_msg_id === 'm1');
    const msg = await wsb.waitType('message', (m) => m.message.conversation_id === A.dmId);
    ok('B receives message', msg.message.content === 'hi bob');
  }
  wsb.send({ type: 'typing', conversation_id: B.dmId });
  {
    const t = await wsa.waitType('typing', (m) => m.user.id === B.user.id);
    ok('A sees B typing', !!t);
  }
  wsb.send({ type: 'read', conversation_id: B.dmId, seq: 1 });
  {
    const rs = await wsb.waitType('read_sync', (m) => m.seq === 1);
    ok('B gets read_sync', !!rs);
  }

  // --- 非成员 WS 发送被拒 ---
  const wsc = new Ws();
  await wsc.connect(C.access_token);
  await wsc.waitType('presence_init');
  wsc.send({ type: 'send_message', conversation_id: A.dmId, content: 'sneaky' });
  {
    try {
      const e = await wsc.waitType('error', () => true, 5000);
      ok('non-member WS send rejected', !!e, '');
    } catch { ok('non-member WS send rejected', false, 'no error frame'); }
  }
  wsc.close();

  // --- 群聊 ---
  const g = await api(A.access_token, 'POST', '/conversations/group', { name: '测试群', member_ids: [B.user.id] });
  ok('create group with friend', g.status === 200 || g.status === 201, `got ${g.status}`);
  const gid = g.json.id;
  {
    const bad = await api(A.access_token, 'POST', '/conversations/group', { name: 'x', member_ids: [C.user.id] });
    ok('cannot invite non-friend', bad.status === 403 || bad.status === 400, `got ${bad.status}`);
    const detail = await api(A.access_token, 'GET', `/conversations/${gid}`);
    ok('group has 2 members', detail.json.member_count === 2 || detail.json.members.length === 2);
  }
  // B 需要拿到群 conv（invalidate 或重新拉列表）
  await wsb.waitType('invalidate', () => true, 3000).catch(() => {});
  wsb.send({ type: 'send_message', conversation_id: gid, client_msg_id: 'g1', content: 'hello grp' });
  {
    const gm = await wsa.waitType('message', (m) => m.message.conversation_id === gid);
    ok('A receives group message from B', gm.message.content === 'hello grp' && gm.message.sender_id === B.user.id);
  }

  // --- @提及 ---
  wsa.send({ type: 'send_message', conversation_id: gid, client_msg_id: 'mt1', content: `hi @${B.username} 看下` });
  {
    const mm = await wsb.waitType('message', (m) => m.message.conversation_id === gid && m.message.content.includes('@'));
    ok('mention parsed to B id', mm.message.mentions.includes(B.user.id), JSON.stringify(mm.message.mentions));
  }
  {
    const convs = await api(B.access_token, 'GET', '/conversations');
    const grp = convs.json.find((c) => c.id === gid);
    ok('B has_mention flag', grp.has_mention === true, `got ${grp?.has_mention}`);
    wsb.send({ type: 'read', conversation_id: gid, seq: grp.last_seq });
    await wsb.waitType('read_sync', () => true, 4000).catch(() => {});
    const convs2 = await api(B.access_token, 'GET', '/conversations');
    ok('has_mention clears after read', convs2.json.find((c) => c.id === gid)?.has_mention === false);
  }
  {
    const h = await api(A.access_token, 'GET', `/conversations/${gid}/messages?limit=10`);
    const mmsg = h.json.messages.find((m) => m.content.includes('@'));
    ok('mentions persisted in history', mmsg?.mentions?.includes(B.user.id), JSON.stringify(mmsg?.mentions));
  }

  // --- 会话内消息搜索 ---
  {
    const r = await api(A.access_token, 'GET', `/conversations/${gid}/messages/search?q=grp`);
    ok('search finds message', r.status === 200 && r.json.some((m) => m.content === 'hello grp'), `got ${r.status}`);
    const r2 = await api(A.access_token, 'GET', `/conversations/${gid}/messages/search?q=${encodeURIComponent('@' + B.username)}`);
    ok('search finds mention text', r2.json.some((m) => m.mentions.includes(B.user.id)));
    const bad = await api(C.access_token, 'GET', `/conversations/${gid}/messages/search?q=grp`);
    ok('non-member search rejected', bad.status === 403 || bad.status === 404, `got ${bad.status}`);
  }

  // --- 离线补发 ---
  wsb.close();
  await new Promise((r) => setTimeout(r, 300));
  for (const [i, c] of ['off1', 'off2'].entries()) {
    wsa.send({ type: 'send_message', conversation_id: A.dmId, client_msg_id: `off${i}`, content: c });
  }
  await wsa.waitType('message_ack', (m) => m.message.seq === 3);
  ok('offline msgs persisted (seq 2,3)', true);
  const wsb2 = new Ws();
  await wsb2.connect(B.access_token);
  await wsb2.waitType('presence_init');
  wsb2.send({ type: 'sync', conversation_id: B.dmId, last_seq: 1 });
  {
    const s = await wsb2.waitType('sync_messages', (m) => m.conversation_id === B.dmId);
    const contents = s.messages.map((m) => m.content);
    ok('B syncs offline msgs', contents.includes('off1') && contents.includes('off2'), JSON.stringify(contents));
  }
  {
    const convs = await api(B.access_token, 'GET', '/conversations');
    const dm = convs.json.find((c) => c.id === B.dmId);
    ok('B unread_count = 2', dm.unread_count === 2, `got ${dm?.unread_count}`);
  }

  // --- 历史分页 ---
  {
    const p1 = await api(A.access_token, 'GET', `/conversations/${A.dmId}/messages?before_seq=3`);
    ok('pagination before_seq', p1.json.messages.every((m) => m.seq < 3) && p1.json.messages.length >= 2);
  }

  // --- 文件/图片消息 ---
  {
    const up = async (blob, name, type) => {
      const fd = new FormData();
      fd.append('conversation_id', A.dmId);
      fd.append('file', new Blob([blob], { type }), name);
      const r = await fetch(`${BASE}/files`, {
        method: 'POST',
        headers: { authorization: `Bearer ${A.access_token}` },
        body: fd,
      });
      return { status: r.status, json: await r.json().catch(() => null) };
    };
    // 文本文件
    const f1 = await up('hello file', 'note.txt', 'text/plain');
    ok('upload file', f1.status === 200 && f1.json.size === 10, `got ${f1.status}`);
    wsa.send({ type: 'send_message', conversation_id: A.dmId, client_msg_id: 'f1', content: '', attachment_id: f1.json.id });
    const fm = await wsa.waitType('message', (x) => x.message.attachment?.id === f1.json.id);
    ok('file msg broadcast w/ attachment', fm.message.kind === 'file' && fm.message.attachment.file_name === 'note.txt', JSON.stringify(fm.message.kind));
    ok('placeholder content', fm.message.content === '[文件] note.txt', fm.message.content);
    const dl = await fetch(`${BASE}/files/${f1.json.id}`);
    ok('download file', dl.status === 200 && (await dl.text()) === 'hello file');
    // 图片 → kind=image
    const png = Uint8Array.from([0x89,0x50,0x4e,0x47,0x0d,0x0a,0x1a,0x0a,0,0,0,0]);
    const f2 = await up(png, 'p.png', 'image/png');
    wsa.send({ type: 'send_message', conversation_id: A.dmId, client_msg_id: 'f2', content: '', attachment_id: f2.json.id });
    const im = await wsa.waitType('message', (x) => x.message.attachment?.id === f2.json.id);
    ok('image kind derived from mime', im.message.kind === 'image' && im.message.content === '[图片]', JSON.stringify(im.message.kind));
    // B 复用 A 的附件 → 拒
    wsb2.send({ type: 'send_message', conversation_id: B.dmId, client_msg_id: 'f3', content: '', attachment_id: f1.json.id });
    try {
      const e = await wsb2.waitType('error', () => true, 5000);
      ok('attachment reuse rejected', !!e);
    } catch { ok('attachment reuse rejected', false, 'no error frame'); }
    // 非成员上传 → 拒
    const fd2 = new FormData();
    fd2.append('conversation_id', A.dmId);
    fd2.append('file', new Blob(['x']), 'x.txt');
    const bad = await fetch(`${BASE}/files`, { method: 'POST', headers: { authorization: `Bearer ${C.access_token}` }, body: fd2 });
    ok('non-member upload rejected', bad.status === 403 || bad.status === 404, `got ${bad.status}`);
  }

  // --- 群主退群→转让 → 解散 ---
  await api(A.access_token, 'DELETE', `/conversations/${gid}/members/me`);
  {
    const d = await api(B.access_token, 'GET', `/conversations/${gid}`);
    ok('ownership transferred to B', d.json.owner_id === B.user.id, `owner=${d.json.owner_id}`);
    const dis = await api(B.access_token, 'DELETE', `/conversations/${gid}`);
    ok('new owner dissolves group', dis.status === 204 || dis.status === 200, `got ${dis.status}`);
  }

  // --- 删好友 → DM 只读 ---
  await api(B.access_token, 'DELETE', `/friends/${A.user.id}`);
  wsa.send({ type: 'send_message', conversation_id: A.dmId, client_msg_id: 'x', content: 'still?' });
  {
    try {
      await wsa.waitType('error', () => true, 5000);
      ok('send after unfriend rejected', true);
    } catch { ok('send after unfriend rejected', false, 'no error frame'); }
  }
  wsa.close(); wsb2.close();

  // --- 管理员后台 ---
  {
    const r = await api(A.access_token, 'GET', '/admin/overview');
    ok('non-admin gets 403 on admin api', r.status === 403);
    const r2 = await api(A.access_token, 'GET', '/admin/users');
    ok('non-admin gets 403 on admin users', r2.status === 403);
  }
  // 完整 admin 流程需要管理员凭据（环境变量门控，库里首个注册者即管理员）
  if (process.env.E2E_ADMIN_USER && process.env.E2E_ADMIN_PASS) {
    const la = await api(null, 'POST', '/auth/login', {
      username: process.env.E2E_ADMIN_USER,
      password: process.env.E2E_ADMIN_PASS,
    });
    ok('admin login', la.status === 200 && la.json.user.role === 'admin', `got ${la.status}`);
    const at = la.json.access_token;
    const ov = await api(at, 'GET', '/admin/overview');
    ok('admin overview', ov.status === 200 && ov.json.user_count >= 3, JSON.stringify(ov.json).slice(0, 120));
    const ul = await api(at, 'GET', `/admin/users?q=${B.username}`);
    ok('admin user search', ul.status === 200 && ul.json.length === 1);
    // 禁用 B：B 在线时收 force_logout → REST/登录全拒
    const wsb3 = new Ws();
    await wsb3.connect(B.access_token);
    const dis = await api(at, 'POST', `/admin/users/${B.user.id}/disable`);
    ok('disable user', dis.status === 204);
    try {
      const f = await wsb3.waitType('force_logout', () => true, 5000);
      ok('kicked via force_logout frame', f.reason === 'disabled');
    } catch { ok('kicked via force_logout frame', false, 'no frame'); }
    const dm = await api(B.access_token, 'GET', '/users/me');
    ok('disabled user rejected on REST', dm.status === 403);
    const dl = await api(null, 'POST', '/auth/login', { username: B.username, password: B.password });
    ok('disabled user cannot login', dl.status === 403);
    const en = await api(at, 'POST', `/admin/users/${B.user.id}/enable`);
    ok('enable user', en.status === 204);
    const em = await api(B.access_token, 'GET', '/users/me');
    ok('enabled user restored', em.status === 200);
    wsb3.close();
  } else {
    console.log('  (skip admin-flow asserts: set E2E_ADMIN_USER/E2E_ADMIN_PASS to enable)');
  }

  writeFileSync(STATE, JSON.stringify({ A, B, C, dmId: A.dmId }, null, 2));
  console.log(`state saved -> ${STATE}`);
}

async function phase2() {
  const s = JSON.parse(readFileSync(STATE, 'utf8'));
  const login = await api(null, 'POST', '/auth/login', { username: s.A.username, password: s.A.password });
  ok('alice still exists after restart', login.status === 200);
  const t = login.json.access_token;
  {
    const msgs = await api(t, 'GET', `/conversations/${s.dmId}/messages`);
    const contents = (msgs.json.messages || []).map((m) => m.content);
    ok('message history persisted', contents.includes('hi bob') && contents.includes('off2'), JSON.stringify(contents));
    const convs = await api(t, 'GET', '/conversations');
    const dm = convs.json.find((c) => c.id === s.dmId);
    ok('conversation metadata persisted', dm && dm.last_seq === 3, `last_seq=${dm?.last_seq}`);
  }
  const lb = await api(null, 'POST', '/auth/login', { username: s.B.username, password: s.B.password });
  const fr = await api(lb.json.access_token, 'GET', '/friends');
  ok('bob still registered', lb.status === 200);
  console.log(`  (bob friends now: ${(fr.json || []).map((f) => f.user.username).join(',') || 'none — unfriended in phase1'})`);
}

(PHASE === 'phase2' ? phase2() : phase1())
  .then(() => { console.log(`\n${pass} passed, ${fail} failed`); process.exit(fail ? 1 : 0); })
  .catch((e) => { console.error('FATAL', e); process.exit(1); });
