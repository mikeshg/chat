// ===== MF2 Android 返回键覆盖层栈 =====
// 打开会话只有一条 history 条目（{chat:1}）；覆盖层不再各自 pushState，
// 而是把「关掉自己」注册进本栈。返回键命中时由 ChatScreen 的 popstate
// 调 consumeBack() 弹最顶层并补回 {chat:1}，逐层关闭：
// ForwardModal → 查看器/预览 → 聊天记录查看页（每层）→ 多选模式 → 会话。
// Esc 键走 ChatView 里的同一优先级编排（与这里顺序一致）。

type Layer = { close: () => void };
const stack: Layer[] = [];

/** 入栈一层覆盖物；返回 unregister()（UI 路径关闭时调用，防止栈里留尸体） */
export function pushBackLayer(close: () => void): () => void {
  const l: Layer = { close };
  stack.push(l);
  return () => {
    const i = stack.indexOf(l);
    if (i >= 0) stack.splice(i, 1);
  };
}

/** 按了一次返回键：关最上层；true=已消费（调用方应补回 {chat:1} 让会话保持打开） */
export function consumeBack(): boolean {
  const l = stack.pop();
  if (!l) return false;
  l.close();
  return true;
}

/** 会话切换/登出兜底清空（组件 effect 的 unregister 正常已足够） */
export function resetBackLayers() {
  stack.length = 0;
}

/** 仅测试用 */
export const backLayerDepth = () => stack.length;
