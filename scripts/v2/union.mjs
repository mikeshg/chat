// node union.mjs <file>... ：每个冲突块保留两边（先 ours 后 theirs），兼容 CRLF
import { readFileSync, writeFileSync } from 'node:fs';
const re = /<<<<<<< [^\r\n]*\r?\n([\s\S]*?)=======\r?\n([\s\S]*?)>>>>>>> [^\r\n]*\r?\n/g;
for (const f of process.argv.slice(2)) {
  const s = readFileSync(f, 'utf8');
  let n = 0;
  writeFileSync(f, s.replace(re, (_, a, b) => { n++; return a + b; }));
  console.log(`${f}: ${n} block(s) unioned`);
}
