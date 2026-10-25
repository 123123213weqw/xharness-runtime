//! 离线全链路实验:POST /tasks → admission → durable turn → mock provider
//! 两步(工具调用→终止文本)→ gh_comment 工具执行被记录 → 结果可查询。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use xharness_agent::FileLeaseManager;
use xharness_context::ToolResultPruningContextPolicy;
use xharness_gh::factory::GhToolFactory;
use xharness_gh::provider_mock::{ScriptedProvider, MOCK_FINAL_TEXT};
use xharness_gh::server::{router, GhHostState};
use xharness_host::{DurableLoopAgentRuntime, ModelRoute};
use xharness_session_jsonl::JsonlSessionStore;

#[tokio::test]
async fn task_post_runs_turn_and_records_tool_call() {
    let dir = std::env::temp_dir().join(format!(
        "xh-gh-experiment-{}-{:?}",
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
        Some(Arc::new(ScriptedProvider)),
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
        task_secret: None,
        seen_comments: Default::default(),
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
            "issue": 7,
            "title": "fix the thing",
            "body": "please do it"
        }))
        .send()
        .await
        .expect("post /tasks");
    let status = response.status();
    let accepted: Value = response.json().await.expect("response body");
    assert_eq!(status, 202, "body: {accepted}");
    assert_eq!(accepted["sessionId"], "gh-acme--x-7");

    let mut final_text = String::new();
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let result: Value = client
            .get(format!("http://{addr}/results/gh-acme--x-7"))
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
    }
    assert_eq!(final_text, MOCK_FINAL_TEXT);

    let recorded = calls.lock().unwrap();
    assert_eq!(recorded.len(), 1, "gh_comment 恰好被调用一次: {recorded:?}");
    assert_eq!(recorded[0]["body"], "fixed by mock agent");
}
