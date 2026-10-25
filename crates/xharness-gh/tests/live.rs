//! 真实端点集成测试:仅当 XH_GH_LIVE=1 时执行(本地/V100 上运行,CI 跳过)。
//!
//!   XH_GH_API_KEY=sk-... XH_GH_LIVE=1 cargo test -p xharness-gh --test live
//!
//! 流程:真实端点部署 → POST /tasks(一句话任务)→ 轮询至 done →
//! 断言 final_text 非空。不依赖 gh_comment 工具被调用(模型自由选择)。

use std::sync::Arc;
use std::time::Duration;

use xharness_agent::FileLeaseManager;
use xharness_context::ToolResultPruningContextPolicy;
use xharness_gh::deployment::{build_registry, SingleModelDeployment};
use xharness_gh::factory::GhToolFactory;
use xharness_gh::server::{router, GhHostState};
use xharness_host::DurableLoopAgentRuntime;
use xharness_provider_openai::OpenAiProtocol;
use xharness_session_jsonl::JsonlSessionStore;

#[tokio::test]
async fn live_endpoint_completes_a_turn() {
    if std::env::var("XH_GH_LIVE").is_err() {
        eprintln!("XH_GH_LIVE 未设置,跳过 live 测试");
        return;
    }
    let api_key = std::env::var("XH_GH_API_KEY").expect("XH_GH_API_KEY");
    let base_url =
        std::env::var("XH_GH_BASE_URL").unwrap_or_else(|_| "https://api.deepseek.com".to_owned());
    let model = std::env::var("XH_GH_MODEL").unwrap_or_else(|_| "deepseek-chat".to_owned());
    let provider = std::env::var("XH_GH_PROVIDER").unwrap_or_else(|_| "deepseek".to_owned());
    let dir = std::env::temp_dir().join(format!("xh-gh-live-{}", std::process::id()));

    let (route, registry) = build_registry(SingleModelDeployment {
        provider: provider.clone(),
        model: model.clone(),
        base_url,
        api_key,
        protocol: OpenAiProtocol::ChatCompletions,
        context_window_tokens: 131_072,
        max_output_tokens: 2_048,
        minimum_output_tokens: None,
        token_safety_margin: 2_048,
    })
    .await
    .expect("deployment");
    let store = Arc::new(
        JsonlSessionStore::new(dir.join("sessions"))
            .expect("store")
            .for_runtime(),
    );
    let leases = Arc::new(FileLeaseManager::new(dir.join("leases")).expect("leases"));
    let runtime = Arc::new(
        DurableLoopAgentRuntime::from_registry(
            route.clone(),
            registry,
            Arc::new(GhToolFactory::default()),
            Arc::new(ToolResultPruningContextPolicy::default()),
            store,
            leases,
            64,
        )
        .expect("runtime"),
    );
    let app = router(GhHostState {
        runtime,
        route,
        results: Default::default(),
        gh: None,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}/tasks"))
        .json(&serde_json::json!({
            "repo": "live/probe",
            "issue": 1,
            "title": "连通性测试",
            "body": "请只回复一句话:报告你的模型名称并说「链路正常」。"
        }))
        .send()
        .await
        .expect("post");
    let status = response.status();
    let accepted: serde_json::Value = response.json().await.expect("body");
    assert_eq!(status, 202, "body: {accepted}");
    let session_id = accepted["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_owned();

    let mut final_text = String::new();
    for _ in 0..300 {
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        let result: serde_json::Value = client
            .get(format!("http://{addr}/results/{session_id}"))
            .send()
            .await
            .expect("get")
            .json()
            .await
            .expect("body");
        if result["status"] == "done" {
            final_text = result["finalText"].as_str().unwrap_or_default().to_owned();
            break;
        }
        if result["status"] == "stopped" {
            panic!("turn 非正常结束: {result}");
        }
    }
    assert!(!final_text.is_empty(), "5 分钟内未完成");
    println!("live final_text: {final_text}");
}
