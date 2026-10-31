use super::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

// ---------- 转发 / 收藏（V2 P2-4）+ 批量/合并转发（MF1） ----------

/// 逐目标结果：成功时携带完整 Posted（广播方按成员个性化 + unarchive 推送），
/// 失败带稳定错误码与用户可读原因（forbidden / not_friends / bad_request）。
pub enum Forwarded {
    Ok {
        conv: Uuid,
        posted: Posted,
    },
    Fail {
        conv: Uuid,
        code: &'static str,
        message: &'static str,
    },
}

/// 批量转发的逐目标结果：Ok 携带本会话写入的全部消息（separate 可能多条）。
pub enum BatchTarget {
    Ok { conv: Uuid, posteds: Vec<Posted> },
    Fail {
        conv: Uuid,
        code: &'static str,
        message: &'static str,
    },
}

/// 批量转发结果：skipped 在源侧只算一次，顶层与逐目标结果中原样复用。
pub struct BatchOutcome {
    pub targets: Vec<BatchTarget>,
    pub skipped: SkipMap,
}

/// forward_batch_prepare 的产物：一次一致读拿到的可用源消息（seq 升序）+ 跳过计数。
/// 快照构建（合并模式）推迟到限流之后的 forward_batch_exec 里做。
pub struct BatchPlan {
    merged: bool,
    title: String,
    source_kind: String,
    sources: Vec<SourceRow>,
    pub skipped: SkipMap,
}

/// 源消息行：分类、建快照、逐条转发共用同一份（一次查询 = 一致读）。
#[derive(sqlx::FromRow)]
struct SourceRow {
    id: Uuid,
    sender_id: Uuid,
    sender_name: String,
    has_avatar: bool,
    avatar_version: i32,
    created_at: DateTime<Utc>,
    kind: String,
    content: String,
    deleted: bool,
    record: Option<Value>,
    reply_to_id: Option<Uuid>,
    att_id: Option<Uuid>,
    att_name: Option<String>,
    att_mime: Option<String>,
    att_size: Option<i64>,
    att_width: Option<i32>,
    att_height: Option<i32>,
    att_has_thumb: Option<bool>,
    att_duration_ms: Option<i32>,
    att_recalled: Option<bool>,
}

impl SourceRow {
    fn att(&self) -> Option<SrcAtt> {
        self.att_id.map(|id| SrcAtt {
            id,
            file_name: self.att_name.clone().unwrap_or_default(),
            mime: self.att_mime.clone().unwrap_or_default(),
            size: self.att_size.unwrap_or_default(),
            width: self.att_width,
            height: self.att_height,
            has_thumb: self.att_has_thumb.unwrap_or(false),
            duration_ms: self.att_duration_ms,
            recalled: self.att_recalled.unwrap_or(false),
        })
    }
}

struct SrcAtt {
    id: Uuid,
    file_name: String,
    mime: String,
    size: i64,
    width: Option<i32>,
    height: Option<i32>,
    has_thumb: bool,
    duration_ms: Option<i32>,
    recalled: bool,
}

/// 「消息不可见/不属于源会话」的 403：和消息不存在共用同一个 deny 体，
/// 不允许借响应差异探测会话或消息是否存在（MF1-C1）。
fn forbidden_member() -> PostError {
    PostError::deny("forbidden", "not a member of this conversation")
}

