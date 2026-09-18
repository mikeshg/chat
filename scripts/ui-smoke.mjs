// 无头 Chrome 驱动真实前端：登录 → 联系人搜索 → 观察控制台异常
// 用法: node scripts/ui-smoke.mjs [baseUrl] [username] [password] [searchQ]
import { spawn } from 'node:child_process';
import { mkdirSync } from 'node:fs';

const BASE = process.argv[2] || 'http://localhost:18080';
const USER = process.argv[3] || 'seeker1';
const PASS = process.argv[4] || 'pass123456';
const Q = process.argv[5] || 'win';

const chrome = spawn('C:/Program Files/Google/Chrome/Application/chrome.exe', [
  '--headless=new', '--remote-debugging-port=9223', '--no-first-run',
  '--user-data-dir=' + process.env.TEMP + '/uismoke', 'about:blank',
]);
await new Promise(r => setTimeout(r, 2500));

const targets = await (await fetch('http://localhost:9223/json')).json();
const page = targets.find(t => t.type === 'page');
const ws = new WebSocket(page.webSocketDebuggerUrl);
let id = 0;
const pending = new Map();
const errors = [];
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
  if (m.method === 'Runtime.exceptionThrown') errors.push(JSON.stringify(m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text).slice(0, 400));
  if (m.method === 'Runtime.consoleAPICalled' && m.params.type === 'error')
    errors.push(m.params.args.map(a => a.value ?? a.description).join(' ').slice(0, 300));
};
const send = (method, params = {}) => new Promise(res => {
  const i = ++id; pending.set(i, res);
  ws.send(JSON.stringify({ id: i, method, params }));
});
const evalJs = async (expr) => {
  const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
  return r.result?.result?.value;
};
await new Promise(r => ws.onopen = r);
await send('Runtime.enable'); await send('Page.enable');
await send('Page.navigate', { url: BASE });
await new Promise(r => setTimeout(r, 2500));

// 登录
await evalJs(`(async()=>{const r=await fetch('${BASE}/v1/auth/login',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({username:'${USER}',password:'${PASS}'})});const j=await r.json();localStorage.setItem('chat_access',j.access_token);localStorage.setItem('chat_refresh',j.refresh_token);localStorage.setItem('chat_user',JSON.stringify(j.user));localStorage.setItem('chat_server_url','${BASE}');return j.user.username})()`);
await send('Page.navigate', { url: BASE });
await new Promise(r => setTimeout(r, 3000));
console.log('page title/state:', await evalJs(`document.querySelector('.conv-list,.auth-card')?.className`));

// 点联系人 tab → 输入搜索词 → 点搜索
await evalJs(`[...document.querySelectorAll('.rail button,.rail *')].find(b=>b.textContent.includes('联系人'))?.click(), 'tab-clicked'`);
await new Promise(r => setTimeout(r, 1200));
console.log('contacts visible:', await evalJs(`!!document.querySelector('.contacts')`));
await evalJs(`(()=>{const i=document.querySelector('.contacts input');const setter=Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype,'value').set;setter.call(i,'${Q}');i.dispatchEvent(new Event('input',{bubbles:true}));return i.value})()`);
await evalJs(`[...document.querySelectorAll('.contacts button')].find(b=>b.textContent==='搜索')?.click(), 'searched'`);
await new Promise(r => setTimeout(r, 2000));
console.log('results rows:', await evalJs(`document.querySelectorAll('.row.found').length`));
console.log('hint:', await evalJs(`document.querySelector('.contacts .muted')?.textContent`));
console.log('page alive:', await evalJs(`!!document.body && document.body.innerHTML.length`));
console.log('=== JS ERRORS ===');
errors.forEach(e => console.log(' ', e));
if (!errors.length) console.log('  (none)');
chrome.kill();
process.exit(0);
