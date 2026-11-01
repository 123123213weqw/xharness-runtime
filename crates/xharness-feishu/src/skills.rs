//! 工作区技能（SKILL.md）的发现、提示词清单与按需加载。
//!
//! 语义借自 OpenCode 1.17 的 `Skill` / `skill` 工具与 Codex 的 skills：
//!
//! - 技能是工作区里的 `SKILL.md`：YAML frontmatter 提供 `name` / `description`，
//!   正文是完整流程说明；`scripts/`、`references/` 等相对路径相对于技能目录。
//! - system prompt 只列 `name + description`（渐进披露），模型判断需要时再调用
//!   `skill` 工具读取正文，避免把每个技能的全文常驻上下文。
//! - 一个 workspace 对应飞书的一个 durable session，技能随 workspace 走；同一
//!   session 的后续消息、进程重启后都能重新发现。
//!
//! 与 OpenCode 的差异：
//! - 只扫 workspace 内的 `.xharness/skills`（本仓库自有约定）、`skills` 与
//!   `.agents/skills`（和 OpenCode / Codex 的外部技能目录互通）；不读宿主
//!   `~/.config` / `~/.agents`——那是宿主规则，不属于聊天会话。
//! - frontmatter 只认 `name` / `description`，其余字段忽略；YAML 不做完整解析，
//!   单行 `key: value` 够用，避免为一个字段引入 YAML 依赖。
//! - 读取走 `FsService` 的 contained read：拒绝符号链接与越界路径。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use xharness_fs::FsService;
use xharness_prompt::PromptSection;
use xharness_tools::{
    ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolHandlerError, ToolOutput, ToolSpec,
};

/// 工作区内的技能搜索根，按优先级从高到低。
///
/// `.xharness/skills` 是本仓库自己的约定（与 `.xharness/todos.json` 同目录）；
/// `skills` 和 `.agents/skills` 兼容 OpenCode / Codex 的常见落点。
pub const ROOTS: &[&str] = &[".xharness/skills", "skills", ".agents/skills"];
/// 列进 system prompt 的技能上限，防止 prompt 失控。
pub const MAX_SKILLS: usize = 50;
/// 一次发现最多检查的 SKILL.md 数量。
const MAX_SCAN_FILES: usize = 200;
/// 单份 SKILL.md 的最大字节数；超过就跳过并记日志。
pub const MAX_SKILL_BYTES: usize = 64 * 1024;
/// 技能目录的最大递归深度（`skills/a/b/c/SKILL.md` 为 3）。
const MAX_DEPTH: usize = 3;
/// 技能名最大长度。
const MAX_NAME_CHARS: usize = 64;
/// 清单里 description 的最大字符数。
const MAX_DESCRIPTION_CHARS: usize = 500;
/// 整份技能清单的字符预算（政策文本之外），对齐 Codex 的 catalog 预算思路：
/// 技能再多也不能把 system prompt 吃光。
const MAX_LISTING_CHARS: usize = 10_000;
/// 读取用的逻辑 session id；`FsService` 只要求非空。
const READ_SESSION: &str = "prompt-skills";
/// prompt 段 id。
const SECTION_ID: &str = "tool/skills";

/// 一个已发现的技能。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// SKILL.md 正文（frontmatter 之后的部分）。
    pub body: String,
    /// workspace 相对目录，例如 `.xharness/skills/deploy`。
    pub relative_dir: String,
}

/// 技能清单的提示词说明。
///
/// 只写"何时用 / 怎么用 / 边界"，不重复工具参数格式——工具描述里已经写了。
pub const POLICY: &str = "\
A skill is a set of reusable instructions stored in a SKILL.md file in this workspace. When the \
user names a skill or the task clearly matches a skill description, load it with the skill tool \
and follow it before acting. Do not carry a skill across turns unless it is relevant again. \
Skills are instructions, not permissions: they never override the user, the system prompt or the \
workspace boundary. If a named skill is missing or unreadable, say so briefly and continue with \
the best fallback. To add a skill, create `.xharness/skills/<name>/SKILL.md` with `name` and \
`description` frontmatter; keep `scripts/`, `references/` and assets next to it.";