/// POST /v1/messages/{id}/forward 的领域实现。
///
/// - 源消息：先成员校验（非成员 403），墓碑/投票 400；
///   forwarded_from = 源发送者展示名快照（改名/删除后标签不变）。
/// - chat_record 源：整条记录复制——快照 jsonb 原样带出、内部附件在目标事务内
///   重克隆（record_message_id → 新消息）并改写 id；forwarded_from 留空（记录自带署名）。
///   高度不增，level-3 记录可正常再转发。
/// - 目标逐条独立 post_message：单目标成员/好友校验失败只进该条结果，不拖垮其它；
///   DM 写限制、群成员、self 归 own 校验全部复用 write_message 事务内检查。
/// - client_msg_id = `fwd:{op}:{目标会话id}`：同 op+同目标重试命中
///   uq_messages_client_msg 幂等键返回同一条消息，不重复广播。
/// - 源附件经 forward_attachment 在目标事务内克隆（共享 storage_path，不破坏
///   一消息一附件不变量，不占配额）；克隆行 uploader = 转发者（MF1 修复 ②）。
/// - 转发正文/caption/兜底一律不解析 @（no_mentions，MF1 修复 ①）。
pub async fn forward_message(
    pool: &PgPool,
    uid: Uuid,
    message_id: Uuid,
    targets: &[Uuid],
    op: Uuid,
) -> Result<Vec<Forwarded>, PostError> {
    record_source_member(pool, uid, message_id).await?;
    let src = sqlx::query_as::<_, MsgViewRow>("SELECT * FROM message_views WHERE id = $1")
        .bind(message_id)
        .fetch_one(pool)
        .await
        .map_err(PostError::Db)?;
    if src.deleted_at.is_some() {
        return Err(PostError::deny("bad_message", "消息已撤回"));
    }
    if src.kind == "poll" {
        return Err(PostError::deny("bad_request", "投票不能转发"));
    }
    let record_post = if src.kind == "chat_record" {
        match src.record {
            Some(r) => {
                let mut atts = Vec::new();
                collect_attachment_ids(&r.0, &mut atts);
                Some(RecordPost {
                    json: r.0,
                    attachments: dedupe_ids(atts),
                })
            }
            // 活消息 record 为 NULL 是数据损坏：按不可用拒，比静默丢快照安全
            None => return Err(PostError::deny("bad_message", "聊天记录不可用")),
        }
    } else {
        None
    };
    let has_record = record_post.is_some();

    let mut results = Vec::with_capacity(targets.len());
    for conv in targets {
        // 幂等键含目标 id：同 op 投往不同会话互不撞键；同目标重试收敛同一行
        let key = format!("fwd:{op}:{conv}");
        let posted = post_message(
            pool,
            // 视图里的 content 已含附件占位文本（'[图片]'/'[文件] x'），
            // 重发同名附件时 normalize_body 不会重复拼装。
            // ADR-forward-keep-quote：保留 reply_to（可跨会话；不可见时客户端显示「原消息不可用」）
            PostMessage {
                client_msg_id: Some(&key),
                // 聊天记录自带逐条署名，外层不再加「转发自」
                forwarded_from: (!has_record).then_some(src.sender_name.as_str()),
                forward_attachment: if has_record { None } else { src.att_id },
                record: record_post.clone(),
                no_mentions: true,
                reply_to_id: src.reply_to_id,
                allow_foreign_reply: src.reply_to_id.is_some(),
                ..PostMessage::text(*conv, uid, &src.content)
            },
        )
        .await;
        results.push(match posted {
            Ok(posted) => Forwarded::Ok {
                conv: *conv,
                posted,
            },
            Err(PostError::Deny { code, message }) => Forwarded::Fail {
                conv: *conv,
                code,
                message,
            },
            Err(PostError::Db(e)) => return Err(PostError::Db(e)),
        });
    }
    Ok(results)
}

/// GET /v1/messages/{id}/record：完整快照只发给该消息所在会话的成员。
/// 消息不存在/非成员 → 同一 403；已撤回/非记录消息 → 404「聊天记录不可用」。
pub async fn get_record(
    pool: &PgPool,
    uid: Uuid,
    message_id: Uuid,
) -> Result<Value, PostError> {
    record_source_member(pool, uid, message_id).await?;
    let row = sqlx::query_as::<_, (String, bool, Option<Value>)>(
        "SELECT kind, deleted_at IS NOT NULL, record FROM messages WHERE id = $1",
    )
    .bind(message_id)
    .fetch_one(pool)
    .await
    .map_err(PostError::Db)?;
    if row.1 || row.0 != "chat_record" {
        return Err(PostError::deny("not_found", "聊天记录不可用"));
    }
    row.2
        .ok_or_else(|| PostError::deny("not_found", "聊天记录不可用"))
}

