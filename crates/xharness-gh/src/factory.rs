//! 最小 SessionToolFactory:每个 session 一个 ToolExecutor,内含记录调用的
//! gh_comment 工具(实验中不发起真实 gh HTTP)。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use xharness_host::{PermissionPreset, SessionToolFactory};
use xharness_tools::{ToolDefinition, ToolExecutor, ToolOutput, ToolRegistry, ToolSpec};

#[derive(Clone, Default)]
pub struct GhToolFactory {
    pub calls: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl SessionToolFactory for GhToolFactory {
    async fn executor(
        &self,
        _session_id: &str,
        _cwd: &str,
        _permission: PermissionPreset,
    ) -> Result<ToolExecutor, String> {
        let registry = Arc::new(ToolRegistry::new());
        let definition = ToolDefinition::new(
            "gh_comment",
            "Post or update an issue comment (marker-based upsert).",
            json!({
                "type": "object",
                "properties": {
                    "issue_number": {"type": "integer"},
                    "body": {"type": "string"}
                },
                "required": ["issue_number", "body"]
            }),
        );
        let calls = Arc::clone(&self.calls);
        registry
            .register(ToolSpec::new(definition, move |ctx| {
                let calls = Arc::clone(&calls);
                async move {
                    calls.lock().unwrap().push(ctx.arguments.as_ref().clone());
                    Ok(ToolOutput::text("comment recorded (mock gh)"))
                }
            }))
            .await
            .map_err(|error| error.to_string())?;
        Ok(ToolExecutor::new(registry))
    }
}
