use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct UserRow {
    pub id: Uuid,
    pub username: String,
    pub display_name: String,
    pub password_hash: String,
    pub role: String, // "admin" | "user"（账号级，与会话内 memberships.role 无关）
    pub disabled_at: Option<DateTime<Utc>>,
    /// 凭据版本：禁用/重置密码时 +1，旧 access token 与 WS ticket 随之失效
    pub session_version: i32,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// 头像存储名（不对外）；NULL = 未设置
    pub avatar_path: Option<String>,
    /// 每次更换/移除 +1，客户端据此失效头像缓存
    pub avatar_version: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserPublic {
    pub id: Uuid,
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub last_seen_at: Option<DateTime<Utc>>,
    /// 头像版本（拼进 GET /users/{id}/avatar?v= 做缓存失效）
    pub avatar_version: i32,
    pub has_avatar: bool,
}

impl From<&UserRow> for UserPublic {
    fn from(u: &UserRow) -> Self {
        Self {
            id: u.id,
            username: u.username.clone(),
            display_name: u.display_name.clone(),
            role: u.role.clone(),
            last_seen_at: u.last_seen_at,
            avatar_version: u.avatar_version,
            has_avatar: u.avatar_path.is_some(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AuthResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub access_expires_in: i64,
    pub user: UserPublic,
}

#[derive(Debug, Deserialize)]
pub struct RegisterReq {
    pub username: String,
    pub display_name: Option<String>,
    pub password: String,
    /// 设备名（V2 P1-5）：服务端去控制字符并截断 60 字符
    #[serde(default)]
    pub device_label: Option<String>,
    /// invite 模式必填；open/closed 忽略（带了也不校验、不计数）
    #[serde(default)]
    pub invite_code: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LoginReq {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub device_label: Option<String>,
}

/// POST /users/me/password：旧密码错误返回 403（401 会触发客户端刷新 token 流程）
#[derive(Debug, Deserialize)]
pub struct ChangePasswordReq {
    pub old_password: String,
    pub new_password: String,
}

/// GET /users/me/sessions 的单条：current 按请求 token 的 sid 判定，online = 有在线 WS 连接
#[derive(Debug, Serialize)]
pub struct SessionView {
    pub id: Uuid,
    pub device_label: String,
    pub user_agent: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub current: bool,
    pub online: bool,
}

#[derive(Debug, Deserialize)]
pub struct RefreshReq {
    pub refresh_token: String,
}

#[derive(Debug, Deserialize)]
pub struct LogoutReq {
    pub refresh_token: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateMeReq {
    pub display_name: String,
}

#[derive(Debug, Deserialize)]
pub struct FriendReqCreate {
    pub username: String,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct FriendRequestView {
    pub id: Uuid,
    pub user: UserPublic,
    pub message: String,
    pub direction: &'static str, // "in" | "out"
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct FriendView {
    pub user: UserPublic,
    pub since: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct MemberView {
    pub user: UserPublic,
    pub role: String,
    pub joined_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttachmentView {
    pub id: Uuid,
    pub file_name: String,
    pub mime: String,
    pub size: i64,
    /// 图片像素尺寸（上传时解码得到；旧附件/非图片为 None）
    pub width: Option<i32>,
    pub height: Option<i32>,
    /// 有缩略图时可用 GET /files/{id}?thumb=1
    pub has_thumb: bool,
    /// 语音时长（毫秒）；有值即语音消息
    pub duration_ms: Option<i32>,
}

/// 回复引用预览：只展开一层。deleted=true 时 excerpt 为空，仍保留 id/seq 供跳转。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyPreview {
    pub message_id: Uuid,
    pub seq: i64,
    pub sender_name: String,
    pub excerpt: String,
    pub kind: String,
    pub deleted: bool,
}

/// 单个 emoji 的聚合视图；reacted_by_me 按观看者个性化，广播时逐成员计算。
#[derive(Debug, Clone, Serialize)]
pub struct ReactionView {
    pub emoji: String,
    pub count: i64,
    pub reacted_by_me: bool,
}

/// 投票选项读模型。voters 仅非匿名投票输出（匿名恒为空数组，不落客户端）。
#[derive(Debug, Clone, Serialize)]
pub struct PollOptionView {
    pub id: Uuid,
    pub index: i32,
    pub label: String,
    pub count: i64,
    pub voters: Vec<Uuid>,
}

/// 投票卡片读模型：挂在 kind='poll' 的消息上，墓碑消息掩去。
/// my_option_ids / voters 由读模型按观看者逐人计算（§8.4）。
#[derive(Debug, Clone, Serialize)]
pub struct PollView {
    pub question: String,
    pub allow_multiple: bool,
    pub anonymous: bool,
    pub closed_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    /// 服务端按数据库 now() 计算的到期态（客户端时钟不可信）
    pub expired: bool,
    /// 投票级单调版本；真实变更与 messages.edit_version 同步 +1
    pub version: i64,
    pub total_voters: i64,
    pub options: Vec<PollOptionView>,
    /// 当前观看者已投的选项 id（匿名投票也返回本人选择）
    pub my_option_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageView {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub seq: i64,
    pub sender_id: Uuid,
    pub sender_username: String,
    pub sender_name: String,
    pub content: String,
    pub kind: String, // "text" | "image" | "file" | "audio" | "poll"
    pub attachment: Option<AttachmentView>,
    pub mentions: Vec<Uuid>, // 被 @ 的用户 id（群聊）
    pub reply_to_id: Option<Uuid>,
    pub reply_to_preview: Option<ReplyPreview>,
    pub edited_at: Option<DateTime<Utc>>,
    /// 消息级单调变更版本：编辑/软删除/reaction 实际变更均 +1
    pub edit_version: i32,
    /// 软删除时间；墓碑消息的正文/附件/提及已被读模型掩去
    pub deleted_at: Option<DateTime<Utc>>,
    pub reactions: Vec<ReactionView>,
    /// kind='poll' 时的投票卡片数据（个性化字段按观看者计算）；旧客户端忽略，看 content 占位
    pub poll: Option<PollView>,
    pub created_at: DateTime<Utc>,
    /// 发送者当前头像版本 / 是否有头像（消息行头像用，免得客户端逐人查）
    pub sender_avatar_version: i32,
    pub sender_has_avatar: bool,
    /// 转发消息：原发送者展示名快照
    pub forwarded_from: Option<String>,
    /// Agent A01：非空 = 本消息由该 Agent 发出（sender_id 仍是主人）。
    /// 客户端归属判断一律看本字段，不能只看 sender_id。
    pub agent: Option<AgentRef>,
}

/// 消息上的 Agent 归属标记（messages.agent_id → agents.id/name 快照读）。
#[derive(Debug, Clone, Serialize)]
pub struct AgentRef {
    pub id: Uuid,
    pub name: String,
}

/// 群公告（会话列表随 ConversationView 下发；清空时整字段为 null）。
#[derive(Debug, Clone, Serialize)]
pub struct AnnouncementView {
    pub text: String,
    pub updated_by_name: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
}

/// PUT /conversations/{id}/announcement：null 或空字符串表示清空。
#[derive(Debug, Deserialize)]
pub struct AnnouncementReq {
    pub text: Option<String>,
}

/// 会话偏好（用户维度，仅本人可见、同步本人全部设备）。
/// 无偏好行时全默认；updated_at 为 NULL 表示从未写入。
#[derive(Debug, Clone, Default, Serialize, sqlx::FromRow)]
pub struct ConversationPref {
    pub pinned_at: Option<DateTime<Utc>>,
    pub archived_at: Option<DateTime<Utc>>,
    /// 定时静音截止；NULL 只表示无定时静音（永久静音看 mute_forever）
    pub muted_until: Option<DateTime<Utc>>,
    pub mute_forever: bool,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct ConversationView {
    pub id: Uuid,
    pub kind: String,
    pub name: Option<String>,
    pub owner_id: Option<Uuid>,
    pub peer: Option<UserPublic>,
    pub member_count: i64,
    pub last_seq: i64,
    pub unread_count: i64,
    pub has_mention: bool, // 有未读的 @我 消息
    pub last_message: Option<MessageView>,
    /// 本人对该会话的偏好（置顶/静音/归档）；旧客户端可忽略
    pub preference: ConversationPref,
    /// 群公告；私聊 / 文件传输助手 / 未设置时为 null。旧客户端忽略
    pub announcement: Option<AnnouncementView>,
    /// Agent A01：kind='agent' 时的专属 Agent（含宿主在线态，供列表/标题用）
    pub agent: Option<ConvAgentRef>,
    pub created_at: DateTime<Utc>,
}

/// AgentConversation 上的 Agent 摘要。host_id 随帧下发，便于宿主上下线广播直接定位。
#[derive(Debug, Clone, Serialize)]
pub struct ConvAgentRef {
    pub id: Uuid,
    pub name: String,
    pub host_id: Uuid,
    pub host_online: bool,
}

/// GET /conversations/preferences 的条目：{conversation_id, preference}
/// 与 WS preference_updated 帧同构（嵌套而非扁平），便于前端同一合并路径处理。
#[derive(Debug, Serialize)]
pub struct PrefEntry {
    pub conversation_id: Uuid,
    pub preference: ConversationPref,
}

/// PUT /conversations/{id}/preferences：只更新显式字段，缺省字段保持不变；
/// JSON null 与缺省等价（null 不作为 forever）。
#[derive(Debug, Deserialize)]
pub struct PrefUpdateReq {
    pub pinned: Option<bool>,
    pub archived: Option<bool>,
    pub mute: Option<PrefMuteReq>,
}

#[derive(Debug, Deserialize)]
pub struct PrefMuteReq {
    pub mode: String, // "none" | "until" | "forever"
    pub until: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct ConversationDetail {
    pub id: Uuid,
    pub kind: String,
    pub name: Option<String>,
    pub owner_id: Option<Uuid>,
    pub members: Vec<MemberView>,
    pub last_seq: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct GroupCreateReq {
    pub name: Option<String>,
    pub member_ids: Vec<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct AddMembersReq {
    pub user_ids: Vec<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct RenameReq {
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct MessagesPage {
    pub messages: Vec<MessageView>,
    pub has_more: bool,
}

#[derive(Debug, Serialize)]
pub struct TicketRes {
    pub ticket: String,
    pub expires_in: u64,
}

#[derive(Debug, Deserialize)]
pub struct WsTicketQuery {
    pub ticket: String,
    /// Agent A01："host" = 桌面端宿主连接（不产生 presence，走 host_hello 握手）；
    /// 缺省/其它值 = 普通用户连接
    pub role: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsIn {
    SendMessage {
        conversation_id: Uuid,
        client_msg_id: Option<String>,
        content: String,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        attachment_id: Option<Uuid>,
        /// 回复目标：必须是同会话消息
        #[serde(default)]
        reply_to_id: Option<Uuid>,
    },
    EditMessage {
        message_id: Uuid,
        content: String,
        /// 乐观并发控制：必须等于当前 edit_version，否则冲突
        expected_version: i32,
        client_op_id: Option<Uuid>,
    },
    DeleteMessage {
        message_id: Uuid,
        client_op_id: Option<Uuid>,
    },
    ReactionUpsert {
        message_id: Uuid,
        emoji: String,
        client_op_id: Option<Uuid>,
    },
    ReactionRemove {
        message_id: Uuid,
        emoji: String,
        client_op_id: Option<Uuid>,
    },
    Typing {
        conversation_id: Uuid,
    },
    /// 客户端前台状态上报（V2 P3-3）：连接建立 / 可见性 / 焦点变化时发送；
    /// 服务端按「至少一条 active 连接」判定是否还有必要发离线推送
    ClientState {
        active: bool,
    },
    Read {
        conversation_id: Uuid,
        seq: i64,
    },
    Sync {
        conversation_id: Uuid,
        last_seq: i64,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsOut {
    Message {
        message: MessageView,
    },
    MessageAck {
        client_msg_id: Option<String>,
        message: MessageView,
    },
    Typing {
        conversation_id: Uuid,
        user: UserPublic,
    },
    Presence {
        user_id: Uuid,
        online: bool,
        last_seen_at: Option<DateTime<Utc>>,
    },
    PresenceInit {
        online: Vec<Uuid>,
    },
    ReadSync {
        conversation_id: Uuid,
        seq: i64,
    },
    SyncMessages {
        conversation_id: Uuid,
        messages: Vec<MessageView>,
        /// 还有更多 seq > 本页末尾 的消息待拉取（超过单页上限时逐页续拉）
        has_more: bool,
    },
    /// 编辑成功广播；reactions 已按接收者个性化，需逐成员单独成帧
    MessageUpdated {
        conversation_id: Uuid,
        message: MessageView,
        edit_version: i32,
    },
    /// 软删除广播：携带完整墓碑视图（id/seq 保留，正文/附件/reaction 已被读模型掩去），
    /// 与 message_updated/reaction_update 一样整视图按 (id, edit_version) 合并——
    /// 部分字段帧会因全局版本互相吞掉更新（反序交付丢正文）。
    MessageDeleted {
        conversation_id: Uuid,
        message: MessageView,
        edit_version: i32,
    },
    /// reaction 聚合变更：携带完整消息视图（reactions 按接收者个性化，逐成员成帧）
    ReactionUpdate {
        conversation_id: Uuid,
        message: MessageView,
        edit_version: i32,
    },
    /// 非发送类 mutation 的最小回执：与 client_msg_id 严格区分，只回发起连接
    OpAck {
        client_op_id: Uuid,
    },
    /// 会话偏好变更：只发给当前 uid 的全部连接（偏好仅本人可见，不广播给其他成员）
    PreferenceUpdated {
        conversation_id: Uuid,
        preference: ConversationPref,
    },
    Invalidate {
        what: &'static str, // "friends" | "conversations" | "requests"
    },
    /// 置顶列表变更：成员收到后重新拉 GET /conversations/{id}/pins
    PinsUpdated {
        conversation_id: Uuid,
    },
    Error {
        code: &'static str,
        message: String,
        client_msg_id: Option<String>,
        /// 非发送类 mutation 的回执关联；与 client_msg_id 不混用
        client_op_id: Option<Uuid>,
    },
    /// Agent A01：宿主上/下线，只发给宿主主人的普通连接
    AgentHostStatus {
        host_id: Uuid,
        online: bool,
    },
    /// Agent A01：调用状态推进，发给调用会话成员（A01 里即主人）的普通连接
    AgentInvocation {
        invocation: InvocationView,
    },
}

// ---------- Agent A01：宿主 / Agent / 调用 ----------

/// GET /v1/agent-hosts 的条目：只含本人的宿主
#[derive(Debug, Serialize)]
pub struct AgentHostView {
    pub id: Uuid,
    pub label: String,
    pub platform: String,
    pub online: bool,
    pub last_seen_at: DateTime<Utc>,
}

/// GET /v1/agents 与 POST /v1/agents 的视图
#[derive(Debug, Serialize)]
pub struct AgentView {
    pub id: Uuid,
    pub name: String,
    pub host_id: Uuid,
    pub host_online: bool,
    /// 运行时描述快照；A01 形状 {"command": "...", "args": ["..."]}
    pub runtime: serde_json::Value,
    /// 宿主机工作目录；空串 = 宿主决定
    pub cwd: String,
    /// 专属 AgentConversation
    pub conversation_id: Uuid,
    pub created_at: DateTime<Utc>,
}

/// POST /v1/agents 请求体
#[derive(Debug, Deserialize)]
pub struct CreateAgentReq {
    pub name: String,
    pub host_id: Uuid,
    pub runtime: AgentRuntimeReq,
    #[serde(default)]
    pub cwd: String,
}

#[derive(Debug, Deserialize)]
pub struct AgentRuntimeReq {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// agent_invocations 行的视图（WS agent_invocation 帧与历史 GET 共用形状）
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct InvocationView {
    pub id: Uuid,
    pub agent_id: Uuid,
    pub conversation_id: Uuid,
    pub trigger_message_id: Uuid,
    pub status: String,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

// ---------- 消息生命周期 REST ----------

#[derive(Debug, Deserialize)]
pub struct EditMessageReq {
    pub content: String,
    /// 必填：等于当前 edit_version 才允许写入，冲突返回 409
    pub expected_version: i32,
    pub client_op_id: Option<Uuid>,
}

/// DELETE /messages/{id} 与 reactions 端点的可选幂等键（query 参数携带；非 UUID 自动 400）
#[derive(Debug, Deserialize)]
pub struct ClientOpQuery {
    pub client_op_id: Option<Uuid>,
}

/// 重连后批量刷新本地缓存消息：按 id 回读正文/墓碑/reaction/回复预览（每批 ≤100）
#[derive(Debug, Deserialize)]
pub struct RefreshMessagesReq {
    pub ids: Vec<Uuid>,
}

// ---------- 轻量投票 REST ----------
// 投票 mutation 只走 REST（WS 不新增 mutation 帧）；同步复用现有广播：
// 创建 → message 帧，投票/关闭 → message_updated（完整个性化 MessageView）。

/// POST /conversations/{id}/polls：创建投票（仅群聊）。
/// 复用发消息事务的 seq 与 client_msg_id 幂等键；kind='poll'，content='[投票] question'。
#[derive(Debug, Deserialize)]
pub struct CreatePollReq {
    pub question: String,
    pub options: Vec<String>,
    #[serde(default)]
    pub allow_multiple: bool,
    #[serde(default)]
    pub anonymous: bool,
    pub expires_at: Option<DateTime<Utc>>,
    /// 与 send_message 同一幂等键空间（重试/重复提交收敛到同一条消息）
    pub client_msg_id: Option<String>,
    /// 契约参数（不持久化；创建幂等由 client_msg_id 承担）
    pub client_op_id: Option<Uuid>,
}

/// PUT /messages/{id}/poll/votes：替换本人选择集合（空数组 = 撤票）。
#[derive(Debug, Deserialize)]
pub struct VotePollReq {
    pub option_ids: Vec<Uuid>,
    /// 契约参数（投票本身是状态替换幂等，无需持久化 op_id）
    pub client_op_id: Option<Uuid>,
}

// ---------- 转发 / 收藏（V2 P2-4） ----------

/// POST /messages/{id}/forward：把一条消息复制到最多 10 个目标会话（去重后）。
/// 收藏 = 转发到本人的文件传输助手，客户端用同一端点。
#[derive(Debug, Deserialize)]
pub struct ForwardReq {
    /// 目标会话 id 列表：1..=10（服务端去重；空/超界 400）
    pub conversation_ids: Vec<Uuid>,
    /// 必填：发起者幂等键（同 op+同目标重试返回同一条消息，不重复广播）
    pub client_op_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct ForwardResultItem {
    pub conversation_id: Uuid,
    pub ok: bool,
    /// ok=true：转发者视角的完整消息视图（含 forwarded_from 快照）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<MessageView>,
    /// ok=false：稳定错误码（forbidden / not_friends / bad_request）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// ok=false：面向用户的中文原因（与 WS 错误帧口径一致）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_text: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ForwardResp {
    /// 与请求目标顺序一一对应；逐条独立成败（HTTP 恒 200，除非源侧统一被拒）
    pub results: Vec<ForwardResultItem>,
}

// 软删除/reaction 的 REST 响应均为发起者视角的完整 MessageView：
// 与 WS 广播同形，客户端统一按 (id, edit_version) 整视图合并。

// ---------- 后台管理（仅 admin） ----------

#[derive(Debug, Serialize)]
pub struct AdminOverview {
    pub user_count: i64,
    pub admin_count: i64,
    pub disabled_count: i64,
    pub message_count: i64,
    pub dm_count: i64,
    pub group_count: i64,
    pub attachment_count: i64,
    pub attachment_bytes: i64,
    pub online_users: i64,
    pub online_conns: i64,
}

#[derive(Debug, Serialize)]
pub struct AdminUserView {
    pub id: Uuid,
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub disabled_at: Option<DateTime<Utc>>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub online: bool,
    pub avatar_version: i32,
    pub has_avatar: bool,
    /// 该用户附件已用存储（按 storage_path 去重）
    pub storage_bytes: i64,
}

/// GET /users/me/storage（V2 P2-1）：quota_bytes 为 null 表示不限额
#[derive(Debug, Serialize)]
pub struct StorageView {
    pub used_bytes: i64,
    pub quota_bytes: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct AdminUsersQuery {
    pub q: Option<String>,
    #[serde(default)]
    pub offset: i64,
}

#[derive(Debug, Deserialize)]
pub struct SetRoleReq {
    pub role: String, // "admin" | "user"
}

#[derive(Debug, Deserialize)]
pub struct ResetPasswordReq {
    pub password: String,
}
