//! Axum 任务入口 + 轮询触发器 + 崩溃恢复。
//!
//! POST /tasks 提交任务,GET /results/{id} 轮询结果;poller 扫描仓库评论,
//! `/xh` 开头且作者为 repo owner 的评论触发。落在 PR 上的任务自动进入
//! review 模式:clone + checkout PR head + diff 注入 + P0/P1 分级输出 +
//! 复现信息块。每个 (repo, issue) 一个持久 durable session;任务落
//! task.json,启动时恢复未完成输入(幂等重放)。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use xharness_core::LoopStatus;
use xharness_host::{
    AgentRuntime, AgentTurnRequest, DurableLoopAgentRuntime, ModelRoute, PermissionPreset,
};
use xharness_session::{Message, MessageRole};

use crate::client::session_marker;
use crate::factory::GhToolConfig;
use crate::gitprep::PrContext;

#[derive(Clone)]
pub struct GhHostState {
    pub runtime: Arc<DurableLoopAgentRuntime>,
    pub route: ModelRoute,
    pub results: Arc<Mutex<HashMap<String, TaskOutcome>>>,
    pub gh: Option<GhToolConfig>,
    /// 每个任务的 workspace 根目录;session 工作目录为 workspace_root/{session_id}。
    pub workspace_root: PathBuf,
    /// 设置后 POST /tasks 必须携带 `Authorization: Bearer <secret>`。
    pub task_secret: Option<String>,
    /// 轮询触发器已处理的评论 id(内存级;重启后由启动预填覆盖)。
    pub seen_comments: Arc<Mutex<HashSet<u64>>>,
}

#[derive(Clone, Debug)]
pub struct TaskOutcome {
    pub status: String,
    pub final_text: String,
}

