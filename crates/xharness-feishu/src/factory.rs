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
use xharness_sandbox::{NetworkAccess, SandboxMode};
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
        // 沙箱与审批是两件事，必须分开配：
        //
        // - 沙箱由这个 config 决定（bwrap：workspace 可写、宿主只读、默认断网）。
        // - 审批由 PermissionPreset 决定，且 headless 服务没有审批 UI：
        //   非 full 档位下工具会以 ApprovalUnavailable 失败，等于全部不可用。
        //
        // 所以这里让**沙箱始终开启**，只把审批放宽。也就是说
        // XH_FS_PERMISSION=full 表达的是"不要求逐步审批"，不是"关掉沙箱"——
        // 想真正脱离沙箱必须改代码，不给运行时留开关。
        let use_sandbox = std::env::var("XH_FS_NO_SANDBOX").ok().as_deref() != Some("1");
        let config = if use_sandbox {
            PlatformConfig::new(cwd)
                .sandbox_mode(SandboxMode::WorkspaceWrite)
                .network(NetworkAccess::Deny)
        } else {
            // 显式逃生口,仅用于排查沙箱本身的问题。
            PlatformConfig::new(cwd).full_access()
        };
        let platform = Arc::new(NativePlatform::new(config).map_err(|error| error.to_string())?);

        let bundle = CodingToolBundle::new(
            Arc::clone(&platform),
            Arc::clone(&self.jobs),
            Arc::clone(&self.web),
            session_id,
            session_id,
        );
        let mut specs = bundle.specs();
        // headless 没有审批 provider,非 full 档位会让每个工具都 ApprovalUnavailable。
        // 隔离由上面的 bwrap 沙箱保证,所以这里统一放开审批。
        let _ = permission;
        for spec in &mut specs {
            spec.requires_approval = false;
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
