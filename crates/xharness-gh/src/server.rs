//! Axum 任务入口:POST /tasks 提交 issue 任务,GET /results/{id} 轮询结果。
//! 每个 (repo, issue) 一个持久 durable session;turn 完成后由完成钩子把
//! 结果 upsert 到该会话的评论槽并打 xh:done。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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

#[derive(Deserialize)]
pub struct TaskPayload {
    pub repo: String,
    pub issue: u64,
    pub title: String,
    pub body: String,
}

pub fn session_id_for(repo: &str, issue: u64) -> String {
    format!("gh-{}-{issue}", repo.replace('/', "--"))
}

async fn submit_task(
    State(state): State<GhHostState>,
    Json(payload): Json<TaskPayload>,
) -> impl IntoResponse {
    if let Some(gh) = &state.gh {
        if payload.repo != gh.repo {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!(
                    "repo {} 不在本 gh-host 的服务范围({})内",
                    payload.repo, gh.repo
                )})),
            );
        }
    }
    let session_id = session_id_for(&payload.repo, payload.issue);
    let workspace = state.workspace_root.join(&session_id);
    std::fs::create_dir_all(&workspace).expect("创建 workspace 目录");
    let request = AgentTurnRequest {
        session_id: session_id.clone(),
        cwd: workspace.to_string_lossy().to_string(),
        route: state.route.clone(),
        permission: PermissionPreset::DangerFullAccess,
        prompt: None,
        messages: vec![Message {
            id: Some(format!("gh-{}/{}/v1", payload.repo, payload.issue)),
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
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error.to_string()})),
        );
    }
    let mut turn = match state.runtime.start_turn(request).await {
        Ok(turn) => turn,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error.to_string()})),
            )
        }
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
    (StatusCode::ACCEPTED, Json(json!({"sessionId": session_id})))
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
