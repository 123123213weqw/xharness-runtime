//! 最小 SessionToolFactory:每个 session 一个 ToolExecutor。
//! 配置了 gh 后端时,gh_comment 执行真实的 marker upsert;否则降级为
//! 本地记录(离线测试/无 token 部署)。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use xharness_host::{PermissionPreset, SessionToolFactory};
use xharness_tools::{
    ToolDefinition, ToolExecutor, ToolHandlerError, ToolOutput, ToolRegistry, ToolSpec,
};

use crate::client::{session_marker, GhClient, UpsertOutcome};

#[derive(Clone)]
pub struct GhToolConfig {
    pub client: Arc<GhClient>,
    pub repo: String,
}

#[derive(Clone, Default)]
pub struct GhToolFactory {
    pub gh: Option<GhToolConfig>,
    pub calls: Arc<Mutex<Vec<Value>>>,
}

/// session_id 形如 `gh-{repo,'/'→'--'}-{issue}`;从尾部解析,repo 中的
/// 连字符由双横线转义保证无歧义。
pub fn parse_session_id(session_id: &str) -> Option<(String, u64)> {
    let rest = session_id.strip_prefix("gh-")?;
    let (repo_part, issue) = rest.rsplit_once('-')?;
    let issue: u64 = issue.parse().ok()?;
    Some((repo_part.replace("--", "/"), issue))
}

#[async_trait]
impl SessionToolFactory for GhToolFactory {
    async fn executor(
        &self,
        session_id: &str,
        _cwd: &str,
        _permission: PermissionPreset,
    ) -> Result<ToolExecutor, String> {
        let registry = Arc::new(ToolRegistry::new());
        let calls = Arc::clone(&self.calls);
        let marker = session_marker(session_id);
        let (definition, repo, client, issue) = match (&self.gh, parse_session_id(session_id)) {
            (Some(config), Some((_parsed_repo, parsed_issue))) => (
                ToolDefinition::new(
                    "gh_comment",
                    "Post or update this session's comment on the issue (marker upsert; \
                     one comment slot per session, call again to revise it).",
                    json!({
                        "type": "object",
                        "properties": {"body": {"type": "string"}},
                        "required": ["body"]
                    }),
                ),
                config.repo.clone(),
                Some(Arc::clone(&config.client)),
                Some(parsed_issue),
            ),
            _ => (
                ToolDefinition::new(
                    "gh_comment",
                    "Record a comment call (mock gh backend, no network).",
                    json!({
                        "type": "object",
                        "properties": {"body": {"type": "string"}},
                        "required": ["body"]
                    }),
                ),
                String::new(),
                None,
                None,
            ),
        };
        registry
            .register(ToolSpec::new(definition, move |ctx| {
                let calls = Arc::clone(&calls);
                let repo = repo.clone();
                let client = client.clone();
                let marker = marker.clone();
                let issue = issue;
                async move {
                    let args = ctx.arguments.as_ref().clone();
                    calls.lock().unwrap().push(args.clone());
                    let body = args
                        .get("body")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    match (client, issue) {
                        (Some(client), Some(issue)) => {
                            let outcome = client
                                .upsert_session_comment(&repo, issue, &marker, &body)
                                .await
                                .map_err(ToolHandlerError::new)?;
                            let note = match outcome {
                                UpsertOutcome::Created { .. } => "created",
                                UpsertOutcome::Updated { .. } => "updated",
                            };
                            Ok(ToolOutput::text(format!(
                                "comment {note} on {repo}#{issue}"
                            )))
                        }
                        _ => Ok(ToolOutput::text("comment recorded (mock gh)")),
                    }
                }
            }))
            .await
            .map_err(|error| error.to_string())?;
        Ok(ToolExecutor::new(registry))
    }
}

// ToolHandlerError 构造器的借用形态,避免闭包内重复书写。
#[allow(dead_code)]
fn _error(message: impl Into<String>) -> ToolHandlerError {
    ToolHandlerError::new(message)
}
