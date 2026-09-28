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
/// A19：按真实路径判断——root 先 canonicalize（解析失败的跳过）；loc 从原始
/// 路径找最长的已存在前缀做 canonicalize，再接上剩余不存在的部分；剩余部分
/// 含 `..` 等非普通组件时落点无法验证，判为不在目录内（交主人审批）。这样
/// 工作目录里的 junction / 符号链接指向目录外时不会被误放行。
pub fn path_inside(loc: &str, roots: &[PathBuf]) -> bool {
    let loc = loc.trim();
    if loc.is_empty() || roots.is_empty() {
        return false;
    }
    let roots: Vec<PathBuf> = roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect();
    if roots.is_empty() {
        return false;
    }
    let raw = Path::new(loc);
    let abs = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        roots[0].join(raw)
    };
    // 最长的已存在前缀：逐组件向前探（不能反向 pop——Windows 的 exists() 对
    // 「不存在目录 + ..」也会返回真，会把 .. 吞进前缀）。不先做词法 .. 归一，
    // 交给 canonicalize 解真实落点。
    let comps: Vec<Component> = abs.components().collect();
    let mut existing = PathBuf::new();
    let mut n = 0;
    for i in 0..comps.len() {
        let cand: PathBuf = comps[..=i].iter().copied().collect();
        // 裸盘符前缀（C: 或 \\?\C:）不是可探测的实体，种下继续
        if matches!(comps[i], Component::Prefix(_)) || cand.exists() {
            existing = cand;
            n = i + 1;
        } else {
            break;
        }
    }
    if n == 0 {
        return false;
    }
    let base = match std::fs::canonicalize(&existing) {
        Ok(c) => c,
        Err(_) => return false,
    };
    // 剩余不存在的部分必须是普通组件，逐个接到解析后的前缀上
    let mut candidate = base;
    for c in comps.iter().skip(n) {
        match c {
            Component::Normal(p) => candidate.push(p),
            _ => return false,
        }
    }
    roots
        .iter()
        .any(|r| candidate == *r || candidate.starts_with(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一棵真实目录树：work（工作目录）/ extra（额外目录）/ outside（目录外）
    /// / workspace（与 work 前缀相同的兄弟目录，测目录边界）。drop 时整树删除。
    struct Tree {
        root: PathBuf,
        work: PathBuf,
        extra: PathBuf,
        outside: PathBuf,
        workspace: PathBuf,
    }

    impl Tree {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("a19-perm-{}", uuid::Uuid::new_v4().simple()));
            let t = Self {
                work: root.join("work"),
                extra: root.join("extra"),
                outside: root.join("outside"),
                workspace: root.join("workspace"),
                root,
            };
            for d in [&t.work, &t.extra, &t.outside, &t.workspace] {
                std::fs::create_dir_all(d).unwrap();
            }
            t
        }

        fn roots(&self) -> Vec<PathBuf> {
            vec![self.work.clone(), self.extra.clone()]
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    /// Windows 建 junction（mklink /J 不需管理员），Unix 建符号链接；
    /// 建不出来打印原因返回 false，用例跳过。
    fn make_link(target: &Path, link: &Path) -> bool {
        #[cfg(windows)]
        {
            match std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
            {
                Ok(o) if o.status.success() => true,
                Ok(o) => {
                    eprintln!(
                        "mklink /J 失败，跳过链接用例: {}",
                        String::from_utf8_lossy(&o.stderr)
                    );
                    false
                }
                Err(e) => {
                    eprintln!("mklink 无法执行，跳过链接用例: {e}");
                    false
                }
            }
        }
        #[cfg(unix)]
        {
            match std::os::unix::fs::symlink(target, link) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("symlink 失败，跳过链接用例: {e}");
                    false
                }
            }
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = (target, link);
            eprintln!("该平台不支持建链接，跳过链接用例");
            false
        }
    }

    #[test]
    fn read_only_allows_read_search_think_fetch_only() {
        let t = Tree::new();
        let roots = t.roots();
        assert!(within_tier(
            "read_only",
            "read",
            &[s(&t.outside.join("x"))],
            &roots
        ));
        assert!(within_tier("read_only", "search", &[], &roots));
        assert!(within_tier("read_only", "think", &[], &roots));
        assert!(within_tier("read_only", "fetch", &[], &roots));
        assert!(!within_tier(
            "read_only",
            "edit",
            &[s(&t.work.join("a.rs"))],
            &roots
        ));
        assert!(!within_tier("read_only", "execute", &[], &roots));
        assert!(!within_tier("read_only", "switch_mode", &[], &roots));
    }

    #[test]
    fn workspace_write_must_stay_inside_every_location() {
        let t = Tree::new();
        let roots = t.roots();
        assert!(within_tier(
            "workspace",
            "edit",
            &[s(&t.work.join("src").join("a.rs"))],
            &roots
        ));
        assert!(within_tier(
            "workspace",
            "delete",
            &[s(&t.extra.join("a"))],
            &roots
        ));
        // 相对路径相对工作目录
        assert!(within_tier(
            "workspace",
            "move",
            &["src/a.rs".into()],
            &roots
        ));
        // 正斜杠分隔同样算目录内
        let slashed = format!("{}/src/a.rs", s(&t.work).replace('\\', "/"));
        assert!(within_tier("workspace", "edit", &[slashed], &roots));
        assert!(!within_tier(
            "workspace",
            "edit",
            &[s(&t.outside.join("a"))],
            &roots
        ));
        // work/../outside/a 经真实路径解析落到目录外
        assert!(!within_tier(
            "workspace",
            "edit",
            &[s(&t.work.join("..").join("outside").join("a"))],
            &roots
        ));
        assert!(!within_tier("workspace", "edit", &[], &roots));
        assert!(!within_tier(
            "workspace",
            "edit",
            &[s(&t.work.join("a")), s(&t.outside.join("b"))],
            &roots
        ));
        assert!(!within_tier(
            "workspace",
            "execute",
            &[s(&t.work.join("a"))],
            &roots
        ));
        // 前缀不是目录边界：<tmp>/workspace 不是 <tmp>/work 的子路径
        assert!(!path_inside(&s(&t.workspace.join("a")), &roots));
    }

    #[test]
    fn full_allows_everything() {
        let t = Tree::new();
        let roots = t.roots();
        assert!(within_tier(
            "full",
            "execute",
            &[s(&t.outside.join("secret"))],
            &roots
        ));
        assert!(within_tier("full", "other", &[], &roots));
    }

    /// A19：不存在的路径按最近已存在祖先的真实落点判断
    #[test]
    fn new_file_judged_by_existing_ancestor() {
        let t = Tree::new();
        let roots = t.roots();
        // 父目录存在的新文件 → 在目录内
        assert!(path_inside(&s(&t.work.join("newfile.txt")), &roots));
        // 连目录带文件都不存在（剩余部分全是普通组件）→ 仍在目录内
        assert!(path_inside(
            &s(&t.work.join("newdir").join("f.txt")),
            &roots
        ));
        // root 自身也算在内
        assert!(path_inside(&s(&t.work), &roots));
        // 目录外的新文件不放行
        assert!(!path_inside(&s(&t.outside.join("newfile.txt")), &roots));
    }

    /// A19：剩余不存在部分含 .. → 落点无法验证，交审批
    #[test]
    fn remainder_with_parent_dir_refused() {
        let t = Tree::new();
        let roots = t.roots();
        // newdir 不存在：newdir/../x 的剩余部分含 .. → 不放行
        let loc = t.work.join("newdir").join("..").join("x");
        assert!(!path_inside(&s(&loc), &roots));
        // 经过真实存在目录的 .. 由 canonicalize 解析：work/sub/../f = work/f → 放行
        std::fs::create_dir_all(t.work.join("sub")).unwrap();
        let back = t.work.join("sub").join("..").join("f.txt");
        assert!(path_inside(&s(&back), &roots));
        // 同样经已存在目录 .. 到目录外 → 不放行
        let out = t.work.join("sub").join("..").join("..").join("outside");
        assert!(!path_inside(&s(&out), &roots));
    }

    /// A19：解析失败的根跳过；全部失败则都不放行
    #[test]
    fn unresolvable_roots_are_skipped() {
        let t = Tree::new();
        let roots = vec![t.root.join("nonexistent"), t.work.clone()];
        assert!(path_inside(&s(&t.work.join("f")), &roots));
        assert!(!path_inside(
            &s(&t.root.join("nonexistent").join("f")),
            &roots
        ));
        assert!(!path_inside(&s(&t.work.join("f")), &[t.root.join("nope")]));
    }

    /// A19：junction / 符号链接指向目录外 → 不自动放行
    #[test]
    fn link_to_outside_is_not_inside() {
        let t = Tree::new();
        let roots = t.roots();
        let link = t.work.join("linked");
        if !make_link(&t.outside, &link) {
            return;
        }
        std::fs::write(t.outside.join("secret.txt"), b"x").unwrap();
        // 已存在的文件：经链接解析到 outside → 不放行
        assert!(!path_inside(&s(&link.join("secret.txt")), &roots));
        // 链接下的新文件同样不放行
        assert!(!path_inside(&s(&link.join("new.txt")), &roots));
        assert!(!within_tier(
            "workspace",
            "edit",
            &[s(&link.join("secret.txt"))],
            &roots
        ));
    }

    /// A19：链接指向目录内另一处 → 放行
    #[test]
    fn link_to_inside_is_inside() {
        let t = Tree::new();
        let roots = t.roots();
        std::fs::create_dir_all(t.work.join("real")).unwrap();
        let link = t.work.join("inlink");
        if !make_link(&t.work.join("real"), &link) {
            return;
        }
        assert!(path_inside(&s(&link.join("f.txt")), &roots));
        assert!(within_tier(
            "workspace",
            "edit",
            &[s(&link.join("f.txt"))],
            &roots
        ));
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
