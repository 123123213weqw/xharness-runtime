//! 工作区 AGENTS.md 项目指令（借 Codex / OpenCode 的项目指令约定）。
//!
//! Codex 从项目根到 cwd 逐层拼接 `AGENTS.md`，OpenCode 从 cwd 向上找最近的
//! `AGENTS.md`，两者都把这份文件当作"项目级规则"，与用户指令分开。XHarness
//! 上游 `xharness-host` 的实现更窄：只读 session workspace 根的一份，不向上
//! 找、不递归。飞书 host 的 workspace 本身就是项目根，所以这里照搬上游语义：
//!
//! - 路径固定为 `<workspace>/AGENTS.md`，模型不能指定宿主路径；
//! - 读取走 `FsService` 的 contained read（拒绝符号链接、拒绝越界）；
//! - 超过 64 KiB 按字节截断并标注（Codex 同样截断，并记 warning）；
//! - 非法 UTF-8 用 lossy 解码（Codex 同样用 `from_utf8_lossy`）。
//!
//! 刻意没做：向上查找父目录的 AGENTS.md。飞书 workspace 是沙箱根，父目录是
//! 宿主，不属于这个会话；把宿主规则读进聊天会话等于越权。

use std::path::Path;

use xharness_fs::{FsError, FsService};
use xharness_prompt::PromptSection;

/// 项目指令文件名。与 Codex / OpenCode / 上游 Host 对齐。
pub const AGENTS_FILE: &str = "AGENTS.md";
/// 单份项目指令的最大字节数。与上游 `xharness-host` 的 `MAX_BYTES` 一致。
pub const MAX_BYTES: usize = 64 * 1024;
/// 读取用的逻辑 session id；`FsService` 只要求非空。
const READ_SESSION: &str = "prompt-instructions";
/// prompt 段 id，与上游 `xharness-host` 对齐，方便直接 diff。
const SECTION_ID: &str = "workspace/instructions/AGENTS.md";
/// 规则段的 id。上游是 `workspace/instructions-policy`。
const POLICY_ID: &str = "workspace/instructions-policy";

/// 告诉模型 AGENTS.md 的边界。
///
/// 原文照抄上游 `xharness-host` 的 `instructions::POLICY`，只去掉最后一句与
/// 飞书无关的 legacy memory 迁移说明。飞书路径和上游 Host 共用同一套语义，
/// 两边以后可以逐句 diff。
pub const POLICY: &str = "\
AGENTS.md is scoped project instruction, not a conversation log or Goal. Before editing below \
the root, inspect AGENTS.md in its ancestor directories inside the workspace; do not scan the \
repository. More specific instructions apply only to their directory subtree and override parent \
project rules, never user instructions, Host policies or permissions. Change rules only for an \
authorized durable project-rule change: read first, preserve unrelated content, report the diff. \
Never auto-save answers, errors, results, temporary choices or inferred preferences; suggest new \
rules.";

/// 读到的工作区指令。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loaded {
    pub text: String,
    pub truncated: bool,
}