/// 记录内附件单独转发（MF1 Q6）：把快照里某个附件条目按普通附件消息逐目标转发。
///
/// 校验顺序（MF1-C1/C2）：调用方先做成员/存在 403、再限流，这里接着做——
/// 记录已撤回 → 400「消息已撤回」（同单条转发口径）；
/// 附件行不存在/record_message_id 不符/已撤回 → 同一 404「附件不存在」，
/// 三种失败不可区分，不给探测缝隙。
/// 快照里找不到对应条目（数据不一致）也归到同一 404。
/// 幂等键 `fwdr:{op}:{target}:{record_attachment_id}`。
pub async fn forward_record_item(
    pool: &PgPool,
    uid: Uuid,
    message_id: Uuid,
    attachment_id: Uuid,
    targets: &[Uuid],
    op: Uuid,
) -> Result<Vec<Forwarded>, PostError> {
    let row = sqlx::query_as::<_, (String, bool, Option<Value>)>(
        "SELECT kind, deleted_at IS NOT NULL, record FROM messages WHERE id = $1",
    )
    .bind(message_id)
    .fetch_optional(pool)
    .await
    .map_err(PostError::Db)?;
    let (kind, deleted, record) = match row {
        Some(r) => r,
        None => return Err(forbidden_member()),
    };
    if deleted {
        return Err(PostError::deny("bad_message", "消息已撤回"));
    }
    // 非 chat_record 的消息不可能有 record 克隆行 → 与「附件不属于它」同一路径 404
    let att_ok = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM attachments
             WHERE id = $1 AND record_message_id = $2 AND recalled_at IS NULL)",
    )
    .bind(attachment_id)
    .bind(message_id)
    .fetch_one(pool)
    .await
    .map_err(PostError::Db)?;
    if kind != "chat_record" || !att_ok {
        return Err(PostError::deny("not_found", "附件不存在"));
    }
    let (_, sender_name, content) = record
        .as_ref()
        .and_then(|r| find_item_by_attachment(r, attachment_id))
        .ok_or_else(|| PostError::deny("not_found", "附件不存在"))?;
    let sender_name: String = sender_name.chars().take(50).collect();

    let mut results = Vec::with_capacity(targets.len());
    for conv in targets {
        let key = format!("fwdr:{op}:{conv}:{attachment_id}");
        let posted = post_message(
            pool,
            PostMessage {
                client_msg_id: Some(&key),
                // 署名取快照里冻结的展示名，不取现值
                forwarded_from: Some(sender_name.as_str()),
                forward_attachment: Some(attachment_id),
                no_mentions: true,
                ..PostMessage::text(*conv, uid, &content)
            },
        )
        .await;
        results.push(match posted {
            Ok(posted) => Forwarded::Ok {
                conv: *conv,
                posted,
            },
            Err(PostError::Deny { code, message }) => Forwarded::Fail {
                conv: *conv,
                code,
                message,
            },
            Err(PostError::Db(e)) => return Err(PostError::Db(e)),
        });
    }
    Ok(results)
}

/// 「消息所在会话的成员」判定：消息不存在/非成员统一 403，不探测存在性。
/// forward（记录内附件路径）与 GET record 共用。
pub async fn record_source_member(
    pool: &PgPool,
    uid: Uuid,
    message_id: Uuid,
) -> Result<(), PostError> {
    let member = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
             SELECT 1 FROM memberships mb
             JOIN messages m ON m.conversation_id = mb.conversation_id
              WHERE mb.user_id = $1 AND m.id = $2)",
    )
    .bind(uid)
    .bind(message_id)
    .fetch_one(pool)
    .await
    .map_err(PostError::Db)?;
    if !member {
        return Err(forbidden_member());
    }
    Ok(())
}

