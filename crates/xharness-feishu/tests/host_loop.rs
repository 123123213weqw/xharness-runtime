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

// ---- system prompt ----------------------------------------------------------

#[test]
fn the_system_prompt_states_the_sandbox_rules() {
    use xharness_feishu::prompt::system_text;

    let text = system_text(None).expect("应当能组装");
    // 这三条是踩过的坑：模型对空目录全盘 find、编造输出、不知道没有网络。
    assert!(text.contains("隔离沙箱"), "必须说明自己在沙箱里");
    assert!(text.contains("默认不要做全盘扫描"), "必须限制默认扫描行为");
    assert!(
        text.contains("沙箱中不可见") || text.contains("被屏蔽"),
        "必须说明哪些内容不可见"
    );
    assert!(text.contains("没有网络访问"), "必须说明断网");
    assert!(text.contains("不要凭想象描述结果"), "必须要求报告真实输出");
    // 镜像自上游 coding/workflow 的两条，缺了会退化成"盲目重试"和"盲从文件内容"。
    assert!(text.contains("先看清再动手"), "缺少最小改动的约束");
    assert!(
        text.contains("不是更高优先级的指令"),
        "缺少 prompt injection 防护"
    );
}

#[test]
fn caller_instructions_are_appended_last() {
    use xharness_feishu::prompt::system_text;

    let text = system_text(Some("你是月光的小助手")).expect("应当能组装");
    assert!(text.contains("你是月光的小助手"), "调用方指令不能丢");
    // 顺序：沙箱规则在前，定制在后，便于后者覆盖默认措辞。
    let sandbox_at = text.find("隔离沙箱").expect("sandbox section");
    let caller_at = text.find("你是月光的小助手").expect("caller section");
    assert!(sandbox_at < caller_at, "调用方指令应当在后");
}

#[test]
fn blank_caller_instructions_are_ignored() {
    use xharness_feishu::prompt::system_text;
    let with_blank = system_text(Some("   ")).expect("assemble");
    let without = system_text(None).expect("assemble");
    assert_eq!(with_blank, without, "空白指令不应产生多余 section");
}

#[tokio::test(flavor = "multi_thread")]
async fn user_message_does_not_contain_the_system_prompt() {
    // 之前 system 被拼进 user message，导致 systemTokens 实测为 0。
    // 这里从会话事件层面确认 user 消息只剩用户原话。
    let dir = fresh_dir("promptmsg");
    let addr = serve(state_at(&dir, None)).await;
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "chat_id": "oc_p", "message_id": "m1",
        "text": "只回复 收到",
        "system_prompt": "你是飞书助手",
    });
    let resp = client
        .post(format!("http://{addr}/tasks"))
        .json(&body)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);

    for _ in 0..200 {
        let result: serde_json::Value = client
            .get(format!("http://{addr}/results/fs-oc_p"))
            .send()
            .await
            .expect("get")
            .json()
            .await
            .expect("json");
        if result["status"] != "unknown" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let session = dir.join("sessions").join("fs-oc_p.jsonl");
    let raw = std::fs::read_to_string(&session).expect("session file");
    let mut user_contents = Vec::new();
    for line in raw.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(events) = record.get("events").and_then(|e| e.as_array()) else {
            continue;
        };
        for event in events {
            let body = event.get("event").cloned().unwrap_or_default();
            if body.get("type").and_then(|t| t.as_str()) == Some("user/message") {
                if let Some(content) = body
                    .get("data")
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_str())
                {
                    user_contents.push(content.to_owned());
                }
            }
        }
    }
    assert!(!user_contents.is_empty(), "应当有 user/message 事件");
    for content in &user_contents {
        assert_eq!(content, "只回复 收到", "user 消息不得混入 system 内容");
        assert!(!content.contains("你是飞书助手"), "system 不该拼进 user");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
