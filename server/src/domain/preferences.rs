use super::*;

// ---------- 会话偏好：置顶 / 静音 / 归档 ----------
//
// 偏好是用户维度数据，不改变会话本身；仅本人可见、仅同步本人全部设备。
// 会话被解散或本人被移出后不可再改（在会话行锁内重检成员资格与解散标记）。

/// 读取 (conv, uid) 的偏好行；无记录时返回全默认（updated_at = None）。
pub async fn pref_of(
    executor: impl PgExecutor<'_>,
    conv: Uuid,
    uid: Uuid,
) -> Result<ConversationPref, sqlx::Error> {
    let row = sqlx::query_as::<
        _,
        (
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
            bool,
            DateTime<Utc>,
        ),
    >(
        "SELECT pinned_at, archived_at, muted_until, mute_forever, updated_at
           FROM conversation_preferences
          WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conv)
    .bind(uid)
    .fetch_optional(executor)
    .await?;
    Ok(row
        .map(
            |(pinned_at, archived_at, muted_until, mute_forever, t)| ConversationPref {
                pinned_at,
                archived_at,
                muted_until,
                mute_forever,
                updated_at: Some(t),
            },
        )
        .unwrap_or_default())
}

/// 静音写入目标（mode 已校验）。
pub(super) enum MuteSet {
    None,
    Forever,
    Until(DateTime<Utc>),
}

/// PUT preferences：缺省字段不改；mute.until 必须在未来；会话解散/非成员拒绝。
/// 状态覆写天然幂等（重复 PUT 同值收敛）；updated_at 每次写入刷新。
pub async fn set_preferences(
    pool: &PgPool,
    conv: Uuid,
    uid: Uuid,
    req: &PrefUpdateReq,
) -> Result<ConversationPref, PostError> {
    // 先校验 mute 形状再进事务：未知 mode / until 用在非 until 模式 / until 缺失都是 400
    let mute = match &req.mute {
        None => None,
        Some(m) => {
            let set = match m.mode.as_str() {
                "none" => MuteSet::None,
                "forever" => MuteSet::Forever,
                "until" => MuteSet::Until(m.until.ok_or_else(|| {
                    PostError::deny("bad_request", "mute.until is required for mode 'until'")
                })?),
                _ => return Err(PostError::deny("bad_request", "unknown mute mode")),
            };
            if !matches!(set, MuteSet::Until(_)) && m.until.is_some() {
                return Err(PostError::deny(
                    "bad_request",
                    "mute.until is only valid with mode 'until'",
                ));
            }
            Some(set)
        }
    };

    let mut tx = pool.begin().await.map_err(PostError::Db)?;
    let res = set_preferences_in_tx(&mut tx, conv, uid, req, mute).await;
    match res {
        Ok(pref) => {
            tx.commit().await.map_err(PostError::Db)?;
            Ok(pref)
        }
        Err(WriteError::Fail(e)) => {
            let _ = tx.rollback().await;
            Err(e)
        }
        Err(WriteError::IdempotentHit) => unreachable!(),
    }
}

pub(super) async fn set_preferences_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    conv: Uuid,
    uid: Uuid,
    req: &PrefUpdateReq,
    mute: Option<MuteSet>,
) -> Result<ConversationPref, WriteError> {
    let conv_row = lock_conversation(tx, conv)
        .await?
        .ok_or_else(|| PostError::deny("forbidden", "not a member of this conversation"))?;
    if conv_row.2 {
        // 已解散（成员已被清空，membership 检查也会拦，但显式拒绝语义更准）
        return Err(PostError::deny("forbidden", "conversation dissolved").into());
    }
    membership(&mut **tx, conv, uid)
        .await?
        .ok_or_else(|| PostError::deny("forbidden", "not a member of this conversation"))?;

    let now = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    let cur = pref_of(&mut **tx, conv, uid).await?;

    let pinned_at = match req.pinned {
        Some(true) => cur.pinned_at.or(Some(now)),
        Some(false) => None,
        None => cur.pinned_at,
    };
    let archived_at = match req.archived {
        Some(true) => Some(now),
        Some(false) => None,
        None => cur.archived_at,
    };
    let (mute_forever, muted_until) = match mute {
        None => (cur.mute_forever, cur.muted_until),
        Some(MuteSet::None) => (false, None),
        Some(MuteSet::Forever) => (true, None),
        Some(MuteSet::Until(t)) => {
            if t <= now {
                return Err(
                    PostError::deny("bad_request", "mute.until must be in the future").into(),
                );
            }
            (false, Some(t))
        }
    };

    sqlx::query(
        "INSERT INTO conversation_preferences
            (conversation_id, user_id, pinned_at, archived_at, mute_forever, muted_until, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, clock_timestamp())
         ON CONFLICT (conversation_id, user_id) DO UPDATE SET
            pinned_at = EXCLUDED.pinned_at,
            archived_at = EXCLUDED.archived_at,
            mute_forever = EXCLUDED.mute_forever,
            muted_until = EXCLUDED.muted_until,
            updated_at = GREATEST(clock_timestamp(), conversation_preferences.updated_at + interval '1 microsecond')",
    )
    .bind(conv)
    .bind(uid)
    .bind(pinned_at)
    .bind(archived_at)
    .bind(mute_forever)
    .bind(muted_until)
    .execute(&mut **tx)
    .await?;

    let pref = pref_of(&mut **tx, conv, uid).await?;
    Ok(pref)
}