/// POST /v1/messages/forward-batch 第一阶段：源会话授权 + 一致读 + 跳过分类。
/// 必须在限流命中之前调用——「没有可转发的消息」「嵌套过深」的 400 不占配额。
///
/// 单条 SELECT 就是一次一致读（MF1-C6）：分类、快照、逐条转发共用这些行。
/// 跳过原因闭集：unavailable（不存在/别的会话/不可见）、recalled、poll、too_deep。
pub async fn forward_batch_prepare(
    pool: &PgPool,
    uid: Uuid,
    conv: Uuid,
    ids: &[Uuid],
    merged: bool,
) -> Result<BatchPlan, PostError> {
    // 源会话授权：会话不存在与非成员同一 403 文案（无存在性泄露）
    let member = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM memberships WHERE conversation_id = $1 AND user_id = $2)",
    )
    .bind(conv)
    .bind(uid)
    .fetch_one(pool)
    .await
    .map_err(PostError::Db)?;
    if !member {
        return Err(forbidden_member());
    }

    let conv_type = sqlx::query_scalar::<_, String>("SELECT type FROM conversations WHERE id = $1")
        .bind(conv)
        .fetch_one(pool)
        .await
        .map_err(PostError::Db)?;
    let caller_name: String = sqlx::query_scalar::<_, String>(
        "SELECT COALESCE(NULLIF(display_name, ''), username::text) FROM users WHERE id = $1",
    )
    .bind(uid)
    .fetch_one(pool)
    .await
    .map_err(PostError::Db)?;
    let peer_name = if conv_type == "dm" {
        sqlx::query_scalar::<_, String>(
            "SELECT COALESCE(NULLIF(u.display_name, ''), u.username::text)
               FROM memberships mb JOIN users u ON u.id = mb.user_id
              WHERE mb.conversation_id = $1 AND mb.user_id <> $2
              ORDER BY u.username LIMIT 1",
        )
        .bind(conv)
        .bind(uid)
        .fetch_optional(pool)
        .await
        .map_err(PostError::Db)?
    } else {
        None
    };

    let rows: Vec<SourceRow> = sqlx::query_as::<_, SourceRow>(
        "SELECT m.id, m.sender_id,
                COALESCE(NULLIF(u.display_name, ''), u.username::text) AS sender_name,
                u.avatar_path IS NOT NULL AS has_avatar, u.avatar_version,
                m.created_at, m.kind, m.content, m.deleted_at IS NOT NULL AS deleted,
                CASE WHEN m.deleted_at IS NULL THEN m.record END AS record,
                m.reply_to_id,
                a.id AS att_id, a.file_name AS att_name, a.mime AS att_mime,
                a.size AS att_size, a.width AS att_width, a.height AS att_height,
                a.thumb_path IS NOT NULL AS att_has_thumb, a.duration_ms AS att_duration_ms,
                a.recalled_at IS NOT NULL AS att_recalled
           FROM messages m
           JOIN users u ON u.id = m.sender_id
           LEFT JOIN attachments a ON a.id = m.attachment_id
          WHERE m.conversation_id = $1 AND m.id = ANY($2)
          ORDER BY m.seq ASC",
    )
    .bind(conv)
    .bind(ids)
    .fetch_all(pool)
    .await
    .map_err(PostError::Db)?;

    // 分类：逐请求 id 判一次；survivors 沿行序（seq 升序）收集，天然按 seq 排好
    let by_id: HashMap<Uuid, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| (r.id, i))
        .collect();
    let mut skipped: SkipMap = BTreeMap::new();
    let mut skip_set: HashSet<Uuid> = HashSet::new();
    let requested: HashSet<Uuid> = ids.iter().copied().collect();
    for id in ids {
        let reason = match by_id.get(id) {
            None => Some("unavailable"),
            Some(&i) => classify(&rows[i], merged),
        };
        if let Some(reason) = reason {
            *skipped.entry(reason.to_string()).or_insert(0) += 1;
            skip_set.insert(*id);
        }
    }
    let sources: Vec<SourceRow> = rows
        .into_iter()
        .filter(|r| requested.contains(&r.id) && !skip_set.contains(&r.id))
        .collect();
    if sources.is_empty() {
        // 全因 too_deep 跳过 → 专用文案；否则统一「没有可转发的消息」
        return if !skipped.is_empty() && skipped.keys().all(|k| k == "too_deep") {
            Err(PostError::deny("bad_request", "聊天记录嵌套过深"))
        } else {
            Err(PostError::deny("bad_request", "没有可转发的消息"))
        };
    }
    Ok(BatchPlan {
        merged,
        title: record_title_for(&conv_type, &caller_name, peer_name.as_deref()),
        source_kind: conv_type,
        sources,
        skipped,
    })
}

