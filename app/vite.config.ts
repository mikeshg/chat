import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// dev 下把 /v1 代理到本机后端；Tauri/生产走登录页填写的服务器地址
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/v1": {
        target: "http://localhost:8080",
        changeOrigin: true,
        ws: true,
      },
    },
  },
});
