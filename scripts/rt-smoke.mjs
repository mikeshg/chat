// 实时性冒烟：浏览器(无头)登录开会话 → 另一用户 WS 发消息 → 验证 DOM 即时出现气泡
import { spawn } from 'node:child_process';

const BASE = process.argv[2] || 'http://localhost:18080';
const WSB = BASE.replace(/^http/, 'ws');
const A = { u: 'rta', p: 'pass123456' };  // 接收方（浏览器）
const B = { u: 'rtb', p: 'pass123456' };  // 发送方

// --- REST 工具 ---
const api = async (t, m, p, b) => {
  const r = await fetch(BASE + '/v1' + p, { method: m, headers: { 'content-type': 'application/json', ...(t ? { authorization: `Bearer ${t}` } : {}) }, body: b && JSON.stringify(b) });
  return { s: r.status, j: await r.json().catch(() => null) };
};

// 注册/登录两个用户，互加好友
for (const x of [A, B]) {
  const r = await api(null, 'POST', '/auth/register', { username: x.u, password: x.p });
  x.t = r.j?.access_token; x.rt = r.j?.refresh_token; x.id = r.j?.user?.id;
  if (!x.t) { const l = await api(null, 'POST', '/auth/login', { username: x.u, password: x.p }); x.t = l.j.access_token; x.rt = l.j.refresh_token; x.id = l.j.user.id; }
}
const req = await api(A.t, 'POST', '/friends/requests', { username: B.u });
const reqs = await api(B.t, 'GET', '/friends/requests');
const inc = reqs.j.incoming.find(i => i.user.username === A.u);
if (inc) await api(B.t, 'POST', `/friends/requests/${inc.id}/accept`);
const convs = await api(A.t, 'GET', '/conversations');
const dm = convs.j.find(c => c.kind === 'dm');
console.log('setup ok, dm =', dm.id);

// --- B 的 WS ---
const bt = (await api(B.t, 'GET', '/ws/ticket')).j.ticket;
const bws = new WebSocket(`${WSB}/v1/ws?ticket=${bt}`);
const bFrames = [];
bws.onmessage = (e) => bFrames.push(JSON.parse(e.data));
await new Promise(r => bws.onopen = r);
console.log('B ws open');

// --- 无头 Chrome A ---
const chrome = spawn('C:/Program Files/Google/Chrome/Application/chrome.exe', [
  '--headless=new', '--remote-debugging-port=9224', '--no-first-run',
  '--user-data-dir=' + process.env.TEMP + '/rtsmoke', 'about:blank']);
await new Promise(r => setTimeout(r, 2500));
const targets = await (await fetch('http://localhost:9224/json')).json();
const ws = new WebSocket(targets.find(t => t.type === 'page').webSocketDebuggerUrl);
let id = 0; const pend = new Map(); const errors = [];
ws.onmessage = (e) => { const m = JSON.parse(e.data);
  if (m.id && pend.has(m.id)) { pend.get(m.id)(m); pend.delete(m.id); }
  if (m.method === 'Runtime.exceptionThrown') errors.push((m.params.exceptionDetails.exception?.description||'').slice(0,300));
  if (m.method === 'Runtime.consoleAPICalled' && m.params.type !== 'log') errors.push(m.params.args.map(a=>a.value??a.description).join(' ').slice(0,200));
};
const send = (me, p = {}) => new Promise(r => { const i = ++id; pend.set(i, r); ws.send(JSON.stringify({ id: i, method: me, params: p })); });
const ev = async (x) => (await send('Runtime.evaluate', { expression: x, awaitPromise: true, returnByValue: true })).result?.result?.value;
await new Promise(r => ws.onopen = r);
await send('Runtime.enable'); await send('Page.enable');
await send('Page.navigate', { url: BASE });
await new Promise(r => setTimeout(r, 2500));
await ev(`(async()=>{localStorage.setItem('chat_access','${A.t}');localStorage.setItem('chat_refresh','${A.rt}');localStorage.setItem('chat_user',JSON.stringify({id:'${A.id}',username:'${A.u}',display_name:'${A.u}',last_seen_at:null}));localStorage.setItem('chat_server_url','${BASE}');return 1})()`);
await send('Page.navigate', { url: BASE });
await new Promise(r => setTimeout(r, 3500));
console.log('page state:', await ev(`document.querySelector('.conv-list,.auth-card,.boot')?.className`));

// 打开 DM 会话
await ev(`[...document.querySelectorAll('.conv-item')].find(e=>e.textContent.includes('${B.u}'))?.click(), 'open'`);
await new Promise(r => setTimeout(r, 1200));

// B 发消息（WS）
bws.send(JSON.stringify({ type: 'send_message', conversation_id: dm.id, client_msg_id: 'rt1', content: 'realtime-probe-msg' }));
await new Promise(r => setTimeout(r, 1800));

const found = await ev(`document.body.innerText.includes('realtime-probe-msg')`);
console.log('message visible WITHOUT refresh:', found);
console.log('B frames seen:', JSON.stringify(bFrames.map(f=>f.type)).slice(0,300));
console.log('B saw A presence:', bFrames.some(f=>f.type==='presence'&&f.user_id===''+A.id&&f.online));
console.log('B got own msg ack:', bFrames.filter(f=>f.type==='message_ack').length);
await ev(`window.__wsdbg = performance.getEntriesByType('resource').filter(e=>e.name.includes('/ws')).map(e=>e.name)`);
console.log('ws resources:', await ev('JSON.stringify(window.__wsdbg)'));
console.log('=== console/exceptions ==='); errors.slice(0,8).forEach(e => console.log(' ', e)); if (!errors.length) console.log('  (none)');
chrome.kill(); process.exit(0);
