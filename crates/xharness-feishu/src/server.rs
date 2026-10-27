//! HTTP 接口：提交任务 + 查询结果。
//!
//! 与 gh-host 的 `/tasks` 相比，payload 不绑定 repo/issue —— 这里是通用形态，
//! 调用方（飞书机器人）给一段文本即可。异步任务模型是刻意的：飞书事件有 3 秒
//! 预算，所以 bot 只负责提交，结果靠轮询。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use xharness_core::LoopStatus;
use xharness_host::{
    AgentRuntime, AgentTurnRequest, DurableLoopAgentRuntime, ModelRoute, PermissionPreset,
};
use xharness_session::{Message, MessageRole};

/// 一条来自飞书的任务。
///
/// `message_id` 是幂等键：平台会重推同一事件，重复提交不应产生第二次 turn。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPayload {
    pub chat_id: String,
    pub message_id: String,
    pub text: String,
    /// 会话 workspace 名；缺省用 chat_id 派生。同一值复用同一个 durable session。
    #[serde(default)]
    pub workspace: Option<String>,
    /// 覆盖默认 system prompt。
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub message_tag: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TaskOutcome {
    pub status: String,
    pub final_text: String,
}

pub struct FeishuHostState {
    pub runtime: Arc<DurableLoopAgentRuntime>,
    pub route: ModelRoute,
    pub results: Arc<Mutex<HashMap<String, TaskOutcome>>>,
    pub workspace_root: PathBuf,
    pub task_secret: Option<String>,
    pub permission: PermissionPreset,
}

/// 会话 id：`fs-{chat}`。把 `/` 等字符折叠，保证能安全用作目录名。
pub fn session_id_for(chat_id: &str, workspace: Option<&str>) -> String {
    let raw = workspace.unwrap_or(chat_id);
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("fs-{cleaned}")
}

fn build_prompt(payload: &TaskPayload) -> String {
    match &payload.system_prompt {
        Some(system) if !system.trim().is_empty() => {
            format!("{system}\n\n---\n\n{}", payload.text)
        }
        _ => payload.text.clone(),
    }
}

pub fn router(state: Arc<FeishuHostState>) -> Router {
    Router::new()
        .route("/tasks", post(submit_task))
        .route("/results/{session_id}", get(get_result))
        .route("/health", get(|| async { Json(json!({"ok": true})) }))
        .with_state(state)
}

async fn submit_task(
    State(state): State<Arc<FeishuHostState>>,
    headers: HeaderMap,
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
    if payload.text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "text 为空"})));
    }
    match submit_inner(&state, payload).await {
        Ok(session_id) => (StatusCode::ACCEPTED, Json(json!({"sessionId": session_id}))),
        Err((code, message)) => (code, Json(json!({"error": message}))),
    }
}

pub async fn submit_inner(
    state: &Arc<FeishuHostState>,
    payload: TaskPayload,
) -> Result<String, (StatusCode, String)> {
    let session_id = session_id_for(&payload.chat_id, payload.workspace.as_deref());
    let workspace = state.workspace_root.join(&session_id);
    std::fs::create_dir_all(&workspace).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("创建 workspace 失败: {error}"),
        )
    })?;

    // 落一份任务快照，便于崩溃后人工核对（恢复仍以 done.marker 为准）。
    let task_file = workspace.join("task.json");
    if let Ok(serialized) = serde_json::to_string_pretty(&payload) {
        let _ = std::fs::write(&task_file, serialized);
    }

    let tag = payload
        .message_tag
        .clone()
        .unwrap_or_else(|| "v1".to_owned());
    let request = AgentTurnRequest {
        session_id: session_id.clone(),
        cwd: workspace.to_string_lossy().to_string(),
        route: state.route.clone(),
        permission: state.permission,
        prompt: None,
        messages: vec![Message {
            // 幂等：同一 message_id 重推时落成同一条 durable 消息。
            id: Some(format!(
                "fs-{}/{}/{}",
                payload.chat_id, payload.message_id, tag
            )),
            role: MessageRole::User,
            content: build_prompt(&payload),
            ..Default::default()
        }],
        input_metadata: Some(json!({
            "source": "feishu",
            "chat_id": payload.chat_id,
            "message_id": payload.message_id,
        })),
    };

    state
        .runtime
        .admit_turn(request.clone())
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;

    let mut turn = state
        .runtime
        .start_turn(request)
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;

    // 同一个 chat 的 session 会跑很多轮，results 以 session_id 为键，
    // 所以必须在启动新 turn 后清掉上一轮的结果——否则轮询端会立刻读到旧答案，
    // 把上一轮的回答误当成这一轮的（实测过：问"口令是什么"回了上一轮的 "OK"）。
    // 顺序很重要：先 start_turn 再清，避免清了之后 admit 失败导致结果丢失。
    state.results.lock().unwrap().remove(&session_id);

    let results = Arc::clone(&state.results);
    let tracked = session_id.clone();
    tokio::spawn(async move {
        while turn.next_event().await.is_some() {}
        let result = turn.result().await;
        let status = match result.status {
            LoopStatus::Completed => "done",
            _ => "stopped",
        };
        let final_text = if result.final_text.is_empty() {
            "(任务结束,模型没有产生文本输出)".to_owned()
        } else {
            result.final_text.clone()
        };
        results.lock().unwrap().insert(
            tracked,
            TaskOutcome {
                status: status.to_owned(),
                final_text,
            },
        );
    });

    Ok(session_id)
}

async fn get_result(
    State(state): State<Arc<FeishuHostState>>,
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
