//! Axum 任务入口:POST /tasks 提交 issue 任务,GET /results/:id 轮询结果。
//! 每个 (repo, issue) 一个持久 durable session。

use std::collections::HashMap;
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

#[derive(Clone)]
pub struct GhHostState {
    pub runtime: Arc<DurableLoopAgentRuntime>,
    pub route: ModelRoute,
    pub results: Arc<Mutex<HashMap<String, TaskOutcome>>>,
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

fn session_id_for(repo: &str, issue: u64) -> String {
    format!("gh-{}-{issue}", repo.replace('/', "-"))
}

async fn submit_task(
    State(state): State<GhHostState>,
    Json(payload): Json<TaskPayload>,
) -> impl IntoResponse {
    let session_id = session_id_for(&payload.repo, payload.issue);
    let request = AgentTurnRequest {
        session_id: session_id.clone(),
        cwd: std::env::temp_dir().to_string_lossy().to_string(),
        route: state.route.clone(),
        permission: PermissionPreset::WorkspaceWrite,
        prompt: None,
        messages: vec![Message {
            id: Some(format!("gh-{}/{}/v1", payload.repo, payload.issue)),
            role: MessageRole::User,
            content: format!(
                "仓库 {} issue #{}: {}\n\n{}",
                payload.repo, payload.issue, payload.title, payload.body
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
    tokio::spawn(async move {
        while turn.next_event().await.is_some() {}
        let result = turn.result().await;
        let status = match result.status {
            LoopStatus::Completed => "done",
            _ => "stopped",
        };
        results.lock().unwrap().insert(
            tracked_session,
            TaskOutcome {
                status: status.to_owned(),
                final_text: result.final_text,
            },
        );
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
