//! zcode 风格的会话任务清单（TodoRead / TodoWrite）。
//!
//! zcode 0.17 用 `TodoRead` / `TodoWrite` 两个工具维护一份会话级清单：模型每次提交
//! 完整列表，用 `pending` / `in_progress` / `completed` 表达进度，清单同时是给用户
//! 看的工作计划。这里落一个同样语义的小版本：
//!
//! - 状态存在会话 workspace 里（`<workspace>/.xharness/todos.json`）。Feishu 侧一个
//!   workspace 对应一个 durable session，所以进程重启、同会话后续消息都能读回清单。
//! - 路径固定，不由模型参数决定，因此 host 直接写宿主文件；清单是 host 自己的会话
//!   状态，不需要再过一遍沙箱。
//! - 每次 `todo_write` 覆盖整份清单；同一时刻最多一项 `in_progress`，否则报错——让
//!   "两件事同时在跑"变成模型能看见的失败，而不是悄悄记错的进度。
//!
//! 刻意没做的：zcode 在模型忘记更新时注入 `todo_reminder` 系统消息。那需要一个
//! 回合内注入通道，为一个工具引入它不划算；先靠工具描述和 system prompt 约束。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use xharness_tools::{
    ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolHandlerError, ToolOutput, ToolSpec,
};

/// 相对 workspace 的清单路径。
pub const TODO_PATH: &str = ".xharness/todos.json";
/// 一份清单最多几项：防止模型把任务拆到失控。
pub const MAX_TODOS: usize = 50;
/// 单项描述的最大字符数。
pub const MAX_CONTENT_CHARS: usize = 500;
/// 当前文件格式版本。
const TODO_FILE_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoPriority {
    High,
    Medium,
    Low,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<TodoPriority>,
}

/// 模型和 UI 都需要的计数视图。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TodoSummary {
    pub total: usize,
    pub pending: usize,
    pub in_progress: usize,
    pub completed: usize,
}

impl TodoSummary {
    pub fn of(todos: &[TodoItem]) -> Self {
        let mut summary = Self {
            total: todos.len(),
            ..Self::default()
        };
        for item in todos {
            match item.status {
                TodoStatus::Pending => summary.pending += 1,
                TodoStatus::InProgress => summary.in_progress += 1,
                TodoStatus::Completed => summary.completed += 1,
            }
        }
        summary
    }
}

#[derive(Serialize, Deserialize)]
struct TodoFile {
    version: u32,
    todos: Vec<TodoItem>,
}

/// 校验并规范化一份完整清单。
///
/// 返回的错误是给模型看的：它要能指出第几项、错在哪，模型才有机会一次改对。
pub fn normalize(todos: Vec<TodoItem>) -> Result<Vec<TodoItem>, String> {
    if todos.len() > MAX_TODOS {
        return Err(format!(
            "清单最多 {MAX_TODOS} 项，当前提交了 {} 项",
            todos.len()
        ));
    }
    let mut in_progress = 0usize;
    let mut normalized = Vec::with_capacity(todos.len());
    for (index, mut item) in todos.into_iter().enumerate() {
        let content = item.content.trim().to_owned();
        if content.is_empty() {
            return Err(format!("第 {} 项的 content 为空", index + 1));
        }
        let chars = content.chars().count();
        if chars > MAX_CONTENT_CHARS {
            return Err(format!(
                "第 {} 项的 content 有 {chars} 字符，超过 {MAX_CONTENT_CHARS} 上限",
                index + 1
            ));
        }
        if item.status == TodoStatus::InProgress {
            in_progress += 1;
        }
        item.content = content;
        normalized.push(item);
    }
    if in_progress > 1 {
        return Err(format!(
            "同一时刻最多一项 in_progress，当前有 {in_progress} 项；完成后先标 completed 再开始下一项"
        ));
    }
    Ok(normalized)
}