pub fn router(state: GhHostState) -> Router {
    Router::new()
        .route("/tasks", post(submit_task))
        .route("/results/{session_id}", get(get_result))
        .with_state(state)
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct TaskPayload {
    pub repo: String,
    pub issue: u64,
    pub title: String,
    pub body: String,
    /// 触发源标识,进入 durable 消息 id 以便幂等;HTTP 直发缺省 "v1"。
    #[serde(default)]
    pub message_tag: Option<String>,
    /// 存在即 review 模式(PR 已检出、diff 就绪)。
    #[serde(default)]
    pub pr: Option<PrContext>,
}

pub fn session_id_for(repo: &str, issue: u64) -> String {
    format!("gh-{}-{issue}", repo.replace('/', "--"))
}

async fn submit_task(
    State(state): State<GhHostState>,
    headers: axum::http::HeaderMap,
    Json(payload): Json<TaskPayload>,
) -> impl IntoResponse {
    if let Some(secret) = &state.task_secret {
        let provided = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default();
        if provided != secret {
            return (StatusCode::UNAUTHORIZED, Json(json!({"error": "鉴权失败"})));
        }
    }
    match submit_inner(&state, payload, false).await {
        Ok(session_id) => (StatusCode::ACCEPTED, Json(json!({"sessionId": session_id}))),
        Err((code, message)) => (code, Json(json!({"error": message}))),
    }
}

fn build_prompt(payload: &TaskPayload, workspace: &std::path::Path) -> String {
    match &payload.pr {
        Some(pr) => {
            let inline_note = if pr.diff_truncated {
                "(内联 diff 已截断,完整 diff 见 diff.patch)"
            } else {
                "(完整 diff)"
            };
            format!(
                "你是资深代码评审。评审 PR #{number}:「{title}」(head `{head}` ← base `{base}`,分支 {head_ref} → {base_ref})。\n\
                 \n\
                 仓库已检出到你的工作目录:`{workspace}`(full access,无审批,bash/read/write/grep 可用)。\n\
                 diff stat:\n```\n{stat}\n```\n\
                 diff {note}:\n```diff\n{diff}\n```\n\
                 \n\
                 评审要求:\n\
                 1. 按严重度分级:**P0 必须修复**(bug/安全/数据丢失/破坏兼容)、**P1 建议修复**(逻辑隐患/缺测试/性能)、**P2 可选**(风格/命名)。\n\
                 2. 每条 finding 的第一行必须是代码引用,格式 `path/to/file.rs:42`;无引用的 finding 不算数。\n\
                 3. 每条 finding 给出可执行的验证方式(命令、测试名或构造步骤);不确定的明确标注「未验证」。\n\
                 4. 可以在工作目录里跑命令核实(read 文件、grep、跑测试),核实过的标注「已验证」。\n\
                 \n\
                 输出格式(严格遵守,不要输出别的章节):\n\
                 ## 总结\n<一两段>\n## P0\n<列表或「无」>\n## P1\n<列表或「无」>\n## P2\n<列表或「无」>\n## 验证建议\n<列表>\n\
                 \n\
                 用户指令:{body}",
                number = pr.number,
                title = pr.title,
                head = &pr.head_sha[..pr.head_sha.len().min(12)],
                base = &pr.base_sha[..pr.base_sha.len().min(12)],
                head_ref = pr.head_ref,
                base_ref = pr.base_ref,
                workspace = workspace.display(),
                stat = pr.diff_stat,
                note = inline_note,
                diff = pr.diff_inline,
                body = payload.body,
            )
        }
        None => format!(
            "仓库 {} issue #{}: {}\n\n{}\n\n你的工作目录(已存在,可用 bash/read/write 等工具直接操作,无审批):{}",
            payload.repo,
            payload.issue,
            payload.title,
            payload.body,
            workspace.display()
        ),
    }
}

/// HTTP 与轮询触发器共用的任务提交入口。
///
/// `recovery` 模式:admit_turn 因消息 id 已存在而被拒时仍尝试 start_turn
/// ——崩溃前已落账但未跑完的输入靠这个路径续跑(durable inbox 去重保证
/// 不会重复消费已完成的输入)。
pub async fn submit_inner(
    state: &GhHostState,
    payload: TaskPayload,
    recovery: bool,
) -> Result<String, (StatusCode, String)> {
    if let Some(gh) = &state.gh {
        if payload.repo != gh.repo {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "repo {} 不在本 gh-host 的服务范围({})内",
                    payload.repo, gh.repo
                ),
            ));
        }
    }
    let session_id = session_id_for(&payload.repo, payload.issue);
    let workspace = state.workspace_root.join(&session_id);
    std::fs::create_dir_all(&workspace).expect("创建 workspace 目录");
    let task_file = workspace.join("task.json");
    if let Ok(serialized) = serde_json::to_string_pretty(&payload) {
        let _ = std::fs::write(&task_file, serialized);
    }
    let message_tag = payload
        .message_tag
        .clone()
        .unwrap_or_else(|| "v1".to_owned());
    let request = AgentTurnRequest {
        session_id: session_id.clone(),
        cwd: workspace.to_string_lossy().to_string(),
        route: state.route.clone(),
        permission: PermissionPreset::DangerFullAccess,
        prompt: None,
        messages: vec![Message {
            id: Some(format!(
                "gh-{}/{}/{}",
                payload.repo, payload.issue, message_tag
            )),
            role: MessageRole::User,
            content: build_prompt(&payload, &workspace),
            ..Default::default()
        }],
        input_metadata: Some(json!({
            "source": "gh",
            "repo": payload.repo,
            "issue": payload.issue,
        })),
    };
    if let Err(error) = state.runtime.admit_turn(request.clone()).await {
        if !recovery {
            return Err((StatusCode::INTERNAL_SERVER_ERROR, error.to_string()));
        }
        eprintln!("[xh-gh] 恢复: admit_turn 被拒({error}),尝试直接 start_turn 续跑 pending 输入");
    }
    let mut turn = match state.runtime.start_turn(request).await {
        Ok(turn) => turn,
        Err(error) => return Err((StatusCode::INTERNAL_SERVER_ERROR, error.to_string())),
    };
    let results = Arc::clone(&state.results);
    let tracked_session = session_id.clone();
    let gh = state.gh.clone();
    let workspace_root = state.workspace_root.clone();
    let payload = Arc::new(payload);
    let issue = payload.issue;
    tokio::spawn(async move {
        while let Some(event) = turn.next_event().await {
            if std::env::var_os("XH_GH_TRACE").is_some() {
                eprintln!("[xh-gh][event] {:?}", event.kind);
            }
        }
        let result = turn.result().await;
        let status = match result.status {
            LoopStatus::Completed => "done",
            _ => "stopped",
        };
        let final_text = if result.final_text.is_empty() {
            "(任务结束,模型没有产生文本输出)"
        } else {
            result.final_text.as_str()
        };
        results.lock().unwrap().insert(
            tracked_session.clone(),
            TaskOutcome {
                status: status.to_owned(),
                final_text: final_text.to_owned(),
            },
        );
        // 完成标记:崩溃恢复据此跳过已完成的任务(at-least-once 语义)。
        {
            let marker = std::path::PathBuf::from(format!(
                "{}/{tracked_session}/done.marker",
                workspace_root.display()
            ));
            let _ = std::fs::write(
                &marker,
                serde_json::json!({"status": status, "finalText": final_text}).to_string(),
            );
        }
        if let Some(gh) = gh {
            let (body, labels) = render_result(&tracked_session, &payload, status, final_text);
            if let Err(error) = gh
                .client
                .upsert_session_comment(&gh.repo, issue, &session_marker(&tracked_session), &body)
                .await
            {
                eprintln!("[xh-gh] 结果评论写入失败: {error}");
            }
            for label in labels {
                if let Err(error) = gh.client.add_label(&gh.repo, issue, label).await {
                    eprintln!("[xh-gh] label 写入失败: {error}");
                }
            }
        }
    });
    Ok(session_id)
}

