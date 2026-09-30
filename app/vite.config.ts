import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// dev 下把 /v1 代理到本机后端；Tauri/生产走登录页填写的服务器地址
// CHAT_API / CHAT_WEB_PORT 供并行 worktree 各用各的后端与端口
export default defineConfig({
  plugins: [react()],
  server: {
    port: Number(process.env.CHAT_WEB_PORT ?? 5173),
    // Windows 上 Node 默认只监听 ::1，走 127.0.0.1 的浏览器（如 Claude 预览面板）连不上
    host: "127.0.0.1",
    proxy: {
      "/v1": {
        target: process.env.CHAT_API ?? "http://localhost:28080",
        changeOrigin: true,
        ws: true,
      },
    },
  },
});
