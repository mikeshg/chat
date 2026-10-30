#!/usr/bin/env bash
# 一键启动 / 更新 Chat 服务端（docker compose，WSL/任意有 Docker 的环境）
# - 本项目还没有任何 compose 容器（含已停止）：首次启动，直接 up -d --build
# - 已有容器：视为更新，要求当前在 main 分支且工作区干净，
#   先 git pull --ff-only origin main 再 up -d --build
# - 缺 .env 时自动生成 JWT_SECRET（已有 .env 不会被覆盖）
set -euo pipefail

# 无论从哪里调用，都回到仓库根目录（compose 文件与 .env 都在这里）
cd "$(dirname "${BASH_SOURCE[0]}")/.."

err() { echo "错误：$*" >&2; exit 1; }

command -v docker >/dev/null 2>&1 || err "未找到 docker 命令，请先安装 Docker（需要支持 docker compose v2 的版本）。"
docker compose version >/dev/null 2>&1 || err "当前 Docker 不支持 docker compose（v2 插件），请升级 Docker 或安装 compose 插件。"

if [ ! -f .env ]; then
  command -v openssl >/dev/null 2>&1 || err "缺少 .env 且未找到 openssl，无法生成 JWT_SECRET；请手动创建 .env（参考 .env.example）。"
  echo "JWT_SECRET=$(openssl rand -hex 32)" > .env
  chmod 600 .env
  echo "提示：未找到 .env，已自动生成随机 JWT_SECRET 写入 .env（已有 .env 不会被覆盖，此提示只出现一次）。"
fi

containers="$(docker compose ps -a -q)" || err "docker compose ps 失败：请确认 Docker 守护进程已启动。"
if [ -n "$containers" ]; then
  # 更新路径：先校验环境再 pull，避免在非 main / 脏工作区上做合并
  command -v git >/dev/null 2>&1 || err "未找到 git 命令，无法自动更新；请安装 git 或手动更新代码。"
  branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null)" || err "当前目录不是 git 仓库，无法自动更新；请手动拉取代码后重试。"
  [ "$branch" = "main" ] || err "当前分支是 $branch，自动更新要求先切换到 main 分支。"
  [ -z "$(git status --porcelain)" ] || err "工作区有未提交或未跟踪的改动，请先提交或清理后再更新。"
  git pull --ff-only origin main || err "git pull 失败：无法快进合并到 origin/main，请手动处理后再试。"
  echo "代码已更新到 main 最新，开始重建并启动…"
else
  echo "首次启动：未发现本项目的 compose 容器，开始构建…"
fi

docker compose up -d --build

echo
echo "启动完成：浏览器打开 http://localhost:28080"
echo "公网部署地址：https://chat.287152.xyz"
