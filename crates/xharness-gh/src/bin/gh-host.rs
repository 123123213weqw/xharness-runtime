//! gh-host:常驻 headless 任务入口。
//!
//! 模型(二选一):
//!   真实:XH_GH_API_KEY(OpenAI 兼容端点密钥,默认按 DeepSeek 配置)
//!   mock :不设 XH_GH_API_KEY 时降级为 ScriptedProvider(离线演示)
//!
//! gh 回写(可选):XH_GH_TOKEN + XH_GH_REPO 同时设置时,gh_comment 工具
//! 与完成钩子执行真实 marker upsert / label;否则降级为本地记录。
//!
//! 其他:XH_GH_BASE_URL(默认 https://api.deepseek.com)、
//!       XH_GH_MODEL(默认 deepseek-chat)、XH_GH_PROVIDER(默认 deepseek)、
//!       XH_GH_CONTEXT_WINDOW(默认 131072)、XH_GH_STATE_DIR(默认 ./gh-state)、
//!       XH_GH_BIND(默认 127.0.0.1:3081)

use std::sync::Arc;

use xharness_agent::FileLeaseManager;
use xharness_context::ToolResultPruningContextPolicy;
use xharness_gh::client::GhClient;
use xharness_gh::deployment::{build_registry, SingleModelDeployment};
use xharness_gh::factory::{GhToolConfig, GhToolFactory};
use xharness_gh::provider_mock::WorkspaceScriptedProvider;
use xharness_gh::server::{router, GhHostState};
use xharness_host::{DurableLoopAgentRuntime, ModelRoute};
use xharness_provider_openai::OpenAiProtocol;
use xharness_session_jsonl::JsonlSessionStore;

#[tokio::main]
async fn main() {
    let base_url =
        std::env::var("XH_GH_BASE_URL").unwrap_or_else(|_| "https://api.deepseek.com".to_owned());
    let model = std::env::var("XH_GH_MODEL").unwrap_or_else(|_| "deepseek-chat".to_owned());
    let provider = std::env::var("XH_GH_PROVIDER").unwrap_or_else(|_| "deepseek".to_owned());
    let context_window: u64 = std::env::var("XH_GH_CONTEXT_WINDOW")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(131_072);
    let state_dir = std::env::var("XH_GH_STATE_DIR").unwrap_or_else(|_| "./gh-state".to_owned());
    let bind = std::env::var("XH_GH_BIND").unwrap_or_else(|_| "127.0.0.1:3081".to_owned());

    let gh = match (std::env::var("XH_GH_TOKEN"), std::env::var("XH_GH_REPO")) {
        (Ok(token), Ok(repo)) if !token.trim().is_empty() && !repo.trim().is_empty() => {
            Some(GhToolConfig {
                client: Arc::new(GhClient::new(token)),
                repo: repo.trim().to_owned(),
            })
        }
        _ => {
            eprintln!("[xh-gh] 未设置 XH_GH_TOKEN/XH_GH_REPO,gh 回写降级为本地记录");
            None
        }
    };

    let store = Arc::new(
        JsonlSessionStore::new(std::path::Path::new(&state_dir).join("sessions"))
            .expect("session store")
            .for_runtime(),
    );
    let leases = Arc::new(
        FileLeaseManager::new(std::path::Path::new(&state_dir).join("leases"))
            .expect("lease manager"),
    );

    let (runtime, route) = match std::env::var("XH_GH_API_KEY") {
        Ok(api_key) if !api_key.trim().is_empty() => {
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
            eprintln!("[xh-gh] 模型: {provider}/{model} @ {base_url}");
            let runtime = DurableLoopAgentRuntime::from_registry(
                route.clone(),
                registry,
                Arc::new(GhToolFactory {
                    gh: gh.clone(),
                    ..Default::default()
                }),
                Arc::new(ToolResultPruningContextPolicy::default()),
                store,
                leases,
                64,
            )
            .expect("runtime");
            (Arc::new(runtime), route)
        }
        _ => {
            eprintln!("[xh-gh] 未设置 XH_GH_API_KEY,模型降级为 mock(WorkspaceScriptedProvider)");
            let route = ModelRoute::new("mock", "scripted");
            let runtime = DurableLoopAgentRuntime::new(
                "mock",
                "scripted",
                Some(Arc::new(WorkspaceScriptedProvider)),
                Arc::new(GhToolFactory {
                    gh: gh.clone(),
                    ..Default::default()
                }),
                Arc::new(ToolResultPruningContextPolicy::default()),
                store,
                leases,
                64,
            );
            (Arc::new(runtime), route)
        }
    };

    let bind_addr = bind.clone();
    let app = router(GhHostState {
        runtime,
        route,
        results: Default::default(),
        gh,
        workspace_root: std::path::Path::new(&state_dir).join("workspaces"),
    });
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .unwrap_or_else(|error| panic!("绑定 {bind_addr} 失败: {error}"));
    eprintln!("gh-host 就绪: http://{bind_addr}  (POST /tasks, GET /results/{{id}})");
    eprintln!("状态目录: {state_dir}");
    axum::serve(listener, app).await.expect("serve");
}