/// 单条源消息的跳过原因；None = 可转发。
/// merged 模式才查嵌套高度（外层记一层，被选记录高度必须 ≤2）；
/// separate/单条转发原样复制，高度不变，level-3 记录照样过。
fn classify(r: &SourceRow, merged: bool) -> Option<&'static str> {
    if r.deleted {
        return Some("recalled");
    }
    if r.kind == "poll" {
        return Some("poll");
    }
    if merged && r.kind == "chat_record" {
        let h = r.record.as_ref().map(record_height).unwrap_or(0);
        if h > 2 {
            return Some("too_deep");
        }
    }
    None
}

/// 第二阶段（限流命中之后）：merged 先建快照做大小/条目/附件上限校验，
/// 再逐目标写。separate 每目标单事务（整目标成败一体），逐条 write_message。
pub async fn forward_batch_exec(
    pool: &PgPool,
    uid: Uuid,
    plan: BatchPlan,
    targets: &[Uuid],
    op: Uuid,
) -> Result<BatchOutcome, PostError> {
    if plan.merged {
        let (record, atts, content) = build_merged(&plan)?;
        let mut out = Vec::with_capacity(targets.len());
        for conv in targets {
            let key = format!("fwdm:{op}:{conv}");
            let posted = post_message(
                pool,
                PostMessage {
                    client_msg_id: Some(&key),
                    record: Some(RecordPost {
                        json: record.clone(),
                        attachments: atts.clone(),
                    }),
                    no_mentions: true,
                    ..PostMessage::text(*conv, uid, &content)
                },
            )
            .await;
            out.push(match posted {
                Ok(p) => BatchTarget::Ok {
                    conv: *conv,
                    posteds: vec![p],
                },
                Err(PostError::Deny { code, message }) => BatchTarget::Fail {
                    conv: *conv,
                    code,
                    message,
                },
                Err(PostError::Db(e)) => return Err(PostError::Db(e)),
            });
        }
        return Ok(BatchOutcome {
            targets: out,
            skipped: plan.skipped,
        });
    }

    let mut out = Vec::with_capacity(targets.len());
    for conv in targets {
        let r = separate_target(pool, uid, *conv, &plan.sources, op).await;
        out.push(match r {
            Ok(posteds) => BatchTarget::Ok {
                conv: *conv,
                posteds,
            },
            Err(PostError::Deny { code, message }) => BatchTarget::Fail {
                conv: *conv,
                code,
                message,
            },
            Err(PostError::Db(e)) => return Err(PostError::Db(e)),
        });
    }
    Ok(BatchOutcome {
        targets: out,
        skipped: plan.skipped,
    })
}

/// 合并快照（MF1-C4/C6）：条目按 seq 序；附件列源侧 id（写时按目标克隆改写）。
/// 上限：总条数（含嵌套）≤2000、总附件（含嵌套）≤300、jsonb ≤1MiB——
/// 超限整请求 400「聊天记录过大」（此时限流已计一次）。
fn build_merged(plan: &BatchPlan) -> Result<(Value, Vec<Uuid>, String), PostError> {
    let items: Vec<Value> = plan.sources.iter().map(snapshot_from_source).collect();
    let record = json!({
        "v": 1,
        "title": plan.title,
        "source_kind": plan.source_kind,
        "count": items.len(),
        "items": items,
    });
    let mut atts = Vec::new();
    collect_attachment_ids(&record, &mut atts);
    let atts = dedupe_ids(atts);
    if count_items(&record) > 2000 || atts.len() > 300 {
        return Err(PostError::deny("bad_request", "聊天记录过大"));
    }
    let size = serde_json::to_vec(&record).map_err(|_| PostError::deny("internal", "db error"))?;
    if size.len() > 1_048_576 {
        return Err(PostError::deny("bad_request", "聊天记录过大"));
    }
    let content = fallback_text(&plan.title, &items, items.len());
    Ok((record, atts, content))
}

