//! 飞书场景的离线 mock provider。
//!
//! 不能直接用 `xharness_gh::provider_mock::WorkspaceScriptedProvider`：它第一步
//! 硬编码调用 `gh_comment` 工具，而飞书的工具集里没有这个工具，turn 会以
//! "未知工具" 失败、永远产不出最终文本。
//!
//! 这里改用 `read`（名字跨平台固定，不像 shell 工具那样随平台变成 bash/zsh/
//! powershell）：先读一个文件，拿到工具结果后再输出终止文本。这样仍然完整
//! 走一遍工具管线，而不是绕过它。

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use xharness_core::{
    FinishReason, ModelCapabilities, ModelProvider, ProviderError, ProviderEvent, ProviderRequest,
    ProviderStream,
};
use xharness_session::MessageRole;

pub const MOCK_FINAL_TEXT: &str = "done: file read by mock";

/// mock 会读这个相对路径。不存在时工具报错，但 loop 仍会继续并终止。
pub const MOCK_TARGET_FILE: &str = "README.md";

#[derive(Default)]
pub struct FeishuScriptedProvider;

#[async_trait]
impl ModelProvider for FeishuScriptedProvider {
    fn provider_name(&self) -> &str {
        "mock"
    }

    fn model_name(&self) -> Option<&str> {
        Some("feishu-scripted")
    }

    async fn capabilities(
        &self,
        _cancellation: CancellationToken,
    ) -> Result<ModelCapabilities, ProviderError> {
        Ok(ModelCapabilities::default())
    }

    async fn stream(
        &self,
        request: ProviderRequest,
        _cancellation: CancellationToken,
    ) -> Result<ProviderStream, ProviderError> {
        let tool_result_seen = request
            .messages
            .last()
            .map(|message| message.role == MessageRole::Tool)
            .unwrap_or(false);

        let (events, finish) = if tool_result_seen {
            (
                vec![ProviderEvent::TextDelta(MOCK_FINAL_TEXT.to_owned())],
                FinishReason::Stop,
            )
        } else {
            (
                vec![ProviderEvent::ToolCallDelta {
                    index: 0,
                    id: "call-1".to_owned(),
                    name: "read".to_owned(),
                    arguments_delta: format!(r#"{{"path":"{MOCK_TARGET_FILE}"}}"#),
                }],
                FinishReason::ToolCalls,
            )
        };

        let stream = async_stream::stream! {
            for event in events {
                yield Ok(event);
            }
            yield Ok(ProviderEvent::Completed {
                finish_reason: Some(finish),
                usage: None,
                provider_items: Vec::new(),
            });
        };
        Ok(Box::pin(stream))
    }
}
