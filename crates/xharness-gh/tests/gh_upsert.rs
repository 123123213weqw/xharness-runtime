//! 假 GitHub API 上的 upsert 语义验证:首次 POST,重复写 PATCH 同一条。
//! 路由结构与真实 API 一致(owner/repo 两段)。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, State},
    routing::{get, patch},
    Json, Router,
};
use serde_json::Value;
use xharness_gh::client::{session_marker, GhClient, UpsertOutcome};

#[derive(Default)]
struct MockGh {
    comments: Mutex<HashMap<u64, String>>,
    next_id: AtomicU64,
    patches: AtomicU64,
}

async fn list_comments(
    State(state): State<Arc<MockGh>>,
    Path((_owner, _repo, _issue)): Path<(String, String, u64)>,
) -> Json<Value> {
    let comments = state.comments.lock().unwrap();
    let items: Vec<Value> = comments
        .iter()
        .map(|(id, body)| serde_json::json!({"id": id, "body": body}))
        .collect();
    Json(Value::Array(items))
}

async fn create_comment(
    State(state): State<Arc<MockGh>>,
    Path((_owner, _repo, _issue)): Path<(String, String, u64)>,
    Json(payload): Json<Value>,
) -> Json<Value> {
    let id = state.next_id.fetch_add(1, Ordering::SeqCst) + 1;
    let body = payload["body"].as_str().expect("body").to_owned();
    state.comments.lock().unwrap().insert(id, body.clone());
    Json(serde_json::json!({"id": id, "body": body}))
}

async fn patch_comment(
    State(state): State<Arc<MockGh>>,
    Path((_owner, _repo, id)): Path<(String, String, u64)>,
    Json(payload): Json<Value>,
) -> Json<Value> {
    state.patches.fetch_add(1, Ordering::SeqCst);
    let body = payload["body"].as_str().expect("body").to_owned();
    state.comments.lock().unwrap().insert(id, body.clone());
    Json(serde_json::json!({"id": id, "body": body}))
}

#[tokio::test]
async fn upsert_creates_then_updates_same_slot() {
    let mock = Arc::new(MockGh::default());
    let app = Router::new()
        .route(
            "/repos/{owner}/{repo}/issues/{issue}/comments",
            get(list_comments).post(create_comment),
        )
        .route(
            "/repos/{owner}/{repo}/issues/comments/{id}",
            patch(patch_comment),
        )
        .with_state(Arc::clone(&mock));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

    let client = GhClient::new("test-token").with_api_root(format!("http://{addr}"));
    let marker = session_marker("gh-acme--x-7");

    let first = client
        .upsert_session_comment("acme/x", 7, &marker, "第一版结果")
        .await
        .expect("first upsert");
    assert_eq!(first, UpsertOutcome::Created { id: 1 });

    let second = client
        .upsert_session_comment("acme/x", 7, &marker, "第二版结果")
        .await
        .expect("second upsert");
    assert_eq!(second, UpsertOutcome::Updated { id: 1 });

    let comments = mock.comments.lock().unwrap();
    assert_eq!(comments.len(), 1, "始终只有一条会话评论");
    let stored = &comments[&1];
    assert!(stored.starts_with(&marker), "评论以 marker 开头");
    assert!(stored.contains("第二版结果"), "内容已更新为最新");
    assert_eq!(mock.patches.load(Ordering::SeqCst), 1);
}
