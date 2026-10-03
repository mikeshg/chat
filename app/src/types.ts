// 与 server/src/model.rs 的 serde 契约一一对应（字段保持 snake_case）
export interface UserPublic {
  id: string;
  username: string;
  display_name: string;
  role: "admin" | "user";
  last_seen_at: string | null;
  /** 头像版本（拼进 /users/{id}/avatar?v=）；旧服务端缺省 */
  avatar_version?: number;
  has_avatar?: boolean;
  /** webhook bot 账号（旧服务端缺省） */
  is_bot?: boolean;
}

/** GET /bots 单条：只有主人看得到 secret 和 tokens */
export interface BotView {
  id: string;
  username: string;
  display_name: string;
  /** 空串 = 不回调 */
  webhook_url: string;
  /** 回调请求的签名密钥 */
  secret: string;
  /** 入站令牌：POST /v1/hooks 带 Authorization: Bearer <token> */
  tokens: BotToken[];
  created_at: string;
}

export interface BotToken {
  id: string;
  name: string;
  token: string;
  created_at: string;
}

export interface AuthResponse {
  access_token: string;
  refresh_token: string;
  access_expires_in: number;
  user: UserPublic;
}

/** GET /users/me/sessions 的单条登录会话（V2 P1-5）：一次登录 = 一台设备 */
export interface SessionView {
  id: string;
  device_label: string;
  user_agent: string;
  created_at: string;
  /** 最近一次 refresh 换发时间 */
  last_used_at: string;
  /** 是否当前请求的 access token 所属会话 */
  current: boolean;
  /** 该会话是否有在线 WS 连接 */
  online: boolean;
}

export interface FriendRequestView {
  id: string;
  user: UserPublic;
  message: string;
  direction: "in" | "out";
  created_at: string;
}

export interface FriendView {
  user: UserPublic;
  since: string;
}

export interface MemberView {
  user: UserPublic;
  role: string;
  joined_at: string;
}

export interface AttachmentView {
  id: string;
  file_name: string;
  mime: string;
  size: number;
  /** 图片像素尺寸（旧附件/非图片缺省） */
  width?: number | null;
  height?: number | null;
  /** 有缩略图时可用 ?thumb=1 */
  has_thumb?: boolean;
  /** 语音时长（毫秒）；有值即语音消息 */
  duration_ms?: number | null;
}

/** 回复引用预览（只展开一层）；deleted=true 时 excerpt 为空，id/seq 仍可用于跳转 */
export interface ReplyPreview {
  message_id: string;
  seq: number;
  sender_name: string;
  excerpt: string;
  kind: string;
  deleted: boolean;
}

/** 单 emoji 聚合；reacted_by_me 是服务端按当前用户个性化输出的 */
export interface ReactionView {
  emoji: string;
  count: number;
  reacted_by_me: boolean;
}

/** 投票选项视图；voters 仅非匿名投票返回（匿名恒为空数组） */
export interface PollOptionView {
  id: string;
  index: number;
  label: string;
  count: number;
  voters: string[];
}

/** 投票卡片视图（kind='poll' 消息携带；个性化字段已按本人计算） */
export interface PollView {
  question: string;
  allow_multiple: boolean;
  anonymous: boolean;
  closed_at: string | null;
  expires_at: string | null;
  /** 服务端按库时间计算的到期态 */
  expired: boolean;
  /** 投票级单调版本；与消息 edit_version 同步递增 */
  version: number;
  total_voters: number;
  options: PollOptionView[];
  /** 本人已投选项 id（匿名投票也返回） */
  my_option_ids: string[];
}

/** POST /conversations/{id}/polls 请求体 */
export interface CreatePollReq {
  question: string;
  options: string[];
  allow_multiple: boolean;
  anonymous: boolean;
  expires_at?: string | null;
  /** 与 send_message 同一幂等键空间 */
  client_msg_id?: string;
}

export interface MessageView {
  id: string;
  conversation_id: string;
  seq: number;
  sender_id: string;
  sender_username: string;
  sender_name: string;
  content: string;
  kind: string; // "text" | "image" | "file" | "audio" | "poll"
  attachment: AttachmentView | null;
  mentions: string[]; // 被 @ 的用户 id（群聊）
  /** 生命周期字段（旧服务端缺省；无版本字段按 0 处理） */
  reply_to_id?: string | null;
  reply_to_preview?: ReplyPreview | null;
  edited_at?: string | null;
  /** 消息级单调变更版本：编辑/软删除/reaction 实际变更均 +1 */
  edit_version?: number;
  deleted_at?: string | null;
  reactions?: ReactionView[];
  /** kind='poll' 时的投票卡片数据（旧服务端缺省）；墓碑消息为 null */
  poll?: PollView | null;
  created_at: string;
  /** 发送者头像（消息行用）；旧服务端缺省 */
  sender_avatar_version?: number;
  sender_has_avatar?: boolean;
  /** 转发消息：原发送者展示名 */
  forwarded_from?: string | null;
}

