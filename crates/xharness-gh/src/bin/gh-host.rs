//! gh-host:常驻 headless 任务入口。环境变量配置真实模型端点,
//! POST /tasks 提交 issue 任务,GET /results/{id} 查询。
//!
//! 必需:XH_GH_API_KEY(OpenAI 兼容端点的密钥,如 DEEPSEEK_API_KEY 的值)
//! 可选:XH_GH_BASE_URL(默认 https://api.deepseek.com)、
//!       XH_GH_MODEL(默认 deepseek-chat)、XH_GH_PROVIDER(默认 deepseek)、
//!       XH_GH_CONTEXT_WINDOW(默认 131072)、XH_GH_STATE_DIR(默认 ./gh-state)、
//!       XH_GH_BIND(默认 127.0.0.1:3081)

use std::sync::{Arc, Mutex};

use xharness_agent::FileLeaseManager;
use xharness_context::ToolResultPruningContextPolicy;
use xharness_gh::deployment::{build_registry, SingleModelDeployment};
use xharness_gh::factory::GhToolFactory;
use xharness_gh::server::{router, GhHostState};
use xharness_host::DurableLoopAgentRuntime;
use xharness_provider_openai::OpenAiProtocol;
use xharness_session_jsonl::JsonlSessionStore;

#[tokio::main]
async fn main() {
    let api_key = std::env::var("XH_GH_API_KEY").expect("XH_GH_API_KEY 未设置");
    let base_url =
        std::env::var("XH_GH_BASE_URL").unwrap_or_else(|_| "https://api.deepseek.com".to_owned());
    let model = std::env::var("XH_GH_MODEL").unwrap_or_else(|_| "deepseek-chat".to_owned());
    let provider = std::env::var("XH_GH_PROVIDER").unwrap_or_else(|_| "deepseek".to_owned());
    let context_window: u64 = std::env::var("XH_GH_CONTEXT_WINDOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(131_072);
    let state_dir = std::env::var("XH_GH_STATE_DIR").unwrap_or_else(|_| "./gh-state".to_owned());
    let bind = std::env::var("XH_GH_BIND").unwrap_or_else(|_| "127.0.0.1:3081".to_owned());

    let deployment = SingleModelDeployment {
        provider: provider.clone(),
        model: model.clone(),
        base_url: base_url.clone(),
        api_key,
        protocol: OpenAiProtocol::ChatCompletions,
        context_window_tokens: context_window,
        max_output_tokens: 8_192,
        minimum_output_tokens: None,
        token_safety_margin: 2_048,
    };
    let (route, registry) = build_registry(deployment)
        .await
        .unwrap_or_else(|error| panic!("模型部署初始化失败: {error}"));

    let store = Arc::new(
        JsonlSessionStore::new(std::path::Path::new(&state_dir).join("sessions"))
            .expect("session store")
            .for_runtime(),
    );
    let leases = Arc::new(
        FileLeaseManager::new(std::path::Path::new(&state_dir).join("leases"))
            .expect("lease manager"),
    );
    let runtime = Arc::new(
        DurableLoopAgentRuntime::from_registry(
            route,
            registry,
            Arc::new(GhToolFactory::default()),
            Arc::new(ToolResultPruningContextPolicy::default()),
            store,
            leases,
            64,
        )
        .expect("runtime"),
    );
    let bind_addr = bind.clone();
    let app = router(GhHostState {
        runtime,
        route: xharness_host::ModelRoute::new(&provider, &model),
        results: Default::default(),
    });
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .unwrap_or_else(|error| panic!("绑定 {bind_addr} 失败: {error}"));
    eprintln!("gh-host 就绪: http://{bind_addr}  (POST /tasks, GET /results/{{id}})");
    eprintln!("模型: {provider}/{model} @ {base_url}  状态目录: {state_dir}");
    axum::serve(listener, app).await.expect("serve");
}

// 防止 Mutex import 警告:results 类型由 server 定义,这里仅占位说明。
#[allow(dead_code)]
type _StateCheck = Arc<Mutex<()>>;