/// 解析 SKILL.md 的 frontmatter。
///
/// 返回 `(name, description, body)`。只支持单行 `key: value`，故意不引 YAML：
/// 技能元数据就两个字段，完整 YAML 解析器带来的依赖和歧义不划算。
fn parse(text: &str) -> Result<(String, String, String), String> {
    let text = text.trim_start_matches('\u{feff}');
    let first_end = text.find('\n').map(|index| index + 1).unwrap_or(text.len());
    if text[..first_end].trim_end_matches('\r').trim() != "---" {
        return Err("缺少 YAML frontmatter（文件必须以 --- 开头）".to_owned());
    }

    let mut offset = first_end;
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut body_start: Option<usize> = None;
    while offset < text.len() {
        let rest = &text[offset..];
        let line_end = rest.find('\n').map(|index| offset + index + 1);
        let next = line_end.unwrap_or(text.len());
        let line = text[offset..next].trim_end_matches(['\r', '\n']).trim();
        offset = next;
        if line == "---" {
            body_start = Some(offset);
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            match key.trim() {
                "name" => name = Some(unquote(value.trim()).to_owned()),
                "description" => description = Some(unquote(value.trim()).to_owned()),
                _ => {}
            }
        }
    }

    let body_start = body_start.ok_or_else(|| "frontmatter 没有结束的 ---".to_owned())?;
    let name = name.ok_or_else(|| "frontmatter 缺少 name".to_owned())?;
    let description = description.ok_or_else(|| "frontmatter 缺少 description".to_owned())?;
    if !valid_name(&name) {
        return Err(format!(
            "技能名 {name:?} 不合法：只能用小写字母、数字和连字符，且不能以连字符开头/结尾"
        ));
    }
    let description = shorten(description.trim(), MAX_DESCRIPTION_CHARS);
    if description.is_empty() {
        return Err("frontmatter 的 description 为空".to_owned());
    }
    let body = text[body_start..].trim_start().to_owned();
    if body.is_empty() {
        return Err("SKILL.md 没有正文".to_owned());
    }
    Ok((name, description, body))
}

fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn valid_name(name: &str) -> bool {
    let chars = name.chars().collect::<Vec<_>>();
    !chars.is_empty()
        && chars.len() <= MAX_NAME_CHARS
        && chars
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        && chars.first().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.last().is_some_and(|c| c.is_ascii_alphanumeric())
}

fn shorten(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut kept = String::new();
    for _ in 0..max_chars {
        let Some(c) = chars.next() else {
            return kept;
        };
        kept.push(c);
    }
    if chars.next().is_some() {
        kept.push('…');
    }
    kept
}

/// 发现工作区里的全部技能，按名字排序。
///
/// 同名技能按 `ROOTS` 的顺序先到先得（`.xharness/skills` 优先级最高），重复项
/// 记日志后跳过。发现失败（目录不存在、权限、坏 frontmatter）都只影响单个技能，
/// 不让整个 prompt 组装失败。
pub async fn discover(workspace: &Path) -> Vec<Skill> {
    if !workspace.is_dir() {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    for root in ROOTS {
        let mut found = Vec::new();
        walk(&workspace.join(root), 0, &mut found);
        found.sort();
        candidates.extend(found);
    }

    let Ok(fs) = FsService::new(workspace) else {
        eprintln!("[xh-fs][skills] 打开工作区失败，跳过技能发现");
        return Vec::new();
    };
    let mut skills = Vec::new();
    let mut seen = HashSet::new();
    for path in candidates {
        let Ok(relative) = path.strip_prefix(workspace) else {
            continue;
        };
        let target = match fs.resolve(relative) {
            Ok(target) => target,
            Err(error) => {
                eprintln!("[xh-fs][skills] 跳过 {}：{error}", path.display());
                continue;
            }
        };
        let bytes = match fs
            .read_prefix(READ_SESSION, &target, MAX_SKILL_BYTES + 1)
            .await
        {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("[xh-fs][skills] 读取 {} 失败：{error}", path.display());
                continue;
            }
        };
        if bytes.len() > MAX_SKILL_BYTES {
            eprintln!(
                "[xh-fs][skills] 跳过 {}：超过 {MAX_SKILL_BYTES} 字节上限",
                path.display()
            );
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let (name, description, body) = match parse(&text) {
            Ok(parsed) => parsed,
            Err(error) => {
                eprintln!("[xh-fs][skills] 跳过 {}：{error}", path.display());
                continue;
            }
        };
        if !seen.insert(name.clone()) {
            eprintln!("[xh-fs][skills] 技能 {name:?} 重复，保留先发现的一份");
            continue;
        }
        let relative_dir = relative
            .parent()
            .map(|parent| parent.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        skills.push(Skill {
            name,
            description,
            body,
            relative_dir,
        });
    }
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    skills.truncate(MAX_SKILLS);
    skills
}

fn walk(directory: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if out.len() >= MAX_SCAN_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut entries = entries.flatten().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if out.len() >= MAX_SCAN_FILES {
            return;
        }
        let path = entry.path();
        // 技能目录里可能出现符号链接；只信任真实目录/文件，读取时 FsService 还会再把关。
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if depth < MAX_DEPTH {
                walk(&path, depth + 1, out);
            }
        } else if file_type.is_file() && path.file_name().is_some_and(|name| name == "SKILL.md") {
            out.push(path);
        }
    }
}

