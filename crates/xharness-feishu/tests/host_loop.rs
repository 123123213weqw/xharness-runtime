//! 端到端：提交任务 → 轮询结果 → 拿到文本。用 mock provider，全程离线。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use xharness_agent::FileLeaseManager;
use xharness_context::ToolResultPruningContextPolicy;
use xharness_feishu::{
    router, session_id_for, FeishuHostState, FeishuScriptedProvider, FeishuToolFactory, TaskPayload,
};
use xharness_host::{DurableLoopAgentRuntime, ModelRoute, PermissionPreset};
use xharness_session_jsonl::JsonlSessionStore;

static SEQ: AtomicUsize = AtomicUsize::new(0);

/// 每个测试独立的 state 目录，避免 lease/session 互相干扰。
fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("xh-fs-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn state_at(dir: &std::path::Path, secret: Option<&str>) -> Arc<FeishuHostState> {
    let store = Arc::new(
        JsonlSessionStore::new(dir.join("sessions"))
            .expect("session store")
            .for_runtime(),
    );
    let leases = Arc::new(FileLeaseManager::new(dir.join("leases")).expect("leases"));
    let runtime = DurableLoopAgentRuntime::new(
        "mock",
        "scripted",
        Some(Arc::new(FeishuScriptedProvider)),
        Arc::new(FeishuToolFactory::default()),
        Arc::new(ToolResultPruningContextPolicy::default()),
        store,
        leases,
        64,
    );
    Arc::new(FeishuHostState {
        runtime: Arc::new(runtime),
        route: ModelRoute::new("mock", "scripted"),
        results: Arc::new(Mutex::new(Default::default())),
        workspace_root: dir.join("workspaces"),
        task_secret: secret.map(str::to_owned),
        permission: PermissionPreset::DangerFullAccess,
    })
}

async fn serve(state: Arc<FeishuHostState>) -> std::net::SocketAddr {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

fn payload(text: &str) -> TaskPayload {
    TaskPayload {
        chat_id: "oc_test".into(),
        message_id: "om_1".into(),
        text: text.into(),
        workspace: None,
        system_prompt: None,
        message_tag: None,
    }
}

#[test]
fn session_id_is_filesystem_safe() {
    assert_eq!(session_id_for("oc_abc123", None), "fs-oc_abc123");
    assert_eq!(session_id_for("a/b:c", None), "fs-a_b_c");
    assert_eq!(
        session_id_for("chat", Some("custom-name")),
        "fs-custom-name"
    );
    // 路径分隔符必须被折叠，否则 workspace 目录可能逃逸出 state_root
    assert!(!session_id_for("../evil", None).contains('/'));
    assert!(!session_id_for("..\\evil", None).contains('\\'));
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_then_poll_returns_final_text() {
    let dir = fresh_dir("loop");
    let addr = serve(state_at(&dir, None)).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://{addr}/tasks"))
        .json(&payload("hello"))
        .send()
        .await
        .expect("post /tasks");
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    let body: serde_json::Value = response.json().await.unwrap();
    let session_id = body["sessionId"].as_str().expect("sessionId").to_owned();
    assert_eq!(session_id, "fs-oc_test");

    let mut status = String::from("unknown");
    let mut final_text = String::new();
    for _ in 0..200 {
        let result: serde_json::Value = client
            .get(format!("http://{addr}/results/{session_id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        status = result["status"].as_str().unwrap_or("unknown").to_owned();
        if status != "unknown" {
            final_text = result["finalText"].as_str().unwrap_or("").to_owned();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_ne!(status, "unknown", "任务在超时前没有产生结果");
    assert!(!final_text.is_empty(), "结果文本为空");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_text_is_rejected() {
    let dir = fresh_dir("empty");
    let addr = serve(state_at(&dir, None)).await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/tasks"))
        .json(&payload("   "))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn task_secret_is_enforced() {
    let dir = fresh_dir("secret");
    let addr = serve(state_at(&dir, Some("s3cret"))).await;
    let client = reqwest::Client::new();

    let unauth = client
        .post(format!("http://{addr}/tasks"))
        .json(&payload("hi"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

    let authed = client
        .post(format!("http://{addr}/tasks"))
        .bearer_auth("s3cret")
        .json(&payload("hi"))
        .send()
        .await
        .unwrap();
    assert_eq!(authed.status(), reqwest::StatusCode::ACCEPTED);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn health_endpoint_answers() {
    let dir = fresh_dir("health");
    let addr = serve(state_at(&dir, None)).await;
    let body: serde_json::Value = reqwest::get(format!("http://{addr}/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["ok"], true);
    let _ = std::fs::remove_dir_all(&dir);
}
