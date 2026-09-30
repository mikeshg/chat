// 会话存档：<state_dir>/<session-id>.json，记录生效模型和完整 transcript。
// load 时把 transcript 装回 Agent，模型能接着上下文聊；host 侧拿到
// session/update 回放做展示。

use std::path::{Path, PathBuf};

use agent::agent::AgentMessage;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(crate) struct Archive {
    /// 生效的模型 id（"<端点 id>/<模型>"）；load 时该模型已删则回退默认
    pub model: String,
    /// 完整 transcript（含 tool 结果），装回 Agent 续上下文
    pub transcript: Vec<AgentMessage>,
}

/// 存档路径。id 来自宿主侧字段，消毒一遍挡路径穿越。
fn path(state_dir: &Path, id: &str) -> PathBuf {
    let safe: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    state_dir.join(format!("{safe}.json"))
}

pub(crate) fn load(state_dir: &Path, id: &str) -> Option<Archive> {
    let text = std::fs::read_to_string(path(state_dir, id)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 写临时文件再改名，避免半写文件被当成有效存档。
pub(crate) fn save(state_dir: &Path, id: &str, archive: &Archive) {
    let file = path(state_dir, id);
    let result = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(state_dir)?;
        let tmp = file.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string(archive).unwrap_or_default())?;
        std::fs::rename(&tmp, &file)
    })();
    if let Err(e) = result {
        eprintln!("builtin: 存档 {} 失败: {e}", file.display());
    }
}
