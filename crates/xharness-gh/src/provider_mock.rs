//! 脚本化 mock provider:第一步发起 gh_comment 工具调用,第二步读取工具
//! 结果后输出终止文本。用于离线验证 loop 的多步执行与工具管线。

use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;
use xharness_core::{
    FinishReason, ModelCapabilities, ModelProvider, ProviderError, ProviderEvent, ProviderRequest,
    ProviderStream,
};
use xharness_session::MessageRole;

pub const MOCK_FINAL_TEXT: &str = "done: comment posted by mock";

#[derive(Default)]
pub struct ScriptedProvider;

#[async_trait]
impl ModelProvider for ScriptedProvider {
    fn provider_name(&self) -> &str {
        "mock"
    }

    fn model_name(&self) -> Option<&str> {
        Some("scripted")
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
                    name: "gh_comment".to_owned(),
                    arguments_delta: r#"{"issue_number":7,"body":"fixed by mock agent"}"#
                        .to_owned(),
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

// ProviderStream 是 Pin<Box<dyn Stream>>,显式借用以约束泛型输出类型。
#[allow(dead_code)]
type _StreamBound = Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderError>> + Send>>;

/// workspace 工具链验证脚本:① bash 执行 printf → ② gh_comment 报告 bash
/// 输出 → ③ 终止文本。内容驱动,不依赖调用计数。
pub struct WorkspaceScriptedProvider;

#[async_trait]
impl ModelProvider for WorkspaceScriptedProvider {
    fn provider_name(&self) -> &str {
        "mock"
    }

    fn model_name(&self) -> Option<&str> {
        Some("scripted")
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
        let last_tool_output = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::Tool)
            .map(|message| message.content.clone())
            .unwrap_or_default();
        if std::env::var_os("XH_GH_TRACE").is_some() {
            eprintln!(
                "[xh-gh][provider] step={} last_tool_output={last_tool_output:?}",
                request.step
            );
        }
        // 工具结果 content 是双重编码:外层 {"content": "<内层 ToolResult JSON>"}。
        let stdout = serde_json::from_str::<serde_json::Value>(&last_tool_output)
            .ok()
            .and_then(|outer| outer["content"].as_str().map(str::to_owned))
            .and_then(|inner| serde_json::from_str::<serde_json::Value>(&inner).ok())
            .and_then(|result| result["stdout"].as_str().map(str::to_owned))
            .unwrap_or_default();
        let (name, arguments, finish) = if stdout.contains("hello-from-workspace") {
            (
                "gh_comment",
                serde_json::to_string(&serde_json::json!({
                    "body": format!("bash 输出: {stdout}")
                }))
                .expect("arguments json"),
                FinishReason::ToolCalls,
            )
        } else if last_tool_output.contains("comment ") {
            ("", String::new(), FinishReason::Stop)
        } else {
            (
                "bash",
                r#"{"command":"printf hello-from-workspace"}"#.to_owned(),
                FinishReason::ToolCalls,
            )
        };
        let mut events: Vec<ProviderEvent> = Vec::new();
        if !name.is_empty() {
            events.push(ProviderEvent::ToolCallDelta {
                index: 0,
                id: format!("call-{}", request.step),
                name: name.to_owned(),
                arguments_delta: arguments,
            });
        } else {
            events.push(ProviderEvent::TextDelta(MOCK_FINAL_TEXT.to_owned()));
        }
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
