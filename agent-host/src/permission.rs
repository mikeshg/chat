// Agent A12：按权限档位决定 session/request_permission 自动放行还是交给主人。
//
// read_only：read / search / think / fetch 自动允许。
// workspace：在只读基础上，edit / delete / move 且每个 location 都在工作目录或额外目录内。
// full：全部自动允许。
// 自动允许时选 agent 给的 allow_once；没有就选第一个 allow 类选项。

use serde_json::Value;
use std::path::{Component, Path, PathBuf};

const READ_KINDS: &[&str] = &["read", "search", "think", "fetch"];
const WRITE_KINDS: &[&str] = &["edit", "delete", "move"];

/// 档位内可以自动放行。locations 为空的写入不算「都在目录内」。
pub fn within_tier(tier: &str, kind: &str, locations: &[String], roots: &[PathBuf]) -> bool {
    if tier == "full" {
        return true;
    }
    let kind = kind.to_ascii_lowercase();
    if READ_KINDS.contains(&kind.as_str()) {
        return true;
    }
    if tier == "workspace" && WRITE_KINDS.contains(&kind.as_str()) {
        return !locations.is_empty() && locations.iter().all(|p| path_inside(p, roots));
    }
    false
}

/// 从 ACP toolCall.locations 抽出路径。元素可以是字符串或 `{path}`。
pub fn location_strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|item| {
                    let s = if let Some(s) = item.as_str() {
                        s
                    } else {
                        item.get("path").and_then(Value::as_str)?
                    };
                    let s = s.trim();
                    if s.is_empty() {
                        None
                    } else {
                        Some(s.to_string())
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 交给服务端的精简 toolCall：title / kind / locations。
pub fn summarize_tool(tool: &Value) -> Value {
    let title = tool["title"].as_str().unwrap_or("").to_string();
    let kind = tool["kind"].as_str().unwrap_or("other").to_string();
    let locations = location_strings(&tool["locations"]);
    serde_json::json!({
        "title": title,
        "kind": kind,
        "locations": locations,
    })
}

/// allow_once 优先，否则数组里第一个 kind 含 allow 的选项。
pub fn pick_allow(options: &Value) -> Option<String> {
    let arr = options.as_array()?;
    if let Some(id) = arr.iter().find_map(|o| {
        (o["kind"].as_str() == Some("allow_once"))
            .then(|| o["optionId"].as_str())
            .flatten()
            .map(str::to_string)
    }) {
        return Some(id);
    }
    arr.iter().find_map(|o| {
        let kind = o["kind"].as_str().unwrap_or("");
        if kind.contains("allow") {
            o["optionId"].as_str().map(str::to_string)
        } else {
            None
        }
    })
}

pub fn has_option(options: &Value) -> bool {
    options.as_array().is_some_and(|a| {
        a.iter()
            .any(|o| o["optionId"].as_str().is_some_and(|s| !s.is_empty()))
    })
}

/// loc 在任一 root 之内（含 root 自身）。相对路径相对 roots[0]（工作目录）。
/// 只做词法归一，不要求路径在磁盘上存在。
pub fn path_inside(loc: &str, roots: &[PathBuf]) -> bool {
    let loc = loc.trim();
    if loc.is_empty() || roots.is_empty() {
        return false;
    }
    let raw = Path::new(loc);
    let abs = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        roots[0].join(raw)
    };
    let norm = logical_norm(&abs);
    roots.iter().any(|r| {
        let rn = logical_norm(r);
        !rn.as_os_str().is_empty() && (norm == rn || norm.starts_with(&rn))
    })
}

fn logical_norm(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from(r"C:\work"), PathBuf::from(r"D:\extra")]
    }

    #[test]
    fn read_only_allows_read_search_think_fetch_only() {
        let roots = roots();
        assert!(within_tier("read_only", "read", &[r"C:\x".into()], &roots));
        assert!(within_tier("read_only", "search", &[], &roots));
        assert!(within_tier("read_only", "think", &[], &roots));
        assert!(within_tier("read_only", "fetch", &[], &roots));
        assert!(!within_tier(
            "read_only",
            "edit",
            &[r"C:\work\a.rs".into()],
            &roots
        ));
        assert!(!within_tier("read_only", "execute", &[], &roots));
        assert!(!within_tier("read_only", "switch_mode", &[], &roots));
    }

    #[test]
    fn workspace_write_must_stay_inside_every_location() {
        let roots = roots();
        assert!(within_tier(
            "workspace",
            "edit",
            &[r"C:\work\src\a.rs".into()],
            &roots
        ));
        assert!(within_tier(
            "workspace",
            "delete",
            &[r"D:\extra\a".into()],
            &roots
        ));
        assert!(within_tier(
            "workspace",
            "move",
            &[r"src\a.rs".into()],
            &roots
        ));
        assert!(within_tier(
            "workspace",
            "edit",
            &[r"C:/work/src/a.rs".into()],
            &roots
        ));
        assert!(!within_tier(
            "workspace",
            "edit",
            &[r"C:\other\a".into()],
            &roots
        ));
        assert!(!within_tier(
            "workspace",
            "edit",
            &[r"C:\work\..\other\a".into()],
            &roots
        ));
        assert!(!within_tier("workspace", "edit", &[], &roots));
        assert!(!within_tier(
            "workspace",
            "edit",
            &[r"C:\work\a".into(), r"C:\other\b".into()],
            &roots
        ));
        assert!(!within_tier(
            "workspace",
            "execute",
            &[r"C:\work\a".into()],
            &roots
        ));
        // 前缀不是目录边界：C:\workspace 不是 C:\work 的子路径
        assert!(!path_inside(r"C:\workspace\a", &roots));
    }

    #[test]
    fn full_allows_everything() {
        let roots = roots();
        assert!(within_tier(
            "full",
            "execute",
            &[r"C:\secret".into()],
            &roots
        ));
        assert!(within_tier("full", "other", &[], &roots));
    }

    #[test]
    fn pick_allow_once_before_other_allow_kinds() {
        let opts = serde_json::json!([
            {"optionId":"always","kind":"allow_always","name":"A"},
            {"optionId":"allow","kind":"allow_once","name":"B"},
            {"optionId":"reject","kind":"reject_once","name":"R"}
        ]);
        assert_eq!(pick_allow(&opts).as_deref(), Some("allow"));
        let opts = serde_json::json!([
            {"optionId":"reject","kind":"reject_once"},
            {"optionId":"always","kind":"allow_always"}
        ]);
        assert_eq!(pick_allow(&opts).as_deref(), Some("always"));
        let opts = serde_json::json!([{"optionId":"reject","kind":"reject_once"}]);
        assert!(pick_allow(&opts).is_none());
    }
}
