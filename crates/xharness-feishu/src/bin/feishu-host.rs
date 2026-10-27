//! feishu-host：常驻 headless agent 服务。
//!
//! 环境变量：
//!   XH_FS_API_KEY      OpenAI 兼容端点密钥（默认按 DeepSeek 配置）。不设则降级 mock。
//!   XH_FS_BASE_URL     默认 https://api.deepseek.com
//!   XH_FS_MODEL        默认 deepseek-chat
//!   XH_FS_PROVIDER     默认 deepseek
//!   XH_FS_BIND         默认 127.0.0.1:3082
//!   XH_FS_STATE_DIR    默认 ./fs-state
//!   XH_FS_TASK_SECRET  设置后 /tasks 强制 Bearer 校验
//!   XH_FS_PERMISSION   workspace(默认) | full
//!   XH_FS_REASONING    思考强度，透传到 ModelRoute

use std::sync::{Arc, Mutex};

use xharness_agent::FileLeaseManager;
use xharness_context::ToolResultPruningContextPolicy;
use xharness_feishu::{router, FeishuHostState, FeishuScriptedProvider, FeishuToolFactory};
use xharness_gh::deployment::{build_registry, SingleModelDeployment};
use xharness_host::{
    DurableLoopAgentRuntime, ModelRoute, PermissionPreset,
};
use xharness_provider_openai::OpenAiProtocol;
use xharness_session_jsonl::JsonlSessionStore;

#[tokio::main]
async fn main() {
    let base_url =
        std::env::var("XH_FS_BASE_URL").unwrap_or_else(|_| "https://api.deepseek.com".to_owned());
    let model = std::env::var("XH_FS_MODEL").unwrap_or_else(|_| "deepseek-chat".to_owned());
    let provider = std::env::var("XH_FS_PROVIDER").unwrap_or_else(|_| "deepseek".to_owned());
    let state_dir = std::env::var("XH_FS_STATE_DIR").unwrap_or_else(|_| "./fs-state".to_owned());
    let bind = std::env::var("XH_FS_BIND").unwrap_or_else(|_| "127.0.0.1:3082".to_owned());
    let task_secret = std::env::var("XH_FS_TASK_SECRET")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let permission = match std::env::var("XH_FS_PERMISSION").as_deref() {
        Ok("full") => PermissionPreset::DangerFullAccess,
        _ => PermissionPreset::WorkspaceWrite,
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
    // web_search / web_fetch 在 host 进程内执行，不走沙箱，因此即使沙箱断网
    // 也能用。接上 DeepSeek 搜索后，查 GitHub 页面、搜资料都不需要给 bash 开网络。
    let factory = {
        let base = FeishuToolFactory::default();
        let wired = match std::env::var("XH_FS_SEARCH_API_KEY") {
            Ok(key) if !key.trim().is_empty() => base.with_deepseek_search(key),
            _ => match std::env::var("XH_FS_API_KEY") {
                Ok(key) if !key.trim().is_empty() => base.with_deepseek_search(key),
                _ => {
                    eprintln!("[xh-fs] 未配置搜索 provider，web_search 不可用（web_fetch 仍可用）");
                    base
                }
            },
        };
        Arc::new(wired)
    };

    let (runtime, mut route) = match std::env::var("XH_FS_API_KEY") {
        Ok(api_key) if !api_key.trim().is_empty() => {
            let deployment = SingleModelDeployment {
                provider: provider.clone(),
                model: model.clone(),
                base_url: base_url.clone(),
                api_key,
                protocol: OpenAiProtocol::ChatCompletions,
                context_window_tokens: 131_072,
                max_output_tokens: 8_192,
                minimum_output_tokens: None,
                token_safety_margin: 2_048,
            };
            let (route, registry) = build_registry(deployment)
                .await
                .unwrap_or_else(|error| panic!("模型部署初始化失败: {error}"));
            eprintln!("[xh-fs] 模型: {provider}/{model} @ {base_url}");
            let runtime = DurableLoopAgentRuntime::from_registry(
                route.clone(),
                registry,
                factory,
                Arc::new(ToolResultPruningContextPolicy::default()),
                store,
                leases,
                64,
            )
            .expect("runtime");
            (Arc::new(runtime), route)
        }
        _ => {
            eprintln!("[xh-fs] 未设置 XH_FS_API_KEY,模型降级为 mock(FeishuScriptedProvider)");
            let route = ModelRoute::new("mock", "scripted");
            let runtime = DurableLoopAgentRuntime::new(
                "mock",
                "scripted",
                Some(Arc::new(FeishuScriptedProvider)),
                factory,
                Arc::new(ToolResultPruningContextPolicy::default()),
                store,
                leases,
                64,
            );
            (Arc::new(runtime), route)
        }
    };

    if let Ok(effort) = std::env::var("XH_FS_REASONING") {
        if !effort.trim().is_empty() {
            route.reasoning_effort = Some(effort.trim().to_owned());
            eprintln!("[xh-fs] 思考强度: {}", effort.trim());
        }
    }

    let state = Arc::new(FeishuHostState {
        runtime,
        route,
        results: Arc::new(Mutex::new(Default::default())),
        workspace_root: std::path::Path::new(&state_dir).join("workspaces"),
        task_secret,
        permission,
    });

    let app = router(state);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|error| panic!("绑定 {bind} 失败: {error}"));
    eprintln!("[xh-fs] 监听 http://{bind}  (permission={permission:?})");
    axum::serve(listener, app).await.expect("serve");
}