/// 结果评论渲染:review 任务带复现信息块与专属 label;P0 出现时加标。
fn render_result(
    session_id: &str,
    payload: &TaskPayload,
    status: &str,
    final_text: &str,
) -> (String, Vec<&'static str>) {
    match &payload.pr {
        Some(pr) => {
            let mut labels = vec!["xh:reviewed"];
            let has_p0 = final_text.contains("## P0") && !final_text.contains("## P0\n无");
            if has_p0 {
                labels.push("xh:review-p0");
            }
            let body = format!(
                "## xh-gh Review · PR #{number} @ `{head}`\n\n{final_text}\n\n---\n\
                 **复现信息**(host 生成,可直接核对)\n\
                 - repo `{repo}` · PR #{number} · head `{head}` ← base `{base}`\n\
                 - diff stat:\n```\n{stat}```\n\
                 - 完整 diff:workspace `diff.patch`;评审轨迹:durable session `{session}`(append-only JSONL)\n\
                 - 复跑:在本 PR 再评论一条 `/xh ...` 即可刷新本评审\n\
                 \n---\n状态: `{status}`",
                number = pr.number,
                head = &pr.head_sha[..pr.head_sha.len().min(12)],
                base = &pr.base_sha[..pr.base_sha.len().min(12)],
                repo = payload.repo,
                stat = pr.diff_stat,
                session = session_id,
                status = status,
            );
            (body, labels)
        }
        None => (
            format!("## xh-gh 任务结果\n\n{final_text}\n\n---\n状态: `{status}`"),
            vec!["xh:done"],
        ),
    }
}

async fn get_result(
    State(state): State<GhHostState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    let outcome = state.results.lock().unwrap().get(&session_id).cloned();
    match outcome {
        Some(outcome) => Json(json!({
            "status": outcome.status,
            "finalText": outcome.final_text,
        })),
        None => Json(json!({"status": "unknown"})),
    }
}

fn repo_owner(repo: &str) -> &str {
    repo.split('/').next().unwrap_or(repo)
}

/// 轮询单次扫描:发现本仓库 owner 的新 `/xh` 评论即提交任务。
/// 评论落在 PR 上时自动进入 review 模式(clone + diff 注入)。
/// 返回触发的任务数。
pub async fn poll_once(state: &GhHostState) -> Result<usize, String> {
    let gh = state.gh.as_ref().ok_or("gh 未配置")?;
    let comments = gh.client.list_repo_comments(&gh.repo).await?;
    let mut dispatched = 0;
    for comment in comments {
        let body = comment.body.trim_start();
        if !body.starts_with("/xh") || comment.user.login != repo_owner(&gh.repo) {
            continue;
        }
        let already_seen = state.seen_comments.lock().unwrap().insert(comment.id);
        if !already_seen {
            continue;
        }
        let Some(issue_number) = comment.issue_number() else {
            continue;
        };
        let issue = gh.client.get_issue(&gh.repo, issue_number).await?;
        let task_text = body.replacen("/xh", "", 1).trim().to_owned();
        let session_id = session_id_for(&gh.repo, issue_number);
        let workspace = state.workspace_root.join(&session_id);
        let pr = if issue.pull_request.is_some() {
            let pull = gh.client.get_pull(&gh.repo, issue_number).await?;
            let origin = gh
                .origin
                .clone()
                .unwrap_or_else(|| format!("https://github.com/{}.git", gh.repo));
            let staging = PathBuf::from(format!("{}.new", workspace.to_string_lossy()));
            let _ = std::fs::remove_dir_all(&staging);
            let prepared = crate::gitprep::prepare_review_workspace(
                &origin,
                &staging,
                crate::gitprep::PullSpec {
                    number: pull.number,
                    head_sha: &pull.head.sha,
                    base_sha: &pull.base.sha,
                    head_ref: &pull.head.ref_name,
                    base_ref: &pull.base.ref_name,
                    title: &pull.title,
                },
            )
            .await;
            match prepared {
                Ok(outcome) => {
                    let _ = std::fs::remove_dir_all(&workspace);
                    if std::fs::rename(&staging, &workspace).is_err() {
                        return Err(format!("review workspace 替换失败: {session_id}"));
                    }
                    Some(outcome.context)
                }
                Err(error) => {
                    let _ = std::fs::remove_dir_all(&staging);
                    return Err(format!("PR #{issue_number} 工作区准备失败: {error}"));
                }
            }
        } else {
            None
        };
        let payload = TaskPayload {
            repo: gh.repo.clone(),
            issue: issue_number,
            title: issue.title.clone(),
            body: if task_text.is_empty() {
                issue.body.clone().unwrap_or_default()
            } else {
                task_text
            },
            message_tag: Some(format!("comment-{}", comment.id)),
            pr,
        };
        submit_inner(state, payload, false)
            .await
            .map_err(|(_, message)| message)?;
        dispatched += 1;
    }
    Ok(dispatched)
}

