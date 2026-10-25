//! 最小 SessionToolFactory:每 session 一个 ToolExecutor,内含
//! CodingToolBundle(read/write/bash 等真实 workspace 工具)+ gh_comment。
//!
//! 权限模型:调用方传 `PermissionPreset::DangerFullAccess` 时,所有工具的
//! `requires_approval` 置 false(沿袭上游 host-app 的处理);未配置 gh 后端
//! 时,gh_comment 降级为本地记录。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use xharness_coding_tools::CodingToolBundle;
use xharness_host::{PermissionPreset, SessionToolFactory};
use xharness_jobs::{JobRegistry, JobRegistryConfig};
use xharness_platform::{NativePlatform, PlatformConfig};
use xharness_tools::{
    ToolDefinition, ToolExecutor, ToolHandlerError, ToolOutput, ToolRegistry, ToolSpec,
};
use xharness_web::WebRuntime;

use crate::client::{session_marker, GhClient, UpsertOutcome};

#[derive(Clone)]
pub struct GhToolConfig {
    pub client: Arc<GhClient>,
    pub repo: String,
    /// clone 用的 origin(URL 或本地路径);None 时用 https://github.com/{repo}.git。
    pub origin: Option<String>,
}

pub struct GhToolFactory {
    pub gh: Option<GhToolConfig>,
    pub calls: Arc<Mutex<Vec<Value>>>,
    pub jobs: Arc<JobRegistry>,
    pub web: Arc<WebRuntime>,
}

impl Default for GhToolFactory {
    fn default() -> Self {
        Self {
            gh: None,
            calls: Arc::new(Mutex::new(Vec::new())),
            jobs: Arc::new(JobRegistry::new(JobRegistryConfig::default())),
            web: Arc::new(WebRuntime::default()),
        }
    }
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
        cwd: &str,
        permission: PermissionPreset,
    ) -> Result<ToolExecutor, String> {
        // full access 不创建沙箱适配器;进程仍走 ProcessRuntime
        // (取消/超时/进程组回收保持有效)。宿主级隔离由部署层负责。
        let platform = Arc::new(
            NativePlatform::new(PlatformConfig::new(cwd).full_access())
                .map_err(|error| error.to_string())?,
        );
        let bundle = CodingToolBundle::new(
            Arc::clone(&platform),
            Arc::clone(&self.jobs),
            Arc::clone(&self.web),
            session_id,
            session_id,
        );
        let mut specs = bundle.specs();
        if permission == PermissionPreset::DangerFullAccess {
            for spec in &mut specs {
                spec.requires_approval = false;
            }
        }
        specs.push(self.gh_comment_spec(session_id));

        let registry = Arc::new(ToolRegistry::new());
        for spec in specs {
            registry
                .register(spec)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(ToolExecutor::new(registry))
    }
}

impl GhToolFactory {
    fn gh_comment_spec(&self, session_id: &str) -> ToolSpec {
        let calls = Arc::clone(&self.calls);
        let marker = session_marker(session_id);
        let (repo, client, issue) = match (&self.gh, parse_session_id(session_id)) {
            (Some(config), Some((_parsed_repo, parsed_issue))) => (
                config.repo.clone(),
                Some(Arc::clone(&config.client)),
                Some(parsed_issue),
            ),
            _ => (String::new(), None, None),
        };
        let definition = match (&client, &issue) {
            (Some(_), Some(_)) => ToolDefinition::new(
                "gh_comment",
                "Post or update this session's comment on the issue (marker upsert; \
                 one comment slot per session, call again to revise it).",
                json!({
                    "type": "object",
                    "properties": {"body": {"type": "string"}},
                    "required": ["body"]
                }),
            ),
            _ => ToolDefinition::new(
                "gh_comment",
                "Record a comment call (mock gh backend, no network).",
                json!({
                    "type": "object",
                    "properties": {"body": {"type": "string"}},
                    "required": ["body"]
                }),
            ),
        };
        ToolSpec::new(definition, move |ctx| {
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
        })
    }
}
