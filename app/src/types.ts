// 与 server/src/model.rs 的 serde 契约一一对应（字段保持 snake_case）
export interface UserPublic {
  id: string;
  username: string;
  display_name: string;
  last_seen_at: string | null;
}

export interface AuthResponse {
  access_token: string;
  refresh_token: string;
  access_expires_in: number;
  user: UserPublic;
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
}

export interface MessageView {
  id: string;
  conversation_id: string;
  seq: number;
  sender_id: string;
  sender_username: string;
  sender_name: string;
  content: string;
  kind: string; // "text" | "image" | "file"
  attachment: AttachmentView | null;
  created_at: string;
}

export interface ConversationView {
  id: string;
  kind: string; // "dm" | "group"
  name: string | null;
  owner_id: string | null;
  peer: UserPublic | null;
  member_count: number;
  last_seq: number;
  unread_count: number;
  last_message: MessageView | null;
  created_at: string;
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
  | { type: "sync_messages"; conversation_id: string; messages: MessageView[] }
  | { type: "invalidate"; what: string }
  | { type: "error"; code: string; message: string };