/** 会话偏好（用户维度；旧服务端无此字段 → 视为全默认） */
export interface ConversationPref {
  pinned_at: string | null;
  archived_at: string | null;
  /** 定时静音截止（RFC3339）；null 只表示无定时静音 */
  muted_until: string | null;
  mute_forever: boolean;
  updated_at: string | null;
}

/** PUT /conversations/{id}/preferences 请求体：缺省字段不改（null 与缺省等价） */
export interface PrefUpdate {
  pinned?: boolean;
  archived?: boolean;
  mute?: { mode: "none" | "until" | "forever"; until?: string };
}

export interface ConversationView {
  id: string;
  kind: string; // "dm" | "group" | "self"（文件传输助手）
  name: string | null;
  owner_id: string | null;
  peer: UserPublic | null;
  member_count: number;
  last_seq: number;
  unread_count: number;
  has_mention: boolean; // 有未读的 @我
  last_message: MessageView | null;
  /** 本人偏好（置顶/静音/归档）；旧服务端缺省 */
  preference?: ConversationPref | null;
  /** 群公告；未设置 / 旧服务端缺省为 null */
  announcement?: AnnouncementView | null;
  created_at: string;
}

/** 群公告。清空时服务端三字段都为 null */
export interface AnnouncementView {
  text: string | null;
  updated_by_name: string | null;
  updated_at: string | null;
}

/** GET /conversations/{id}/pins 的一条 */
export interface PinEntry {
  message: MessageView;
  pinned_by_name: string | null;
  pinned_at: string;
}

export interface ConversationDetail {
  id: string;
  kind: string;
  name: string | null;
  owner_id: string | null;
  members: MemberView[];
  last_seq: number;
  created_at: string;
}

export interface MessagesPage {
  messages: MessageView[];
  has_more: boolean;
}

export interface RequestsView {
  incoming: FriendRequestView[];
  outgoing: FriendRequestView[];
}

// WS 出站帧
export type WsOut =
  | { type: "message"; message: MessageView }
  | { type: "message_ack"; client_msg_id: string | null; message: MessageView }
  | { type: "typing"; conversation_id: string; user: UserPublic }
  | { type: "presence"; user_id: string; online: boolean; last_seen_at: string | null }
  | { type: "presence_init"; online: string[] }
  | { type: "read_sync"; conversation_id: string; seq: number }
  | {
      type: "sync_messages";
      conversation_id: string;
      messages: MessageView[];
      /** 服务端分页：true 表示缺口未完，需要按新水位继续 sync（旧服务端无此字段→视为 false） */
      has_more?: boolean;
    }
  | {
      type: "message_updated";
      conversation_id: string;
      message: MessageView;
      edit_version: number;
    }
  | {
      /** 软删除广播：完整墓碑视图（正文/附件/reaction 已掩蔽，id/seq 保留） */
      type: "message_deleted";
      conversation_id: string;
      message: MessageView;
      edit_version: number;
    }
  | {
      /** reaction 变更广播：完整视图，reactions 已按本接收者个性化 */
      type: "reaction_update";
      conversation_id: string;
      message: MessageView;
      edit_version: number;
    }
  | {
      /** 会话偏好变更：服务端只发给本人全部连接 */
      type: "preference_updated";
      conversation_id: string;
      preference: ConversationPref;
    }
  | { type: "invalidate"; what: string }
  | { type: "pins_updated"; conversation_id: string }
  | { type: "force_logout"; reason: string }
  | {
      type: "error";
      code: string;
      message: string;
      client_msg_id?: string | null;
    };

// ---------- 后台管理 ----------
export interface AdminOverview {
  user_count: number;
  admin_count: number;
  disabled_count: number;
  message_count: number;
  dm_count: number;
  group_count: number;
  attachment_count: number;
  attachment_bytes: number;
  online_users: number;
  online_conns: number;
}