/// 读取 `<workspace>/AGENTS.md`。
///
/// 不存在返回 `Ok(None)`；符号链接、越界、I/O 错误返回 `Err`，由调用方决定
/// 是忽略还是提示——提示词层的文件读坏了不该让整个 turn 起不来。
pub async fn load(workspace: &Path) -> Result<Option<Loaded>, String> {
    if !workspace.is_dir() {
        return Ok(None);
    }
    let fs = FsService::new(workspace).map_err(|error| format!("打开工作区失败: {error}"))?;
    let target = match fs.resolve(AGENTS_FILE) {
        Ok(target) => target,
        Err(FsError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(format!("定位 AGENTS.md 失败: {error}")),
    };
    // 多读 1 字节，用来判断是否截断。read_prefix 本身不会越过 max_bytes。
    let bytes = match fs.read_prefix(READ_SESSION, &target, MAX_BYTES + 1).await {
        Ok(bytes) => bytes,
        // resolve 不要求文件已存在，真正的“不存在”在读取时才出现。
        Err(FsError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(format!("读取 AGENTS.md 失败: {error}")),
    };
    let truncated = bytes.len() > MAX_BYTES;
    let bytes = &bytes[..bytes.len().min(MAX_BYTES)];
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if truncated {
        // 截断可能落在多字节字符中间；丢掉落单的替换字符，保留合法前缀。
        while text.ends_with('\u{fffd}') {
            text.pop();
        }
    }
    let text = text.trim_start_matches('\u{feff}').trim().to_owned();
    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(Loaded { text, truncated }))
}

/// 组装项目指令相关段落：规则段总是加，内容段仅在存在 AGENTS.md 时加。
pub fn sections(loaded: &Option<Loaded>, workspace: &Path) -> Vec<PromptSection> {
    let mut sections = vec![PromptSection::new(POLICY_ID, "2", POLICY)];
    let Some(loaded) = loaded else {
        return sections;
    };
    let root = serde_json::to_string(&workspace.to_string_lossy())
        .unwrap_or_else(|_| "\"<workspace>\"".to_owned());
    let mut content = format!(
        "Project instructions from AGENTS.md\nScope: workspace {root} and its descendants only.\n\n{}",
        loaded.text
    );
    if loaded.truncated {
        content.push_str("\n\n[AGENTS.md truncated at 64 KiB]");
    }
    sections.push(PromptSection::content_addressed(SECTION_ID, content));
    sections
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn fresh_dir(tag: &str) -> std::path::PathBuf {
        let sequence = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "xh-fs-instructions-{tag}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[tokio::test]
    async fn a_missing_file_means_no_project_instructions() {
        let dir = fresh_dir("missing");
        assert_eq!(load(&dir).await.expect("load"), None);
        let sections = sections(&None, &dir);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].id, POLICY_ID);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_workspace_file_becomes_a_scoped_section() {
        let dir = fresh_dir("root");
        std::fs::write(dir.join(AGENTS_FILE), "# 规则\n\n改动前先跑测试。\n").unwrap();
        let loaded = load(&dir).await.expect("load").expect("some");
        assert_eq!(loaded.text, "# 规则\n\n改动前先跑测试。");
        assert!(!loaded.truncated);

        let sections = sections(&Some(loaded), &dir);
        assert_eq!(sections.len(), 2);
        let content = &sections[1].content;
        assert!(content.contains("改动前先跑测试"));
        // scope 必须写清边界，避免模型把宿主规则也当项目规则。
        let root = dir.to_string_lossy();
        assert!(content.contains(&root.to_string()), "{content}");
        assert!(content.contains("descendants only"), "{content}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_huge_file_is_truncated_at_the_byte_budget() {
        let dir = fresh_dir("huge");
        // 4 字节 UTF-8 字符跨越边界，确认 lossy 解码不会 panic。
        let body = "字".repeat(MAX_BYTES);
        std::fs::write(dir.join(AGENTS_FILE), body).unwrap();
        let loaded = load(&dir).await.expect("load").expect("some");
        assert!(loaded.truncated);
        assert!(loaded.text.len() <= MAX_BYTES);
        let sections = sections(&Some(loaded), &dir);
        assert!(sections[1].content.contains("truncated at 64 KiB"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_empty_file_is_not_a_section() {
        let dir = fresh_dir("empty");
        std::fs::write(dir.join(AGENTS_FILE), "  \n\t\n").unwrap();
        assert_eq!(load(&dir).await.expect("load"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_out_of_the_workspace_is_refused() {
        use std::os::unix::fs::symlink;

        let dir = fresh_dir("symlink");
        let outside = fresh_dir("outside");
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, "HOST SECRET").unwrap();
        symlink(&secret, dir.join(AGENTS_FILE)).unwrap();

        let error = load(&dir).await.expect_err("must refuse symlink");
        assert!(
            error.contains("symbolic-link") || error.contains("symlink"),
            "{error}"
        );
        // 内容绝不能进入 prompt。
        let sections = sections(&None, &dir);
        assert!(!sections.iter().any(|s| s.content.contains("HOST SECRET")));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
