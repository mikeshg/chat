import { useLayoutEffect, useRef, useState } from "react";
import { menuPos } from "./menuPos";

/**
 * 右键菜单容器：遮罩 + 浮层。先按点击点渲染，useLayoutEffect 里量出真实尺寸后
 * 用 menuPos 翻转/夹紧到视口内（绘制前完成，不闪）。菜单内容变化（如错误条出现）
 * 会重测量并重夹紧。菜单体拦截 click：只让遮罩响应点外关闭，
 * 否则菜单内任何点击（含禁用项/错误条）都会冒泡到遮罩把菜单关掉。
 */
export default function CtxMenu(props: {
  x: number;
  y: number;
  onClose: () => void;
  children: React.ReactNode;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ x: props.x, y: props.y });
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    const p = menuPos(
      props.x,
      props.y,
      r.width,
      r.height,
      window.innerWidth,
      window.innerHeight,
    );
    setPos((cur) => (cur.x === p.x && cur.y === p.y ? cur : p));
  });
  return (
    <div
      className="ctx-mask"
      onClick={props.onClose}
      onContextMenu={(e) => e.preventDefault()}
    >
      <div
        className="ctx-menu"
        ref={ref}
        style={{ left: pos.x, top: pos.y }}
        onClick={(e) => e.stopPropagation()}
      >
        {props.children}
      </div>
    </div>
  );
}