export interface AdminUserView {
  id: string;
  username: string;
  display_name: string;
  role: "admin" | "user";
  disabled_at: string | null;
  last_seen_at: string | null;
  created_at: string;
  online: boolean;
  avatar_version?: number;
  has_avatar?: boolean;
  /** 该用户附件已用存储（V2 P2-1；旧服务端缺省） */
  storage_bytes?: number;
}

/** GET /users/me/storage（V2 P2-1）：quota_bytes 为 null/缺省表示不限额 */
export interface StorageView {
  used_bytes: number;
  quota_bytes: number | null;
}

// ---------- V2 P1-2 注册开关 + 邀请码 ----------
export type RegistrationMode = "open" | "invite" | "closed";

/** GET /server-info（公共，旧服务端无此接口 → 客户端按 open 处理） */
export interface ServerInfo {
  registration_mode: RegistrationMode;
  /** ntfy 推送已配置（V2 P3-3；旧服务端无此字段 → 视为未配置） */
  push_available?: boolean;
  /** 视频会议地址前缀；空字符串 = 关闭按钮。旧服务端缺省 */
  meeting_base_url?: string;
}

/** GET/PUT /admin/settings */
export interface AdminSettings {
  registration_mode: RegistrationMode;
  /** 每用户附件存储上限（MB，0 = 不限；旧服务端缺省） */
  storage_quota_mb?: number;
  /** 视频会议地址；空字符串 = 关闭。旧服务端缺省 */
  meeting_base_url?: string;
  /** ntfy 服务器地址（V2 P3-3，http/https）；空 = 关闭推送 */
  push_ntfy_base?: string;
  /** ntfy 访问令牌：只写不读——PUT 提交用，GET 响应里不出现 */
  push_ntfy_token?: string;
  /** 令牌是否已设置（GET 只回这个布尔值） */
  push_ntfy_token_set?: boolean;
  /** 本服务对外地址，推送通知点击跳转用；空 = 不带 */
  public_url?: string;
  /** 在线更新的 Gitea 仓库地址（安装包在它的 Release 上）；空 = 关闭 */
  update_repo_url?: string;
}

/** 一台登记了推送的手机：各自一个 ntfy 主题 */
export interface PushDevice {
  id: string;
  name: string;
  /** {push_ntfy_base}/{topic}；available=false 时为 null */
  subscribe_url: string | null;
  created_at: string;
}

/** /users/me/push 及设备增删、测试的返回（V2 P3-3） */
export interface PushState {
  /** 服务端已配置 ntfy（管理员侧） */
  available: boolean;
  enabled: boolean;
  /** 通知正文是否带消息内容 */
  preview: boolean;
  devices: PushDevice[];
}

export interface InviteView {
  id: string;
  code: string;
  note: string;
  created_by_name: string | null;
  /** null = 不限次数 */
  max_uses: number | null;
  uses: number;
  /** null = 永不过期 */
  expires_at: string | null;
  revoked_at: string | null;
  created_at: string;
  /** 服务端按「未撤销、未过期、未用完」算出 */
  usable: boolean;
}

/** POST /admin/invites 请求体：null/缺省 = 不限 */
export interface CreateInviteReq {
  max_uses?: number | null;
  expires_in_hours?: number | null;
  note?: string;
}

// ---------- V2 P2-4 转发 / 收藏 ----------

/** POST /messages/{id}/forward 的逐目标结果（与去重后请求目标一一对应） */
export interface ForwardResultItem {
  conversation_id: string;
  ok: boolean;
  /** ok=true：转发者视角的完整消息视图（forwarded_from 为来源快照） */
  message?: MessageView;
  /** ok=false：稳定错误码（forbidden / not_friends / bad_request） */
  error?: string;
  /** ok=false：面向用户的中文原因 */
  message_text?: string;
}

export interface ForwardResp {
  results: ForwardResultItem[];
}

// ---------- V2 P4-1 备份/恢复 ----------

/** GET /admin/backups 单条成品备份 */
export interface BackupItem {
  name: string;
  size: number;
  created_at: string;
}

export interface BackupList {
  /** 有备份或恢复在进行中 */
  in_progress: boolean;
  items: BackupItem[];
}

/** POST /admin/backups → 202 */
export interface BackupCreated {
  name: string;
}

/** POST /admin/backups/{name}/download-ticket → {url} */
export interface BackupTicket {
  url: string;
}

/** POST /admin/restore → {tables, files} */
export interface RestoreResult {
  tables: string[];
  files: number;
}