/// 常驻轮询循环:启动时预填已见评论,避免把历史 `/xh` 当新任务。
pub async fn run_poller(state: GhHostState, interval: Duration) {
    let Some(gh) = &state.gh else { return };
    match gh.client.list_repo_comments(&gh.repo).await {
        Ok(comments) => {
            let mut seen = state.seen_comments.lock().unwrap();
            for comment in comments {
                seen.insert(comment.id);
            }
        }
        Err(error) => eprintln!("[xh-gh] poller 启动预填失败: {error}"),
    }
    eprintln!(
        "[xh-gh] poller 运行中: 每 {:?} 扫描 {}/issues/comments,触发词 `/xh`(仅 owner)",
        interval, gh.repo
    );
    loop {
        tokio::time::sleep(interval).await;
        match poll_once(&state).await {
            Ok(0) => {}
            Ok(n) => eprintln!("[xh-gh] poller 触发 {n} 个任务"),
            Err(error) => eprintln!("[xh-gh] poller 轮询失败: {error}"),
        }
    }
}

/// 启动恢复:扫描 workspace_root 下带 task.json 的会话,重放提交。
/// durable inbox 按消息 id 去重:已完成的输入不会被再次消费,只有
/// 崩溃前已落账、未跑完的输入会真正续跑。结果评论为 marker upsert,
/// 重复写安全。
pub async fn recover_pending(state: &GhHostState) {
    let Ok(entries) = std::fs::read_dir(&state.workspace_root) else {
        return;
    };
    for entry in entries.flatten() {
        let task_file = entry.path().join("task.json");
        let Ok(raw) = std::fs::read_to_string(&task_file) else {
            continue;
        };
        let Ok(payload) = serde_json::from_str::<TaskPayload>(&raw) else {
            eprintln!(
                "[xh-gh] 恢复: {} task.json 解析失败,跳过",
                entry.path().display()
            );
            continue;
        };
        let label = payload
            .message_tag
            .clone()
            .unwrap_or_else(|| "v1".to_owned());
        if entry.path().join("done.marker").exists() {
            eprintln!(
                "[xh-gh] 恢复: {}({label})已完成,跳过",
                entry.path().display()
            );
            continue;
        }
        // 首次尝试原 tag(pending 输入会被同 id 认领);若原输入已被消费
        // (mid-turn 崩溃,id 标记已用会被抑制),换新 tag 重试并注明恢复。
        let mut retry = payload.clone();
        retry.message_tag = Some(format!(
            "{}-r{}",
            label,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default()
        ));
        retry.body = format!(
            "(系统恢复:上次执行被中断,请重新完成本任务)\n\n{}",
            payload.body
        );
        match submit_inner(state, retry, true).await {
            Ok(session_id) => {
                eprintln!("[xh-gh] 恢复: {session_id}({label})以重试 tag 重新驱动")
            }
            Err((_, message)) => {
                eprintln!(
                    "[xh-gh] 恢复: {}({label})跳过: {message}",
                    entry.path().display()
                )
            }
        }
    }
}