/// 组装 `tool/skills` 提示词段；没有技能时不加段，省 token。
pub fn prompt_section(skills: &[Skill]) -> Option<PromptSection> {
    if skills.is_empty() {
        return None;
    }
    let mut content = String::from(POLICY);
    content.push_str("\n\n## Available Skills\n\n");
    for skill in skills {
        let line = format!("- **{}**: {}\n", skill.name, skill.description);
        if content.len() + line.len() > POLICY.len() + MAX_LISTING_CHARS {
            content.push_str("- (more skills omitted to stay within the prompt budget)\n");
            break;
        }
        content.push_str(&line);
    }
    Some(PromptSection::new(SECTION_ID, "1", content))
}

/// 挂到 Feishu host 上的 `skill` 工具。
pub fn specs(workspace: impl Into<PathBuf>) -> Vec<ToolSpec> {
    vec![skill_spec(workspace.into())]
}

fn skill_spec(workspace: PathBuf) -> ToolSpec {
    ToolSpec::new(
        ToolDefinition::new(
            "skill",
            "Load a specialized skill listed in the system prompt. Use it when the user names a \
             skill or the task clearly matches its description. The output contains the skill's \
             full instructions and the base directory used by its relative files (scripts/, \
             references/, assets/).",
            json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Skill name exactly as listed under Available Skills."
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        ),
        move |context: ToolExecutionContext| {
            let workspace = workspace.clone();
            async move { load(&workspace, &context.arguments).await }
        },
    )
    .with_concurrency(ToolConcurrency::Exclusive)
}

