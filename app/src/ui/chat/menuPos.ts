/**
 * 右键菜单定位（纯函数，便于单测）。
 * 优先在点击点向右下展开；右/下出界则翻转到点击点的左/上侧；
 * 翻转后仍越界（点击点贴着对侧边缘，或菜单比视口还大）就夹紧，
 * 保证菜单边缘不越过视口四周的 margin。
 */
export function menuPos(
  x: number,
  y: number,
  w: number,
  h: number,
  vw: number,
  vh: number,
  margin = 4,
): { x: number; y: number } {
  let px = x;
  let py = y;
  if (px + w > vw - margin) px = x - w; // 右出界 → 向左翻
  if (py + h > vh - margin) py = y - h; // 下出界 → 向上翻
  // 上界 vw-margin-w 可能小于 margin（菜单比视口宽）：取 max 保证至少贴住左缘
  px = Math.min(Math.max(px, margin), Math.max(margin, vw - margin - w));
  py = Math.min(Math.max(py, margin), Math.max(margin, vh - margin - h));
  return { x: px, y: py };
}
