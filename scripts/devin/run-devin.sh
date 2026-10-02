#!/usr/bin/env bash
# V2 并行开发：在任务 worktree 里跑一个 Devin（swe-2-max，非交互），自动续跑。
# 用法（后台运行）: bash scripts/devin/run-devin.sh <任务号> <new|resume>
#   new    = 用 ../chat-wt/_runtime/devin/<任务>.prompt.md 新开会话（wt-setup.mjs 生成）
#   resume = 按会话名续跑该 worktree 最近一次的 Devin 会话
# 行为：
# - .env.local 注入进程环境（Devin 不需要也不能去读它，读含口令文件会触发强制确认）
# - 被拒 / 结束时没写交付文档 → -r 续跑（最多 15 次）
# - 模型容量不足（capacity issues，可重试）→ 等 3 分钟再试（单独计数，最多 40 次 ≈ 2 小时）
# - 账户当日额度用完（usage quota has been exhausted）→ 直接停
# - 看门狗：被拒后 60s 无新输出、或 10 分钟无新输出 → 结束整棵进程树（Devin 起的后台 vite/后端会让进程挂住）
# - 每轮结束释放本槽位端口；日志与续跑记录在 ../chat-wt/_runtime/devin/
set -u
PROG="${PROG:-v2}"   # 开发计划目录 docs/archive/$PROG（交付文档路径）
T="$1"; MODE="$2"
TOOL="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$TOOL/../.." && pwd)"
S="$REPO/../chat-wt/_runtime/devin"
LOGS="$(cygpath -u "$APPDATA")/devin/cli/logs"
t=$(echo "$T" | tr 'A-Z' 'a-z')
WT="$REPO/../chat-wt/$t"
MODEL="${DEVIN_MODEL:-swe-2-max}"
mkdir -p "$S"
cd "$WT" || exit 1
set -a; . ./.env.local; set +a
COMMON=(--model "$MODEL" --permission-mode smart --respect-workspace-trust false)
CONT="$TOOL/continue.prompt.md"

# 该 worktree 最近一次 Devin 会话名（日志里 cwd 匹配 + "for session <name>"）
sid() {
  local f
  f=$(cd "$LOGS" && grep -lF -- 'chat-wt\'"$t"' workspace_root' devin_*.log 2>/dev/null | xargs -r ls -t | head -1)
  [ -n "$f" ] && grep -o "for session [a-z0-9-]*" "$LOGS/$f" | tail -1 | awk '{print $3}'
}

# 释放本槽位的后端/前端端口（只杀监听这两个端口的进程）
free_ports() {
  for port in "$PORT" "$CHAT_WEB_PORT"; do
    netstat -ano | awk -v p=":$port" '$2 ~ p"$" && $4 == "LISTENING" {print $5}' | sort -u | while read -r pid; do
      [ -n "$pid" ] && [ "$pid" != 0 ] && taskkill //PID "$pid" //T //F >/dev/null 2>&1
    done
  done
}

run_one() {
  local log="$1"; shift
  devin "$@" > "$log" 2>&1 &
  local bpid=$!
  local winpid; winpid=$(cat /proc/$bpid/winpid 2>/dev/null)
  local last_size=-1 idle=0
  while kill -0 "$bpid" 2>/dev/null; do
    sleep 20
    local size; size=$(wc -c < "$log")
    if [ "$size" = "$last_size" ]; then idle=$((idle+20)); else idle=0; last_size=$size; fi
    if { grep -q "rejected a tool call" "$log" && [ $idle -ge 60 ]; } || [ $idle -ge 600 ]; then
      echo "$(date +%H:%M) watchdog kill $T idle=${idle}s winpid=$winpid" >> "$S/$T.resume.log"
      [ -n "$winpid" ] && taskkill //PID "$winpid" //T //F >/dev/null 2>&1
      kill "$bpid" 2>/dev/null
      break
    fi
  done
  wait "$bpid" 2>/dev/null
  free_ports
}

n=0; rej=0; cap=0; mode="$MODE"
free_ports
while :; do
  log="$S/$T.run$n.log"
  if [ "$mode" = new ]; then
    run_one "$log" --prompt-file "$S/$T.prompt.md" "${COMMON[@]}" -p
  else
    run_one "$log" -r "$(sid)" --prompt-file "$CONT" "${COMMON[@]}" -p
  fi
  n=$((n+1))
  # 已经建过会话就一律续跑（保留上下文）
  [ -n "$(sid)" ] && mode=resume
  # 账户当日额度用完：重试没有意义，直接停（额度要用户自己在 Devin 后台处理）
  if grep -q "usage quota has been exhausted" "$log"; then
    echo "$(date +%H:%M) $T quota exhausted: stopping" >> "$S/$T.resume.log"
    break
  fi
  if grep -q "capacity issues" "$log"; then
    cap=$((cap+1)); [ $cap -ge 40 ] && { echo "$(date +%H:%M) $T capacity: giving up" >> "$S/$T.resume.log"; break; }
    echo "$(date +%H:%M) $T capacity backoff #$cap" >> "$S/$T.resume.log"
    sleep 180
    continue
  fi
  if grep -q "rejected a tool call" "$log" || [ ! -f "docs/archive/$PROG/delivery/$T.md" ]; then
    rej=$((rej+1)); [ $rej -ge 15 ] && break
    echo "$(date +%H:%M) resume #$rej session=$(sid)" >> "$S/$T.resume.log"
    continue
  fi
  break
done
echo "$T finished: runs=$n rejects=$rej capacity=$cap delivery=$(ls docs/archive/$PROG/delivery/$T.md 2>/dev/null || echo missing)"