/// 一条源消息 → 快照条目（设计 2.1 字段集）。
/// 附件条目：源附件还在且未撤回 → attachment 列源 id；否则 attachment=null、
/// content 留 caption（没有 caption 就写「[已撤回的附件]」占位——MF1-C6 选定口径）。
/// 嵌套记录：record 整份拷贝（附件 id 写时改写）；content 用一行标题摘要。
fn snapshot_from_source(s: &SourceRow) -> Value {
    let mut content = s.content.clone();
    let mut attachment = Value::Null;
    let mut record = Value::Null;
    match s.kind.as_str() {
        "chat_record" => {
            let inner = s.record.clone().unwrap_or(Value::Null);
            content = format!("[聊天记录] {}", record_title(&inner));
            record = inner;
        }
        "image" | "file" | "audio" => match s.att() {
            Some(a) if !a.recalled => {
                attachment = json!({
                    "id": a.id,
                    "file_name": a.file_name,
                    "mime": a.mime,
                    "size": a.size,
                    "width": a.width,
                    "height": a.height,
                    "has_thumb": a.has_thumb,
                    "duration_ms": a.duration_ms,
                });
            }
            att => {
                // 附件被撤回/行缺失：条目保留，附件置空；正文留 caption 或占位文本
                let placeholder = attachment_placeholder(
                    &s.kind,
                    att.as_ref().map(|a| a.file_name.as_str()),
                );
                if content == placeholder {
                    content = "[已撤回的附件]".to_string();
                }
            }
        },
        _ => {}
    }
    snapshot_item(
        s.sender_id,
        &s.sender_name,
        s.has_avatar,
        s.avatar_version,
        s.created_at,
        &s.kind,
        &content,
        attachment,
        record,
    )
}

/// separate 模式逐目标写入：本目标的消息与附件克隆同事务，整体提交或回滚。
/// 同事务里先按前缀捞同 op 已提交的行——本次源里被撤/被跳过的那条也要回放
/// （同 op 不同体的重试不比对请求体，已文档化）；并发撞唯一键才走 separate_replay。
async fn separate_target(
    pool: &PgPool,
    uid: Uuid,
    conv: Uuid,
    sources: &[SourceRow],
    op: Uuid,
) -> Result<Vec<Posted>, PostError> {
    let mut tx = pool.begin().await.map_err(PostError::Db)?;
    let stored: Vec<(String, i64, Uuid)> = sqlx::query_as(
        "SELECT client_msg_id, seq, id FROM messages
          WHERE conversation_id = $1 AND sender_id = $2
            AND client_msg_id LIKE $3
          ORDER BY seq",
    )
    .bind(conv)
    .bind(uid)
    .bind(format!("fwds:{op}:{conv}:%"))
    .fetch_all(&mut *tx)
    .await
    .map_err(PostError::Db)?;
    let stored_seq: HashMap<String, i64> = stored
        .iter()
        .map(|(k, s, _)| (k.clone(), *s))
        .collect();

    // hits 按已存 seq 排（首次写入顺序），fresh 恒排在后（seq 更大）。
    let mut hits: Vec<(i64, Posted)> = Vec::new();
    let mut fresh: Vec<Posted> = Vec::new();
    let mut emitted: HashSet<String> = HashSet::new();
    for s in sources {
        let key = separate_key(op, conv, s.id);
        let key = normalize_client_msg_id(Some(&key))?;
        let pm = post_for_source(s, conv, uid);
        match write_message(&mut tx, &pm, key.as_deref()).await {
            Ok(p) => {
                let covered = p.inserted;
                match key
                    .as_deref()
                    .and_then(|k| stored_seq.get(k).map(|seq| (*seq, k)))
                {
                    Some((seq, k)) => {
                        emitted.insert(k.to_string());
                        hits.push((seq, p));
                    }
                    None => {
                        let _ = covered;
                        fresh.push(p);
                    }
                }
            }
            Err(WriteError::IdempotentHit) => {
                let _ = tx.rollback().await;
                return separate_replay(pool, uid, conv, sources, op).await;
            }
            Err(WriteError::Fail(e)) => {
                let _ = tx.rollback().await;
                return Err(e);
            }
        }
    }
    // 本次请求没覆盖到、但同 op 已提交过的行：回放返回，不广播。
    for (key, seq, id) in stored {
        if emitted.contains(&key) {
            continue;
        }
        let p = load_posted(&mut tx, conv, uid, id, false, Vec::new()).await?;
        hits.push((seq, p));
    }
    hits.sort_by_key(|(seq, _)| *seq);
    tx.commit().await.map_err(PostError::Db)?;
    Ok(hits
        .into_iter()
        .map(|(_, p)| p)
        .chain(fresh)
        .collect())
}

