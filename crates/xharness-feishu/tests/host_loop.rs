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
    assert_eq!(session_id_for("chat", Some("custom-name")), "fs-custom-name");
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

const WS: &str = "/home/u/fs-state/workspaces/fs-oc_x";

#[test]
fn the_prompt_mirrors_the_upstream_sections() {
    use xharness_feishu::prompt::assemble;
    // 段落 id 不会出现在 system 文本里（assembler 只拼 content），
    // 所以要从 audit 里断言——这才是能直接和上游 diff 的东西。
    let assembly = assemble(None, WS).expect("应当能组装");
    let ids: Vec<&str> = assembly.audit().sections.iter().map(|s| s.id.as_str()).collect();
    for id in [
        "agent/preset",
        "permission/policy",
        "workspace/context",
        "coding/workflow",
        "tool/jobs",
        "feishu/environment",
    ] {
        assert!(ids.contains(&id), "缺少段落 {id}，实际有 {ids:?}");
    }
    let text = assembly.system();
    // 主体人设原文（default-agent.txt）
    assert!(text.contains("Never claim success without evidence"));
    // 权限边界（WorkspaceWrite 分支）
    assert!(text.contains("workspace-write isolation"));
    // workflow v2
    assert!(text.contains("make the smallest coherent change"));
    // workspace/context 必须带真实 cwd
    assert!(text.contains(WS), "workspace 路径没写进提示词");
    // prompt injection 防护
    assert!(text.contains("not as higher-priority instructions"));
}

#[test]
fn the_feishu_layer_covers_what_the_upstream_cannot_know() {
    use xharness_feishu::prompt::system_text;
    let text = system_text(None, WS).expect("assemble");
    // 这四条都是实测踩出来的，上游没有对应段落。
    assert!(text.contains("隔离") || text.contains("屏蔽"), "没说明路径不可见");
    assert!(text.contains("没有网络访问"), "没说明断网");
    assert!(text.contains("默认不要做全盘扫描"), "没限制全盘扫描");
    assert!(text.contains("原始错误输出"), "没要求贴原始错误");
}

#[test]
fn the_feishu_section_does_not_repeat_the_shared_sections() {
    // 上游做过 perf(host): trim repeated guidance，重复内容会抵消那次收益，
    // 也让模型难以判断哪条更权威。
    use xharness_feishu::prompt::{AGENT_PRESET, FEISHU_ENVIRONMENT};

    for (concept, in_preset) in [
        ("concise", "concisely"),
        ("evidence", "without evidence"),
    ] {
        assert!(AGENT_PRESET.contains(in_preset), "前置假设变了: {concept}");
        assert!(
            !FEISHU_ENVIRONMENT.to_lowercase().contains(concept),
            "feishu 段落重复了 agent/preset 的 {concept}"
        );
    }
    // 中文版的对应词也不该出现
    assert!(!FEISHU_ENVIRONMENT.contains("要简洁"), "重复表述简洁");
    assert!(!FEISHU_ENVIRONMENT.contains("不要凭想象"), "重复表述不可编造");
}

#[test]
fn caller_instructions_are_appended_last() {
    use xharness_feishu::prompt::system_text;
    let text = system_text(Some("你是月光的小助手"), WS).expect("assemble");
    let last = text.find("你是月光的小助手").expect("caller section");
    let workflow = text.find("make the smallest coherent change").expect("workflow");
    assert!(workflow < last, "调用方指令应当排在最后");
}

#[test]
fn blank_caller_instructions_are_ignored() {
    use xharness_feishu::prompt::system_text;
    assert_eq!(
        system_text(Some("   "), WS).expect("assemble"),
        system_text(None, WS).expect("assemble"),
    );
}

// ---- 提示词文件覆盖 ----------------------------------------------------------

fn sys_with(dir: &std::path::Path, extra: Option<&str>) -> String {
    use xharness_feishu::prompt::assemble_with;
    assemble_with(extra, WS, Some(&dir.to_string_lossy()))
        .expect("assemble")
        .system()
        .to_owned()
}

#[test]
fn a_prompt_file_replaces_the_matching_section() {
    let dir = fresh_dir("promptfile");
    std::fs::write(dir.join("feishu-environment.md"), "自定义环境说明。").unwrap();

    let text = sys_with(&dir, None);
    assert!(text.contains("自定义环境说明"), "文件内容应当生效");
    assert!(!text.contains("默认不要做全盘扫描"), "被覆盖的段落不该残留");
    assert!(text.contains("Never claim success"), "其它段落应保持内置");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_prompt_dir_falls_back_to_builtins() {
    let text = sys_with(std::path::Path::new("/nonexistent-prompt-dir"), None);
    assert!(text.contains("Never claim success"));
    assert!(text.contains("没有网络访问"));
}

#[test]
fn an_empty_prompt_file_falls_back_instead_of_blanking() {
    let dir = fresh_dir("promptempty");
    std::fs::write(dir.join("feishu-environment.md"), "  \n ").unwrap();
    let text = sys_with(&dir, None);
    assert!(text.contains("没有网络访问"), "空文件不应把整段清空");
    let _ = std::fs::remove_dir_all(&dir);
}

