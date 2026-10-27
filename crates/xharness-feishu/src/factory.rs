//! 飞书侧的 SessionToolFactory。
//!
//! 与 `GhToolFactory` 的区别只在**工具集**：这里给的是通用 workspace 工具
//! （read / write / edit / glob / grep / bash），不带 gh_comment。会话标识
//! 用 `fs-{chat}`，同一会话的多条消息落在同一个 durable session 里，上下文
//! 自然累积。
//!
//! 权限模型刻意保守：默认 `WorkspaceWrite`（文件操作限制在会话 workspace
//! 内），只有显式设置 `XH_FS_PERMISSION=full` 才切到 `DangerFullAccess`
//! 并关闭工具审批 —— 因为飞书那边是聊天用户，不是仓库所有者。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use xharness_coding_tools::CodingToolBundle;
use xharness_host::{PermissionPreset, SessionToolFactory};
use xharness_jobs::{JobRegistry, JobRegistryConfig};
use xharness_platform::{NativePlatform, PlatformConfig};
use xharness_tools::{ToolExecutor, ToolRegistry};
use xharness_web::WebRuntime;

/// 每个 session 的 workspace 由调用方给出；这里只负责把工具挂上去。
pub struct FeishuToolFactory {
    pub jobs: Arc<JobRegistry>,
    pub web: Arc<WebRuntime>,
    /// 供测试断言工具确实被执行过。
    pub calls: Arc<Mutex<Vec<String>>>,
}

impl Default for FeishuToolFactory {
    fn default() -> Self {
        Self {
            jobs: Arc::new(JobRegistry::new(JobRegistryConfig::default())),
            web: Arc::new(WebRuntime::default()),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl SessionToolFactory for FeishuToolFactory {
    async fn executor(
        &self,
        session_id: &str,
        cwd: &str,
        permission: PermissionPreset,
    ) -> Result<ToolExecutor, String> {
        let mut config = PlatformConfig::new(cwd);
        if permission == PermissionPreset::DangerFullAccess {
            // 只有显式配置才会走到这里；仍保留会话 workspace 作为根。
            config = config.full_access();
        }
        let platform = Arc::new(NativePlatform::new(config).map_err(|error| error.to_string())?);

        let bundle = CodingToolBundle::new(
            Arc::clone(&platform),
            Arc::clone(&self.jobs),
            Arc::clone(&self.web),
            session_id,
            session_id,
        );
        let mut specs = bundle.specs();
        if permission == PermissionPreset::DangerFullAccess {
            // 无 UI 可审批；full 模式下视为已授权（与 GhToolFactory 一致）。
            for spec in &mut specs {
                spec.requires_approval = false;
            }
        }

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