/// separate 撞键回放：按前缀 `fwds:{op}:{conv}:` 捞回该 op 在本目标已提交的
/// 全部消息（含本次请求体里已被跳过/删除的源——重试不比对请求体，已文档化），
/// 标记 inserted=false 不广播；本次仍可用但还没存过的源单独补发。
async fn separate_replay(
    pool: &PgPool,
    uid: Uuid,
    conv: Uuid,
    sources: &[SourceRow],
    op: Uuid,
) -> Result<Vec<Posted>, PostError> {
    let stored: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT client_msg_id, id FROM messages
          WHERE conversation_id = $1 AND sender_id = $2
            AND client_msg_id LIKE $3
          ORDER BY seq",
    )
    .bind(conv)
    .bind(uid)
    .bind(format!("fwds:{op}:{conv}:%"))
    .fetch_all(pool)
    .await
    .map_err(PostError::Db)?;
    let member_ids = member_ids(pool, conv).await.map_err(PostError::Db)?;
    let mut seen: HashSet<String> = HashSet::with_capacity(stored.len());
    let mut posteds = Vec::with_capacity(stored.len() + sources.len());
    for (key, id) in stored {
        seen.insert(key);
        let message = view_by_id(pool, uid, id).await.map_err(PostError::Db)?;
        posteds.push(Posted {
            message,
            inserted: false,
            member_ids: member_ids.clone(),
            unarchived: Vec::new(),
        });
    }
    for s in sources {
        let key = separate_key(op, conv, s.id);
        if seen.contains(&key) {
            continue;
        }
        let mut pm = post_for_source(s, conv, uid);
        pm.client_msg_id = Some(key.as_str());
        // post_message 内部已处理并发撞键：返回已存在行（inserted=false，不广播）。
        posteds.push(post_message(pool, pm).await?);
    }
    Ok(posteds)
}

fn separate_key(op: Uuid, conv: Uuid, src: Uuid) -> String {
    format!("fwds:{op}:{conv}:{src}")
}

/// 源消息 → 逐条转发的 PostMessage：普通消息同单条转发口径；chat_record 复制快照。
fn post_for_source<'a>(s: &'a SourceRow, conv: Uuid, uid: Uuid) -> PostMessage<'a> {
    if s.kind == "chat_record" {
        let mut atts = Vec::new();
        if let Some(r) = &s.record {
            collect_attachment_ids(r, &mut atts);
        }
        PostMessage {
            record: s.record.clone().map(|j| RecordPost {
                json: j,
                attachments: dedupe_ids(atts),
            }),
            forwarded_from: None,
            no_mentions: true,
            reply_to_id: s.reply_to_id,
            allow_foreign_reply: s.reply_to_id.is_some(),
            ..PostMessage::text(conv, uid, &s.content)
        }
    } else {
        PostMessage {
            forwarded_from: Some(&s.sender_name),
            forward_attachment: s.att().filter(|a| !a.recalled).map(|a| a.id),
            no_mentions: true,
            reply_to_id: s.reply_to_id,
            allow_foreign_reply: s.reply_to_id.is_some(),
            ..PostMessage::text(conv, uid, &s.content)
        }
    }
}

fn dedupe_ids(ids: Vec<Uuid>) -> Vec<Uuid> {
    let mut seen = HashSet::new();
    ids.into_iter().filter(|i| seen.insert(*i)).collect()
}
