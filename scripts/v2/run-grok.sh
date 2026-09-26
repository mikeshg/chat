#!/usr/bin/env bash
# V2 并行开发：在任务 worktree 里跑 Grok Build CLI（grok-4.7-build-fast，xhigh 推理，无界面），自动续跑。
# 用法（后台运行）: bash scripts/v2/run-grok.sh <任务号> [prompt 文件]
#   prompt 文件缺省为 ../chat-wt/_runtime/devin/<任务>.prompt.md（wt-setup.mjs 生成）
# 行为：
# - .env.local 注入进程环境
# - 危险命令用 --deny 挡掉（git 提交/推送/重置等、docker、wsl、cua-driver）
# - --output-format streaming-json 持续输出进度；30 分钟无输出视为卡住，结束整棵进程树
# - 结束时没写交付文档 → 用同一 session id 续跑（最多 5 次）
set -u
PROG="${PROG:-v2}"   # 开发计划目录 docs/design/$PROG（交付文档路径）
T="$1"
TOOL="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$TOOL/../.." && pwd)"
S="$REPO/../chat-wt/_runtime/devin"
t=$(echo "$T" | tr 'A-Z' 'a-z')
WT="$REPO/../chat-wt/$t"
PROMPT="${2:-$S/$T.prompt.md}"
MODEL="${GROK_MODEL:-grok-4.7-build-fast}"
mkdir -p "$S"
cd "$WT" || exit 1
set -a; . ./.env.local; set +a
SID=$(node -e 'console.log(crypto.randomUUID())')
echo "$(date +%H:%M) grok session $SID model=$MODEL" >> "$S/$T.resume.log"

DENY=()
for r in "git commit*" "git push*" "git reset*" "git checkout*" "git restore*" "git clean*" "git stash*" \
         "git rebase*" "git branch*" "git worktree*" "git merge*" "git switch*" "cargo clean*" \
         "docker*" "wsl*" "*cua-driver*" "sudo*"; do
  DENY+=(--deny "Bash($r)")
done
COMMON=(-m "$MODEL" --reasoning-effort xhigh --output-format streaming-json "${DENY[@]}")

free_ports() {
  for port in "$PORT" "$CHAT_WEB_PORT"; do
    netstat -ano | awk -v p=":$port" '$2 ~ p"$" && $4 == "LISTENING" {print $5}' | sort -u | while read -r pid; do
      [ -n "$pid" ] && [ "$pid" != 0 ] && taskkill //PID "$pid" //T //F >/dev/null 2>&1
    done
  done
}

run_one() {
  local log="$1"; shift
  grok "$@" > "$log" 2>&1 &
  local bpid=$!
  local winpid; winpid=$(cat /proc/$bpid/winpid 2>/dev/null)
  local last_size=-1 idle=0
  while kill -0 "$bpid" 2>/dev/null; do
    sleep 20
    local size; size=$(wc -c < "$log")
    if [ "$size" = "$last_size" ]; then idle=$((idle+20)); else idle=0; last_size=$size; fi
    if [ $idle -ge 1800 ]; then
      echo "$(date +%H:%M) watchdog kill $T idle=${idle}s winpid=$winpid" >> "$S/$T.resume.log"
      [ -n "$winpid" ] && taskkill //PID "$winpid" //T //F >/dev/null 2>&1
      kill "$bpid" 2>/dev/null
      break
    fi
  done
  wait "$bpid" 2>/dev/null
  free_ports
}

CONT="$S/$T.grok-continue.md"
cat > "$CONT" <<'EOF'
继续完成你的任务单。先用 git status / git diff --stat 回顾已完成的部分，从中断处继续；全部完成后按 docs/design/__PROG__/devin-rules.md 第 5 节写交付文档并输出状态表。启动后端做浏览器实测用 `node C:/Users/xie_f/projects/other/chat/scripts/v2/wt-server.mjs start|stop`。
EOF
sed -i "s#__PROG__#$PROG#" "$CONT"

n=0
free_ports
run_one "$S/$T.grok-run$n.log" --prompt-file "$PROMPT" -s "$SID" "${COMMON[@]}"
while [ ! -f "docs/design/$PROG/delivery/$T.md" ] && [ $n -lt 5 ]; do
  n=$((n+1))
  echo "$(date +%H:%M) grok resume #$n session=$SID" >> "$S/$T.resume.log"
  run_one "$S/$T.grok-run$n.log" -r "$SID" --prompt-file "$CONT" "${COMMON[@]}"
done
echo "$T finished: runs=$((n+1)) delivery=$(ls docs/design/$PROG/delivery/$T.md 2>/dev/null || echo missing)"
