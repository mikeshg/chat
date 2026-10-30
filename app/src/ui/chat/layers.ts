// ===== 浮层叠放顺序（z-index 唯一事实来源；MF2-fix1）=====
// DOM 序不保证层叠（转发弹窗在 JSX 里先于查看器/查看页渲染，同 z-index
// 时会被后渲染的查看器盖住），层级必须显式给值。Esc/返回键的关闭顺序
// 与这里一致——最上层最先关：
// ForwardModal → 查看器/预览 → 聊天记录查看页（逐层）→ 多选 → 会话
// （编排见 ChatView 的 keydown 捕获与 backStack.ts）。
export const Z = {
  /** 聊天记录查看页遮罩（.rv-mask）：低于一切弹窗，记录内打开的浮层都压在它上面 */
  recordView: 90,
  /** 通用弹窗层（.modal-mask）：附件预览/大图查看器与其余弹窗同级 */
  modal: 100,
  /** 转发弹窗：必须高过查看器/预览——记录内 viewer-forward 不关预览，弹窗要可交互 */
  forwardModal: 110,
  /** 右键菜单遮罩 / 菜单本体（CtxMenu） */
  ctxMask: 200,
  ctxMenu: 201,
  /** 触屏动作面板 / 应用内 toast */
  sheet: 300,
  toast: 300,
  /** 轻量操作反馈（已复制/撤回提示） */
  hint: 400,
  /** 应用内确认/提示弹窗（DialogHost，替代 native confirm/alert）：最上层，压住一切浮层 */
  dialog: 500,
} as const;
