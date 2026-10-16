// ===== MM2 =====
// 已下线功能的保留开关：代码路径全部保留编译（tsc noUnusedLocals=false），
// 只是不再出现在消息菜单 / 悬停工具栏里，后续版本想恢复改回 true 即可。

/** 气泡悬停快捷工具栏（emoji 表态 + 回复/编辑/删除/置顶/转发/收藏） */
export const FEATURE_HOVER_TOOLBAR = false;
/** 消息菜单「表情」项（打开快捷 emoji 选择条） */
export const FEATURE_MENU_REACTION = false;
/** 消息菜单「编辑」项 */
export const FEATURE_MENU_EDIT = false;
/** 消息菜单「置顶」项 */
export const FEATURE_MENU_PIN = false;
/** 消息菜单「收藏」项 */
export const FEATURE_MENU_FAVORITE = false;
/** 消息菜单/动作面板「多选」项（MF2：多选 + 逐条/合并转发） */
export const FEATURE_MULTI_SELECT = true;
