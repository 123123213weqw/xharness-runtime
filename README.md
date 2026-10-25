# XHarness Runtime

XHarness 的 headless 运行时核心:从 [x-harness-rs](https://github.com/123123213weqw/x-harness-rs) 拆出的纯运行时层,不含 Web 控制面与桌面/移动组合。用于无界面部署场景(如 gh 驱动的自动化 agent、沙箱内嵌执行),以及作为未来执行后端([Xandbox](https://github.com/123123213weqw/Xandbox))的接入点。

Apache-2.0。状态:**拆分验证中,尚未发布。**

## 包含什么

| 组件 | 职责 |
| --- | --- |
| `crates/xharness-core` | Provider 中立的 agent loop 与 run 控制 |
| `crates/xharness-agent` | 持久输入队列、监督与恢复 |
| `crates/xharness-session` / `session-jsonl` | append-only 会话事件日志(唯一真源) |
| `crates/xharness-tools` | 策略感知的工具注册与执行管线 |
| `crates/xharness-platform`(+fs/process/sandbox) | 原生文件、进程、沙箱边界(macOS/Linux/Windows) |
| `crates/xharness-provider-openai` | Chat Completions / Responses 适配 |
| `crates/xharness-context/compaction/prompt/token` | 上下文预算、压缩、System 组装、Token 计量 |
| `crates/xharness-goal/jobs/schedule/interaction` | 目标状态、后台作业、调度、交互 trait |

## 不包含什么(留在上游)

`xharness-host`(会话控制面/RPC/UI 投影)、`xharness-server`(HTTP/WS/静态托管)、Tauri 桌面、移动端、Telegram/微信渠道、cloud、computer-use、keyring。需要 UI 或渠道时,请使用上游完整运行时。

## 溯源

- 来源:`123123213weqw/x-harness-rs`
- 基线提交:`7eed575632105e47ab35e48d1e801f8ecb43a1db`
- 拆分日期:2026-10-25
- 拆分方式:按 `xharness-core / xharness-agent / xharness-platform / xharness-provider-openai` 四根做依赖闭包 BFS,逐 crate 原样复制(源码零修改);`runtime/`、`scripts/fixtures/`、`config/providers.local-reasoning.example.json` 为测试所需的上游根目录资源,一并保留。
- git 历史未随迁;需要时可对上游做 `git filter-repo` graft。

## 已知差异

- CI 只覆盖 Linux + macOS;Windows 门控测试(如 `bundled_windows.rs`,依赖 `apps/desktop` 桌面资源)未随迁。
- 上游的架构边界检查(`config/architecture-*.json` + `check-architecture.py`)属于 host 仓库治理,未随迁;本仓库以 `cargo check/test` 为底线。

## 验证

```sh
cargo check --workspace --all-targets
cargo test --workspace
```

已在本仓库拆分时于 macOS (aarch64, Rust 1.99) 全量通过。

## 下一步

1. gh-adapter:轮询/租约认领/评论 upsert,事件 → durable inbox 的单向映射;
2. Xandbox sandbox backend:fail-closed 的"VM 预隔离"实现,替换原生 Bubblewrap/Seatbelt 路径;
3. headless 二进制:最小化组合根(仅 runtime + provider + platform)。
