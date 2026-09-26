# ---------- 前端构建 ----------
FROM node:22-alpine AS web
RUN npm i -g pnpm@11
WORKDIR /src
COPY package.json pnpm-workspace.yaml pnpm-lock.yaml ./
COPY app/package.json app/
RUN pnpm install --frozen-lockfile
COPY app ./app
RUN pnpm -F app build

# ---------- 后端构建 ----------
FROM rust:1-bookworm AS server
WORKDIR /src
COPY server/Cargo.toml server/Cargo.lock server/
# 服务端以 path 依赖引用 agent-host（共享协议类型），两边都要拷
COPY agent-host/Cargo.toml agent-host/
# 先用空壳编一遍依赖，利用层缓存
RUN mkdir server/src agent-host/src \
    && echo 'fn main(){}' > server/src/main.rs \
    && echo '' > server/src/lib.rs \
    && echo '' > agent-host/src/lib.rs \
    && cargo build --release --manifest-path server/Cargo.toml
COPY agent-host/src agent-host/src
COPY server/src server/src
COPY server/migrations server/migrations
RUN touch agent-host/src/lib.rs server/src/lib.rs server/src/main.rs \
    && cargo build --release --manifest-path server/Cargo.toml

# ---------- 运行时 ----------
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=server /src/server/target/release/chat-server ./chat-server
COPY --from=web /src/app/dist ./dist
ENV BIND=0.0.0.0 PORT=8080 APP_DIST=/app/dist
EXPOSE 8080
CMD ["./chat-server"]