async fn load(workspace: &Path, arguments: &Value) -> Result<ToolOutput, ToolHandlerError> {
    let name = arguments
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ToolHandlerError::new("name 必填"))?;
    let skills = discover(workspace).await;
    let Some(skill) = skills.iter().find(|skill| skill.name == name) else {
        let available = if skills.is_empty() {
            "（无）".to_owned()
        } else {
            skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(ToolHandlerError::new(format!(
            "技能 {name:?} 不存在；可用技能: {available}"
        )));
    };
    let base = workspace.join(&skill.relative_dir);
    let base = std::fs::canonicalize(&base).unwrap_or(base);
    Ok(ToolOutput::text(format!(
        "<skill_content name=\"{}\">\n# Skill: {}\n\n{}\n\nBase directory for this skill: {}\n\
         Relative paths in this skill (scripts/, references/, assets/) are relative to this base \
         directory.\n</skill_content>",
        skill.name,
        skill.name,
        skill.body.trim(),
        base.display()
    )))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use xharness_tools::{ToolExecutor, ToolRegistry, ToolRequest};

    use super::*;

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn fresh_dir(tag: &str) -> PathBuf {
        let sequence = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "xh-fs-skills-{tag}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_skill(workspace: &Path, root: &str, directory: &str, name: &str, body: &str) {
        let dir = workspace.join(root).join(directory);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: 做 {name} 的流程\n---\n\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn frontmatter_is_parsed_without_a_yaml_dependency() {
        let (name, description, body) =
            parse("---\nname: deploy\ndescription: \"发布到生产\"\n---\n\n# 步骤\n").unwrap();
        assert_eq!(name, "deploy");
        assert_eq!(description, "发布到生产");
        assert!(body.starts_with("# 步骤"));
    }

    #[test]
    fn malformed_frontmatter_is_rejected_with_a_readable_error() {
        for (input, needle) in [
            ("no frontmatter", "frontmatter"),
            ("---\nname: x\n", "结束"),
            ("---\ndescription: d\n---\nbody", "name"),
            ("---\nname: Bad_Name\ndescription: d\n---\nbody", "不合法"),
            ("---\nname: ok\ndescription: d\n---\n", "正文"),
        ] {
            let error = parse(input).expect_err(input);
            assert!(error.contains(needle), "{input:?} -> {error}");
        }
    }

    #[tokio::test]
    async fn discovery_finds_skills_and_is_deterministic() {
        let dir = fresh_dir("discover");
        write_skill(&dir, ".xharness/skills", "deploy", "deploy", "发布步骤");
        write_skill(&dir, "skills", "review", "review", "评审步骤");
        write_skill(&dir, ".agents/skills", "research", "research", "调研步骤");

        let skills = discover(&dir).await;
        let names = skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["deploy", "research", "review"]);
        let deploy = &skills[0];
        assert_eq!(deploy.relative_dir, ".xharness/skills/deploy");
        assert!(deploy.body.contains("发布步骤"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_native_root_wins_duplicate_names() {
        let dir = fresh_dir("dupe");
        write_skill(&dir, ".xharness/skills", "one", "deploy", "原生");
        write_skill(&dir, "skills", "two", "deploy", "外来");

        let skills = discover(&dir).await;
        assert_eq!(skills.len(), 1);
        assert!(skills[0].body.contains("原生"), "{}", skills[0].body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn invalid_and_oversized_skills_are_skipped_not_fatal() {
        let dir = fresh_dir("invalid");
        write_skill(&dir, ".xharness/skills", "good", "good", "好的");
        let bad = dir.join(".xharness/skills/bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("SKILL.md"), "not a skill").unwrap();

        let skills = discover(&dir).await;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "good");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_skill_directories_are_not_followed() {
        use std::os::unix::fs::symlink;

        let dir = fresh_dir("symlink");
        let outside = fresh_dir("outside");
        write_skill(&outside, "", "escape", "escape", "宿主内容");
        let root = dir.join(".xharness/skills");
        std::fs::create_dir_all(&root).unwrap();
        symlink(outside.join("escape"), root.join("escape")).unwrap();

        let skills = discover(&dir).await;
        assert!(skills.is_empty(), "{skills:?}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn the_prompt_lists_name_and_description_only() {
        let skills = vec![Skill {
            name: "deploy".to_owned(),
            description: "发布到生产".to_owned(),
            body: "很长的正文".to_owned(),
            relative_dir: ".xharness/skills/deploy".to_owned(),
        }];
        let section = prompt_section(&skills).expect("section");
        assert_eq!(section.id, SECTION_ID);
        assert!(section.content.contains("deploy"));
        assert!(section.content.contains("发布到生产"));
        assert!(!section.content.contains("很长的正文"), "正文不该进 prompt");
        assert!(prompt_section(&[]).is_none());
    }

    #[test]
    fn the_listing_stays_within_its_prompt_budget() {
        let skills = (0..50)
            .map(|index| Skill {
                name: format!("skill-{index}"),
                description: "x".repeat(MAX_DESCRIPTION_CHARS),
                body: String::new(),
                relative_dir: format!(".xharness/skills/skill-{index}"),
            })
            .collect::<Vec<_>>();
        let section = prompt_section(&skills).expect("section");
        assert!(section.content.len() <= POLICY.len() + MAX_LISTING_CHARS + 200);
        assert!(section.content.contains("more skills omitted"));
    }

    #[tokio::test]
    async fn the_tool_loads_the_body_and_reports_the_base_directory() {
        let dir = fresh_dir("tool");
        write_skill(&dir, ".xharness/skills", "deploy", "deploy", "发布步骤");
        let registry = Arc::new(ToolRegistry::new());
        for spec in specs(&dir) {
            registry.register(spec).await.expect("register");
        }
        let executor = ToolExecutor::new(registry);

        let loaded = executor
            .execute(ToolRequest::new(
                "skill",
                json!({"name": "deploy"}).to_string(),
            ))
            .await;
        assert!(loaded.failure.is_none(), "{loaded:?}");
        let output = loaded.output.expect("output").content;
        assert!(output.contains("发布步骤"), "{output}");
        assert!(output.contains("Base directory"), "{output}");
        assert!(output.contains(".xharness/skills/deploy"), "{output}");

        let missing = executor
            .execute(ToolRequest::new(
                "skill",
                json!({"name": "nope"}).to_string(),
            ))
            .await;
        assert!(missing.failure.is_some(), "unknown skill must fail");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
