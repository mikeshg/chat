import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { watchIdentity } from "./api";
import { flushDrafts } from "./store";
import { applyTheme, readThemeChoice } from "./theme";
import "./styles.css";

// 主题在 React 渲染前应用，避免先白后黑闪一下（system = 不设 data-theme，媒体查询接管）
applyTheme(readThemeChoice(localStorage));
// 页面卸载/隐藏时把在途防抖草稿立即落盘
window.addEventListener("pagehide", flushDrafts);
// 别的标签页换了账号 / 服务器 / 登出：本页的界面、缓存、WS 都属于旧身份，整页重载最干净
watchIdentity(() => location.reload());

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