/// 读取清单。缺失视为空清单；损坏则报错，不静默丢进度。
pub fn read(workspace: &Path) -> Result<Vec<TodoItem>, String> {
    let path = workspace.join(TODO_PATH);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let file: TodoFile = serde_json::from_slice(&bytes)
                .map_err(|error| format!("{} 解析失败: {error}", path.display()))?;
            if file.version != TODO_FILE_VERSION {
                return Err(format!(
                    "{} 的版本是 {}，本 host 只认 {TODO_FILE_VERSION}",
                    path.display(),
                    file.version
                ));
            }
            Ok(file.todos)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("读取 {} 失败: {error}", path.display())),
    }
}

/// 原子写入清单：先写同目录临时文件再 rename，避免半截 JSON。
pub fn write(workspace: &Path, todos: &[TodoItem]) -> Result<(), String> {
    let path = workspace.join(TODO_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建 {} 失败: {error}", parent.display()))?;
    }
    let body = serde_json::to_vec_pretty(&TodoFile {
        version: TODO_FILE_VERSION,
        todos: todos.to_vec(),
    })
    .map_err(|error| format!("序列化清单失败: {error}"))?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, body)
        .map_err(|error| format!("写入 {} 失败: {error}", temporary.display()))?;
    match std::fs::rename(&temporary, &path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(format!("替换 {} 失败: {error}", path.display()))
        }
    }
}

/// 挂到 Feishu host 上的两个工具：写入（全量替换）与读取。
pub fn specs(workspace: impl Into<PathBuf>) -> Vec<ToolSpec> {
    let workspace = workspace.into();
    vec![write_spec(workspace.clone()), read_spec(workspace)]
}

fn write_spec(workspace: PathBuf) -> ToolSpec {
    ToolSpec::new(
        ToolDefinition::new(
            "todo_write",
            "Create and update the task list for the current session; the list is the working plan shown to the user.\n\nEach call replaces the whole list: send every item with content, status and optional priority. Keep at most one item in_progress, mark it completed before starting the next, and drop items that are no longer relevant. Use todo_read to inspect the current list.",
            json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "Complete replacement list, ordered as the work should proceed.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string", "description": "Imperative description of one unit of work."},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                                "priority": {"type": "string", "enum": ["high", "medium", "low"]}
                            },
                            "required": ["content", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["todos"],
                "additionalProperties": false
            }),
        ),
        move |context: ToolExecutionContext| {
            let workspace = workspace.clone();
            async move { apply_write(&workspace, &context.arguments) }
        },
    )
    .with_concurrency(ToolConcurrency::Exclusive)
}

