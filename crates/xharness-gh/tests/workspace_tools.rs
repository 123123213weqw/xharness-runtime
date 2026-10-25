//! workspace 工具链(DangerFullAccess、无审批)离线验证:
//! bash 真实执行 → 输出流入 gh_comment 参数 → 终止。任何一环等待审批或
//! 权限拒绝都会让测试超时/失败。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use xharness_agent::FileLeaseManager;
use xharness_context::ToolResultPruningContextPolicy;
use xharness_gh::factory::GhToolFactory;
use xharness_gh::provider_mock::{WorkspaceScriptedProvider, MOCK_FINAL_TEXT};
use xharness_gh::server::{router, GhHostState};
use xharness_host::{DurableLoopAgentRuntime, ModelRoute};
use xharness_session_jsonl::JsonlSessionStore;

#[tokio::test]
async fn bash_output_flows_into_gh_comment_without_approval() {
    let dir = std::env::temp_dir().join(format!(
        "xh-gh-workspace-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
    ));
    let store = Arc::new(
        JsonlSessionStore::new(dir.join("sessions"))
            .expect("session store")
            .for_runtime(),
    );
    let leases = Arc::new(FileLeaseManager::new(dir.join("leases")).expect("lease manager"));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let factory = Arc::new(GhToolFactory {
        gh: None,
        calls: Arc::clone(&calls),
        ..Default::default()
    });
    let runtime = Arc::new(DurableLoopAgentRuntime::new(
        "mock",
        "scripted",
        Some(Arc::new(WorkspaceScriptedProvider)),
        factory,
        Arc::new(ToolResultPruningContextPolicy::default()),
        store,
        leases,
        64,
    ));
    let state = GhHostState {
        runtime,
        route: ModelRoute::new("mock", "scripted"),
        results: Default::default(),
        gh: None,
        workspace_root: dir.join("workspaces"),
    };
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}/tasks"))
        .json(&serde_json::json!({
            "repo": "acme/x",
            "issue": 9,
            "title": "workspace tools",
            "body": "run bash and report"
        }))
        .send()
        .await
        .expect("post /tasks");
    let status = response.status();
    let accepted: Value = response.json().await.expect("body");
    assert_eq!(status, 202, "body: {accepted}");

    let mut final_text = String::new();
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let result: Value = client
            .get(format!("http://{addr}/results/gh-acme--x-9"))
            .send()
            .await
            .expect("get /results")
            .json()
            .await
            .expect("result body");
        if result["status"] == "done" {
            final_text = result["finalText"].as_str().expect("text").to_owned();
            break;
        }
        if result["status"] == "stopped" {
            panic!("turn 提前结束: {result}");
        }
    }
    assert_eq!(final_text, MOCK_FINAL_TEXT);

    let recorded = calls.lock().unwrap();
    let gh_call = recorded
        .iter()
        .find(|call| call["body"].as_str().is_some())
        .expect("gh_comment 被调用");
    assert!(
        gh_call["body"]
            .as_str()
            .unwrap()
            .contains("hello-from-workspace"),
        "bash 的输出应流入 gh_comment 参数: {gh_call:?}"
    );
}
