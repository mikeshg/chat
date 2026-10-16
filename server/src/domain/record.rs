use super::*;
use crate::model::{RecordPreview, RecordPreviewItem};
use serde_json::{json, Value};
use std::collections::HashMap;

// ---------- 聊天记录（chat_record）快照助手：纯 jsonb 操作，不进库 ----------
//
// 快照结构（设计 2.1，落 messages.record jsonb）：
// { v:1, title, source_kind:"dm"|"group"|"self", count,
//   items:[{ sender_id, sender_name, has_avatar, avatar_version, created_at,
//            kind:"text"|"image"|"file"|"audio"|"chat_record", content,
//            attachment:{...}|null, record:{...}|null }] }
//
// 嵌套记录的 attachment.id 指向「与宿主记录同会话的克隆行」；跨会话复制时按
// collect_attachment_ids → clone_record_attachment → rewrite_attachment_ids 三步走，
// 克隆行 record_message_id 一律指向最外层那条 chat_record 消息。

/// 快照总嵌套高度：无嵌套 = 1；否则 1 + 最深子记录高度。按 jsonb 现算，不加列。
pub fn record_height(record: &Value) -> u32 {
    fn h(v: &Value) -> u32 {
        let mut max_child = 0;
        if let Some(items) = v.get("items").and_then(|i| i.as_array()) {
            for it in items {
                if let Some(r) = it.get("record") {
                    if !r.is_null() {
                        max_child = max_child.max(h(r));
                    }
                }
            }
        }
        1 + max_child
    }
    h(record)
}

/// 递归收集快照里全部 attachment.id（含嵌套）。
pub fn collect_attachment_ids(record: &Value, out: &mut Vec<Uuid>) {
    if let Some(items) = record.get("items").and_then(|i| i.as_array()) {
        for it in items {
            if let Some(id) = it
                .get("attachment")
                .and_then(|a| a.get("id"))
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok())
            {
                out.push(id);
            }
            if let Some(r) = it.get("record") {
                if !r.is_null() {
                    collect_attachment_ids(r, out);
                }
            }
        }
    }
}

/// 按 旧id→新id 映射重写快照里的全部 attachment.id（含嵌套）。
pub fn rewrite_attachment_ids(record: &mut Value, map: &HashMap<Uuid, Uuid>) {
    if let Some(items) = record.get_mut("items").and_then(|i| i.as_array_mut()) {
        for it in items {
            if let Some(att) = it.get_mut("attachment") {
                if !att.is_null() {
                    if let Some(new) = att
                        .get("id")
                        .and_then(|v| v.as_str())
                        .and_then(|s| Uuid::parse_str(s).ok())
                        .and_then(|old| map.get(&old))
                    {
                        att["id"] = json!(new.to_string());
                    }
                }
            }
            if let Some(r) = it.get_mut("record") {
                if !r.is_null() {
                    rewrite_attachment_ids(r, map);
                }
            }
        }
    }
}

/// 嵌套层总条数（含顶层 items 与各层嵌套 items；用于 DoS 上限）
pub fn count_items(record: &Value) -> usize {
    let mut n = 0;
    if let Some(items) = record.get("items").and_then(|i| i.as_array()) {
        for it in items {
            n += 1;
            if let Some(r) = it.get("record") {
                if !r.is_null() {
                    n += count_items(r);
                }
            }
        }
    }
    n
}

/// 记录快照标题（损坏快照给空串）
pub fn record_title(record: &Value) -> String {
    record
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string()
}

