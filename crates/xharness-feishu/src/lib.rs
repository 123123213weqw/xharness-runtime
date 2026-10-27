//! 飞书 headless host：把 XHarness 运行时包成一个通用 agent 服务。
//!
//! 与 `xharness-gh` 的关系：那个 crate 是 GitHub 场景的专用入口（payload 绑定
//! repo/issue、prompt 固定为 PR 评审分级）。这里复用同一套底层——模型注册、
//! durable session、工具管线、崩溃恢复——只把入口换成飞书，payload 变成通用
//! 文本，因此也能被其它渠道调用。
//!
//! 分工：飞书长连接、去重、状态机、消息格式化留在 TypeScript 的 bots 侧；
//! 这里只负责"跑 turn"这一件事。这样渠道演进不会牵动 Rust，反之亦然。

pub mod factory;
pub mod provider_mock;
pub mod server;

pub use factory::FeishuToolFactory;
pub use provider_mock::FeishuScriptedProvider;
pub use server::{router, session_id_for, FeishuHostState, TaskOutcome, TaskPayload};