fn read_spec(workspace: PathBuf) -> ToolSpec {
    ToolSpec::new(
        ToolDefinition::new(
            "todo_read",
            "Read the current session task list and its status counts.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        ),
        move |_context: ToolExecutionContext| {
            let workspace = workspace.clone();
            async move {
                let todos = read(&workspace).map_err(ToolHandlerError::new)?;
                Ok(ToolOutput::text(
                    json!({"todos": todos, "summary": TodoSummary::of(&todos)}).to_string(),
                ))
            }
        },
    )
    .with_concurrency(ToolConcurrency::Exclusive)
}

fn apply_write(workspace: &Path, arguments: &Value) -> Result<ToolOutput, ToolHandlerError> {
    let raw = arguments
        .get("todos")
        .cloned()
        .ok_or_else(|| ToolHandlerError::new("todos 必填"))?;
    let todos: Vec<TodoItem> = serde_json::from_value(raw)
        .map_err(|error| ToolHandlerError::new(format!("todos 格式不正确: {error}")))?;
    let todos = normalize(todos).map_err(ToolHandlerError::new)?;
    let previous = read(workspace).map_err(ToolHandlerError::new)?;
    write(workspace, &todos).map_err(ToolHandlerError::new)?;
    Ok(ToolOutput::text(
        json!({
            "todos": todos,
            "previousTodos": previous,
            "summary": TodoSummary::of(&todos),
        })
        .to_string(),
    ))
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
            "xh-fs-todo-{tag}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.to_owned(),
            status,
            priority: None,
        }
    }

    #[test]
    fn write_then_read_roundtrips_with_summary() {
        let dir = fresh_dir("roundtrip");
        let todos = vec![
            item("inspect", TodoStatus::Completed),
            item("edit", TodoStatus::InProgress),
            item("test", TodoStatus::Pending),
        ];
        write(&dir, &todos).expect("write");
        assert_eq!(read(&dir).expect("read"), todos);
        assert_eq!(
            TodoSummary::of(&todos),
            TodoSummary {
                total: 3,
                pending: 1,
                in_progress: 1,
                completed: 1,
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_an_empty_list() {
        let dir = fresh_dir("missing");
        assert!(read(&dir).expect("read").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_in_progress_items_are_rejected() {
        let error = normalize(vec![
            item("a", TodoStatus::InProgress),
            item("b", TodoStatus::InProgress),
        ])
        .expect_err("must reject");
        assert!(error.contains("最多一项 in_progress"), "{error}");
    }

    #[test]
    fn blank_and_overlong_content_are_rejected() {
        let error = normalize(vec![item("   ", TodoStatus::Pending)]).expect_err("blank");
        assert!(error.contains("为空"), "{error}");

        let long = "x".repeat(MAX_CONTENT_CHARS + 1);
        let error = normalize(vec![item(&long, TodoStatus::Pending)]).expect_err("long");
        assert!(error.contains("超过"), "{error}");
    }

    #[test]
    fn content_is_trimmed_and_item_count_is_capped() {
        let normalized = normalize(vec![item("  keep me  ", TodoStatus::Pending)]).expect("trim");
        assert_eq!(normalized[0].content, "keep me");

        let many = (0..=MAX_TODOS)
            .map(|index| item(&format!("task {index}"), TodoStatus::Pending))
            .collect::<Vec<_>>();
        let error = normalize(many).expect_err("too many");
        assert!(error.contains("最多"), "{error}");
    }

    #[test]
    fn a_corrupt_file_is_reported_instead_of_dropped() {
        let dir = fresh_dir("corrupt");
        let path = dir.join(TODO_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        let error = read(&dir).expect_err("corrupt");
        assert!(error.contains("解析失败"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tools_replace_the_full_list_and_read_it_back() {
        let dir = fresh_dir("tools");
        let registry = Arc::new(ToolRegistry::new());
        for spec in specs(&dir) {
            registry.register(spec).await.expect("register");
        }
        let executor = ToolExecutor::new(registry);

        let first = executor
            .execute(ToolRequest::new(
                "todo_write",
                json!({"todos": [
                    {"content": "inspect", "status": "in_progress", "priority": "high"},
                    {"content": "edit", "status": "pending"}
                ]})
                .to_string(),
            ))
            .await;
        assert!(first.is_ok(), "{first:?}");

        let replaced = executor
            .execute(ToolRequest::new(
                "todo_write",
                json!({"todos": [
                    {"content": "inspect", "status": "completed"},
                    {"content": "edit", "status": "in_progress"}
                ]})
                .to_string(),
            ))
            .await;
        assert!(replaced.is_ok(), "{replaced:?}");
        let body: Value =
            serde_json::from_str(&replaced.output.expect("output").content).expect("json");
        assert_eq!(body["summary"]["completed"], 1);
        assert_eq!(body["summary"]["in_progress"], 1);
        assert_eq!(body["previousTodos"].as_array().unwrap().len(), 2);

        let read_back = executor.execute(ToolRequest::new("todo_read", "{}")).await;
        assert!(read_back.is_ok(), "{read_back:?}");
        let body: Value =
            serde_json::from_str(&read_back.output.expect("output").content).expect("json");
        assert_eq!(body["todos"][0]["content"], "inspect");
        assert_eq!(body["todos"][1]["status"], "in_progress");

        let invalid = executor
            .execute(ToolRequest::new(
                "todo_write",
                json!({"todos": [
                    {"content": "a", "status": "in_progress"},
                    {"content": "b", "status": "in_progress"}
                ]})
                .to_string(),
            ))
            .await;
        assert!(
            invalid.failure.is_some(),
            "duplicate in_progress must fail: {invalid:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
