//! Axum 任务入口:POST /tasks 提交 issue 任务,GET /results/{id} 轮询结果。
//! 每个 (repo, issue) 一个持久 durable session;turn 完成后由完成钩子把
//! 结果 upsert 到该会话的评论槽并打 xh:done。

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
use serde::Deserialize;
use serde_json::json;
use xharness_core::LoopStatus;
use xharness_host::{
    AgentRuntime, AgentTurnRequest, DurableLoopAgentRuntime, ModelRoute, PermissionPreset,
};
use xharness_session::{Message, MessageRole};

use crate::client::session_marker;
use crate::factory::GhToolConfig;

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

#[derive(Deserialize, Clone)]
pub struct TaskPayload {
    pub repo: String,
    pub issue: u64,
    pub title: String,
    pub body: String,
    /// 触发源标识,进入 durable 消息 id 以便幂等;HTTP 直发缺省 "v1"。
    #[serde(default)]
    pub message_tag: Option<String>,
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
    match submit_inner(&state, payload).await {
        Ok(session_id) => (StatusCode::ACCEPTED, Json(json!({"sessionId": session_id}))),
        Err((code, message)) => (code, Json(json!({"error": message}))),
    }
}

/// HTTP 与轮询触发器共用的任务提交入口。
pub(crate) async fn submit_inner(
    state: &GhHostState,
    payload: TaskPayload,
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
            id: Some(format!("gh-{}/{}/{}", payload.repo, payload.issue, message_tag)),
            role: MessageRole::User,
            content: format!(
                "仓库 {} issue #{}: {}\n\n{}\n\n你的工作目录(已存在,可用 bash/read/write 等工具直接操作,无审批):{}",
                payload.repo, payload.issue, payload.title, payload.body,
                workspace.to_string_lossy()
            ),
            ..Default::default()
        }],
        input_metadata: Some(json!({
            "source": "gh",
            "repo": payload.repo,
            "issue": payload.issue,
        })),
    };
    if let Err(error) = state.runtime.admit_turn(request.clone()).await {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, error.to_string()));
    }
    let mut turn = match state.runtime.start_turn(request).await {
        Ok(turn) => turn,
        Err(error) => return Err((StatusCode::INTERNAL_SERVER_ERROR, error.to_string())),
    };
    let results = Arc::clone(&state.results);
    let tracked_session = session_id.clone();
    let gh = state.gh.clone();
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
        if let Some(gh) = gh {
            let body = format!("## xh-gh 任务结果\n\n{final_text}\n\n---\n状态: `{status}`");
            if let Err(error) = gh
                .client
                .upsert_session_comment(&gh.repo, issue, &session_marker(&tracked_session), &body)
                .await
            {
                eprintln!("[xh-gh] 结果评论写入失败: {error}");
            }
            if let Err(error) = gh.client.add_label(&gh.repo, issue, "xh:done").await {
                eprintln!("[xh-gh] label 写入失败: {error}");
            }
        }
    });
    Ok(session_id)
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
        let issue = match gh.client.get_issue(&gh.repo, issue_number).await {
            Ok(issue) => issue,
            Err(error) => return Err(error),
        };
        let task_text = body.replacen("/xh", "", 1).trim().to_owned();
        let payload = TaskPayload {
            repo: gh.repo.clone(),
            issue: issue_number,
            title: issue.title,
            body: if task_text.is_empty() {
                issue.body.unwrap_or_default()
            } else {
                task_text
            },
            message_tag: Some(format!("comment-{}", comment.id)),
        };
        submit_inner(state, payload)
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
