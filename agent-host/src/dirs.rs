//! Agent A27：给「选工作区目录」列本机目录。只列子目录，不读文件内容。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 一次最多回这么多个子目录，免得超大目录撑爆一帧
const MAX_DIRS: usize = 1000;

/// path 为空 → 起点（Windows 各盘符 + 主目录；其他系统 / 和主目录）。
/// 返回 {path, parent, dirs: [{name, path}], error}。parent 为 null 表示回到起点。
pub fn list(path: &str) -> Value {
    if path.trim().is_empty() {
        return json!({"path": "", "parent": null, "dirs": roots(), "error": null});
    }
    let dir = PathBuf::from(path.trim());
    let parent = dir.parent().map(|p| p.to_string_lossy().to_string());
    let read = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) => {
            return json!({"path": path, "parent": parent, "dirs": [], "error": e.to_string()})
        }
    };
    let mut dirs: Vec<(String, String)> = read
        .filter_map(Result::ok)
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| {
            (
                e.file_name().to_string_lossy().to_string(),
                e.path().to_string_lossy().to_string(),
            )
        })
        .collect();
    dirs.sort_by_key(|(name, _)| name.to_lowercase());
    dirs.truncate(MAX_DIRS);
    let dirs: Vec<Value> = dirs
        .into_iter()
        .map(|(name, path)| json!({"name": name, "path": path}))
        .collect();
    json!({
        "path": dir.to_string_lossy(),
        "parent": parent,
        "dirs": dirs,
        "error": null,
    })
}

fn roots() -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        let p = PathBuf::from(home);
        out.push(
            json!({"name": format!("主目录（{}）", p.display()), "path": p.to_string_lossy()}),
        );
    }
    if cfg!(windows) {
        for letter in b'A'..=b'Z' {
            let drive = format!("{}:\\", letter as char);
            if Path::new(&drive).exists() {
                out.push(json!({"name": drive.clone(), "path": drive}));
            }
        }
    } else {
        out.push(json!({"name": "/", "path": "/"}));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_only_subdirs_sorted_with_parent() {
        let base = std::env::temp_dir().join(format!("a27dirs_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(base.join("b-dir")).unwrap();
        std::fs::create_dir_all(base.join("A-dir")).unwrap();
        std::fs::write(base.join("file.txt"), "x").unwrap();
        let v = list(&base.to_string_lossy());
        let names: Vec<&str> = v["dirs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["A-dir", "b-dir"]);
        assert_eq!(
            v["parent"],
            base.parent().unwrap().to_string_lossy().as_ref()
        );
        assert!(v["error"].is_null());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_dir_reports_error_and_empty_is_roots() {
        let v = list("Z:/definitely/not/here/a27");
        assert!(v["error"].is_string());
        let r = list("");
        assert!(!r["dirs"].as_array().unwrap().is_empty());
        assert!(r["parent"].is_null());
    }
}