/// 在快照各层里找 attachment.id == att 的条目，返回其 (kind, sender_name, content)。
/// 记录内附件单独转发用；找不到 = 条目已被改写/数据不一致 → 调用方按 404 处理。
pub fn find_item_by_attachment(
    record: &Value,
    att: Uuid,
) -> Option<(String, String, String)> {
    let items = record.get("items")?.as_array()?;
    for it in items {
        let hit = it
            .get("attachment")
            .and_then(|a| a.get("id"))
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
            == Some(att);
        if hit {
            let kind = it.get("kind").and_then(|k| k.as_str())?.to_string();
            let sender = it
                .get("sender_name")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string();
            let content = it
                .get("content")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string();
            return Some((kind, sender, content));
        }
        if let Some(r) = it.get("record") {
            if !r.is_null() {
                if let Some(found) = find_item_by_attachment(r, att) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// 预览单条的摘要文本：text 用正文截 60 字；附件类用占位标签（含文件名），
/// 附件缺失/已撤回的条目退回正文（caption 或「[已撤回的附件]」）。
fn item_excerpt(it: &Value) -> String {
    let kind = it.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    let content = it.get("content").and_then(|c| c.as_str()).unwrap_or("");
    let att = it.get("attachment").filter(|a| !a.is_null());
    let text = match kind {
        "chat_record" => format!(
            "[聊天记录] {}",
            it.get("record").map(record_title).unwrap_or_default()
        ),
        "image" if att.is_some() => "[图片]".to_string(),
        "audio" if att.is_some() => "[语音]".to_string(),
        "file" if att.is_some() => format!(
            "[文件] {}",
            att.and_then(|a| a.get("file_name"))
                .and_then(|f| f.as_str())
                .unwrap_or_default()
        ),
        _ => content.to_string(),
    };
    text.chars().take(60).collect()
}

/// record jsonb → 列表/WS 卡片预览（前 4 条；永不含附件 id）。损坏快照返回 None。
pub fn record_preview(record: &Value) -> Option<RecordPreview> {
    let title = record.get("title")?.as_str()?;
    let count = record.get("count").and_then(|c| c.as_i64()).unwrap_or(0);
    let items = record
        .get("items")
        .and_then(|i| i.as_array())
        .map(|a| {
            a.iter()
                .take(4)
                .map(|it| RecordPreviewItem {
                    sender_name: it
                        .get("sender_name")
                        .and_then(|s| s.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    kind: it
                        .get("kind")
                        .and_then(|k| k.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    text: item_excerpt(it),
                })
                .collect()
        })
        .unwrap_or_default();
    Some(RecordPreview {
        title: title.to_string(),
        count,
        items,
    })
}

/// 快照条目（建外层 record 用）：字段与设计 2.1 一致，sender_name ≤50 字、content ≤4000 字。
#[allow(clippy::too_many_arguments)]
pub fn snapshot_item(
    sender_id: Uuid,
    sender_name: &str,
    has_avatar: bool,
    avatar_version: i32,
    created_at: DateTime<Utc>,
    kind: &str,
    content: &str,
    attachment: Value,
    record: Value,
) -> Value {
    json!({
        "sender_id": sender_id,
        "sender_name": sender_name.chars().take(50).collect::<String>(),
        "has_avatar": has_avatar,
        "avatar_version": avatar_version,
        "created_at": created_at,
        "kind": kind,
        "content": content.chars().take(4000).collect::<String>(),
        "attachment": attachment,
        "record": record,
    })
}

/// 兜底文本（设计 2.8）：首行「[聊天记录] {title}」，中间每行「发送者：摘要」，
/// 末行恒为「…（共 N 条，请升级客户端查看）」。总长 ≤4000 字，超长时末条截断并始终带尾行。
pub fn fallback_text(title: &str, items: &[Value], count: usize) -> String {
    const LIMIT: usize = 4000;
    let tail = format!("…（共 {count} 条，请升级客户端查看）");
    let mut out = format!("[聊天记录] {title}");
    // 留够 "\n"+tail 的字符预算
    let tail_len = tail.chars().count() + 1;
    for it in items {
        let name = it
            .get("sender_name")
            .and_then(|s| s.as_str())
            .unwrap_or_default();
        let line = format!("{name}：{}", item_excerpt(it));
        let used = out.chars().count();
        if used + 1 + line.chars().count() + tail_len > LIMIT {
            // 本行放不下了：截到预算内再收尾
            let room = LIMIT.saturating_sub(used + 1 + tail_len);
            if room > 0 {
                out.push('\n');
                out.extend(line.chars().take(room));
            }
            break;
        }
        out.push('\n');
        out.push_str(&line);
    }
    out.push('\n');
    out.push_str(&tail);
    out
}

/// 合并转发的标题（设计 2.4）：私聊「A 和 B 的聊天记录」、自会话「A 的聊天记录」、
/// 群聊固定「群聊的聊天记录」（不含群名）。名字先截 50 字。
pub fn record_title_for(conv_type: &str, caller_name: &str, peer_name: Option<&str>) -> String {
    let n = |s: &str| s.chars().take(50).collect::<String>();
    match (conv_type, peer_name) {
        ("group", _) => "群聊的聊天记录".to_string(),
        ("dm", Some(p)) => format!("{} 和 {} 的聊天记录", n(caller_name), n(p)),
        _ => format!("{} 的聊天记录", n(caller_name)),
    }
}

/// 附件占位文本（判定「这条附件条目是否带 caption」与构造兜底共用）：
/// image → "[图片]"；audio → "[语音]"；file → "[文件] {file_name}"。
pub fn attachment_placeholder(kind: &str, file_name: Option<&str>) -> String {
    match kind {
        "image" => "[图片]".to_string(),
        "audio" => "[语音]".to_string(),
        _ => format!("[文件] {}", file_name.unwrap_or_default()),
    }
}
