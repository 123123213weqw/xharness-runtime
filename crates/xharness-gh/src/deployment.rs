//! 单模型真实部署:OpenAI 兼容端点(DeepSeek 等)→ ModelRegistry。
//! 组合方式沿袭上游 host-app 的 single_with_attachments,去掉图片/推理
//! 兼容层与附件,保留 token guard 预算。

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use xharness_core::ModelProvider;
use xharness_host::{ModelDescriptor, ModelRegistry, ModelRoute, RegisteredModel};
use xharness_provider_openai::{OpenAiProtocol, OpenAiProvider, OpenAiProviderConfig};
use xharness_token::{TokenBudget, TokenGuard};

const UNKNOWN_CONTEXT_WORKING_BUDGET: u64 = 32_768;

#[derive(Clone, Debug)]
pub struct SingleModelDeployment {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub protocol: OpenAiProtocol,
    pub context_window_tokens: u64,
    pub max_output_tokens: u64,
    pub minimum_output_tokens: Option<u64>,
    pub token_safety_margin: u64,
}

pub fn token_guard(
    model: &str,
    context_window_tokens: Option<u64>,
    max_output_tokens: u64,
    minimum_output_tokens: Option<u64>,
    token_safety_margin: u64,
) -> Result<Option<TokenGuard>, String> {
    if model == "unconfigured" {
        return Ok(None);
    }
    let minimum = minimum_output_tokens
        .unwrap_or_else(|| xharness_host::automatic_minimum_output_tokens(max_output_tokens));
    let context_window_tokens = context_window_tokens.unwrap_or_else(|| {
        UNKNOWN_CONTEXT_WORKING_BUDGET.max(
            minimum
                .saturating_add(token_safety_margin)
                .saturating_add(UNKNOWN_CONTEXT_WORKING_BUDGET),
        )
    });
    TokenGuard::conservative(TokenBudget {
        context_window_tokens,
        reserved_output_tokens: max_output_tokens,
        minimum_output_tokens: minimum,
        safety_margin_tokens: token_safety_margin,
    })
    .map(Some)
    .map_err(|error| error.to_string())
}

/// 构建 (default_route, registry);registry 内含真实 provider 适配器与
/// provider 上报的上下文能力,token guard 按上报窗口计算。
pub async fn build_registry(
    config: SingleModelDeployment,
) -> Result<(ModelRoute, ModelRegistry), String> {
    let route = ModelRoute::new(&config.provider, &config.model);
    let provider_config = OpenAiProviderConfig::new(
        config.protocol,
        config.base_url,
        config.api_key,
        &config.model,
    )
    .with_context_window_fallback(Some(config.context_window_tokens));
    let adapter = OpenAiProvider::new(provider_config).map_err(|error| error.to_string())?;
    let capabilities = adapter
        .capabilities(CancellationToken::new())
        .await
        .map_err(|error| error.to_string())?;
    let guard = token_guard(
        &config.model,
        capabilities.context_window.effective_hard_max(),
        config.max_output_tokens,
        config.minimum_output_tokens,
        config.token_safety_margin,
    )?;
    let descriptor = ModelDescriptor::new(
        &config.provider,
        &config.provider,
        &config.model,
        &config.model,
    )
    .with_context_window(capabilities.context_window);
    let mut registry = ModelRegistry::new();
    registry
        .register(RegisteredModel::new(descriptor, Arc::new(adapter)).with_token_guard(guard))
        .map_err(|error| error.to_string())?;
    Ok((route, registry))
}
