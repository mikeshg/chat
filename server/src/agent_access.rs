//! Agent 使用权：管理员始终可用；否则全局关则不可用；
//! 否则 `users.agent_access` 有值用它，NULL 跟随全局默认。
//! 所有检查点（REST、宿主握手、派发、清排队）都走这里。

use serde_json::Value;
use sqlx::PgExecutor;
use uuid::Uuid;

pub struct Policy {
    pub enabled: bool,
    pub default_allowed: bool,
}

fn flag(v: Option<Value>, default: bool) -> bool {
    v.and_then(|v| v.as_bool()).unwrap_or(default)
}

/// 管理员 → 可用；否则全局关 → 不可用；否则覆盖值，没有覆盖就用全局默认。
pub fn effective(role: &str, enabled: bool, default_allowed: bool, ov: Option<bool>) -> bool {
    if role == "admin" {
        return true;
    }
    if !enabled {
        return false;
    }
    ov.unwrap_or(default_allowed)
}

pub async fn load_policy(ex: impl PgExecutor<'_>) -> Result<Policy, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, Value)>(
        "SELECT key, value FROM server_settings
          WHERE key IN ('agent_enabled', 'agent_default_allowed')",
    )
    .fetch_all(ex)
    .await?;
    let mut enabled = true;
    let mut default_allowed = true;
    for (k, v) in rows {
        let Some(b) = v.as_bool() else { continue };
        if k == "agent_enabled" {
            enabled = b;
        } else if k == "agent_default_allowed" {
            default_allowed = b;
        }
    }
    Ok(Policy {
        enabled,
        default_allowed,
    })
}

/// 该用户现在能不能用 Agent。用户不存在当不可用。
pub async fn user_allows(ex: impl PgExecutor<'_>, uid: Uuid) -> Result<bool, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, Option<bool>, Option<Value>, Option<Value>)>(
        "SELECT u.role, u.agent_access,
                (SELECT value FROM server_settings WHERE key = 'agent_enabled'),
                (SELECT value FROM server_settings WHERE key = 'agent_default_allowed')
           FROM users u WHERE u.id = $1",
    )
    .bind(uid)
    .fetch_optional(ex)
    .await?;
    let Some((role, ov, en, def)) = row else {
        return Ok(false);
    };
    Ok(effective(&role, flag(en, true), flag(def, true), ov))
}
