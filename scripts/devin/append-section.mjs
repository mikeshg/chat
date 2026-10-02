// node scripts/devin/append-section.mjs <branch> <任务号> <file>   例：node scripts/devin/append-section.mjs v2-p2-2 P2-2 server/tests/it.rs
// 解决「两边都在文件末尾追加」的冲突：取 HEAD 版本，再把 <branch> 里从 `===== V2 <任务号> =====` 分节行开始到文件末尾的整段追加上去。
// 只传任务号（不要传以 / 开头的注释标记：Git Bash 会把它当路径改写）。
// 只在确认该分支对这个文件的改动全部是末尾追加时使用（git diff <base> <branch> -- <file> 只有一个 @@ 在文件尾）。
import { execSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';
const [branch, task, file] = process.argv.slice(2);
const show = (ref) => execSync(`git show ${ref}:${file}`, { encoding: 'utf8', maxBuffer: 1 << 27 });
const ours = show('HEAD');
const theirs = show(branch);
const prefix = process.env.SECTION_PREFIX ?? "V2"; // Agent 票用 SECTION_PREFIX=Agent
const hit = theirs.search(new RegExp(`===== ${prefix} ${task}(?![0-9A-Za-z])`)); // 允许标记后带说明文字
if (hit < 0) throw new Error(`section ${prefix} ${task} not found in ${branch}:${file}`);
const i = theirs.lastIndexOf('\n', hit) + 1; // 分节注释所在行的行首
const nl = ours.includes('\r\n') ? '\r\n' : '\n';
const add = theirs.slice(i).replace(/\r?\n/g, nl);
writeFileSync(file, ours.replace(/\s*$/, '') + nl + nl + add);
console.log(`${file}: HEAD + ${branch} section (${add.split(nl).length} lines)`);
