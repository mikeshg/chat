// node scripts/v2/union.mjs [--theirs-first] <file>... ：每个冲突块保留两边，兼容 CRLF。
// 默认 ours 在前；两边都往同一结构体末尾加字段、而 ours 还在后面接了新定义时用 --theirs-first。
import { readFileSync, writeFileSync } from 'node:fs';
const args = process.argv.slice(2);
const theirsFirst = args[0] === '--theirs-first';
const re = /<<<<<<< [^\r\n]*\r?\n([\s\S]*?)=======\r?\n([\s\S]*?)>>>>>>> [^\r\n]*\r?\n/g;
for (const f of args.filter((a) => !a.startsWith('--'))) {
  const s = readFileSync(f, 'utf8');
  let n = 0;
  writeFileSync(f, s.replace(re, (_, a, b) => { n++; return theirsFirst ? b + a : a + b; }));
  console.log(`${f}: ${n} block(s) unioned${theirsFirst ? ' (theirs first)' : ''}`);
}
