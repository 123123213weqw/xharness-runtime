//! GitHub 触发的 headless agent 任务入口(实验)。
//!
//! 架构:GitHub Actions 触发后 POST /tasks → `admit_turn` 持久落账 →
//! `start_turn` 启动 durable turn → 事件流排空 → 结果进入 results 表,
//! 由调用方轮询或由 gh_comment 工具直接回写。
//!
//! 实验范围:验证 runtime admission/tool 执行/多步 loop 全链路,不含真实
//! gh HTTP 与部署配置。

pub mod deployment;
pub mod factory;
pub mod provider_mock;
pub mod server;
