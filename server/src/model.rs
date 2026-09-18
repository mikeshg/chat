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
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserPublic {
    pub id: Uuid,
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub last_seen_at: Option<DateTime<Utc>>,
}

impl From<&UserRow> for UserPublic {
    fn from(u: &UserRow) -> Self {
        Self {
            id: u.id,
            username: u.username.clone(),
            display_name: u.display_name.clone(),
            role: u.role.clone(),
            last_seen_at: u.last_seen_at,
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
}

#[derive(Debug, Deserialize)]
pub struct LoginReq {
    pub username: String,
    pub password: String,
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
    pub kind: String, // "text" | "image" | "file"
    pub attachment: Option<AttachmentView>,
    pub mentions: Vec<Uuid>, // 被 @ 的用户 id（群聊）
    pub created_at: DateTime<Utc>,
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
    pub created_at: DateTime<Utc>,
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
    },
    Typing {
        conversation_id: Uuid,
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
    },
    Invalidate {
        what: &'static str, // "friends" | "conversations" | "requests"
    },
    Error {
        code: &'static str,
        message: String,
    },
}

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
