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
- 基线提交:`58d894140cb566865023481b49eed437b6ccb5fe`
- 拆分日期:2026-10-25
- 拆分方式:按 `xharness-core / xharness-agent / xharness-platform / xharness-provider-openai` 四根做依赖闭包 BFS,逐 crate 原样复制(源码零修改);`runtime/`、`scripts/fixtures/`、`config/providers.local-reasoning.example.json` 为测试所需的上游根目录资源,一并保留。
- git 历史未随迁;需要时可对上游做 `git filter-repo` graft。

## 已知差异

- CI 只覆盖 Linux + macOS;Windows 门控测试(如 `bundled_windows.rs`,依赖 `apps/desktop` 桌面资源)未随迁。
- 上游的架构边界检查(`config/architecture-*.json` + `check-architecture.py`)属于 host 仓库治理,未随迁;本仓库以 `cargo check/test` 为底线。

## 上游策略:冻结

运行时核心定位为基础设施,已冻结(基线 `58d8941`),不自动跟随上游——本地特殊设计因此没有同步冲突压力。同步机制保留为手动档:需要追上游时走 Actions 的 workflow_dispatch 或本地 `bash scripts/sync-upstream.sh`(按 members 清单重新抽取闭包 + cargo check,变更仍以 PR + CI 审查进入)。

例外:上游若修复闭包内 crate 的安全类问题(session/沙箱/鉴权),值得手动同步一次;常规功能迭代不追。

## 验证

```sh
cargo check --workspace --all-targets
cargo test --workspace
```

已在本仓库拆分时于 macOS (aarch64, Rust 1.99) 全量通过。

## gh 接入实验(M0 进行中)

`crates/xharness-gh` 已打通 headless 全链路(离线):POST /tasks → `admit_turn` 持久落账(要求用户消息带稳定 id)→ `start_turn` 启动 durable turn → mock provider 两步(gh_comment 工具调用 → 终止文本)→ 工具执行被记录 → 结果可查询。测试 `tests/full_loop.rs` 0.3s 内闭环。

架构:GitHub Actions 触发 → POST /tasks(host 的 Axum 端点)→ 每 (repo, issue) 一个持久 durable session;Actions 只做转发,重活在 host。宿主推荐与自托管 runner 同机,零公网暴露。

剩余:真实 provider 接线(OpenAiProviderConfig)、真实 gh client + marker 评论 upsert、`resume_session` 重启恢复路径、steering(`admit_steering`)、部署配置。

## 已知 flake 与环境差异

- `xharness-web` 的 SSRF 拒绝测试(`shared_download_transport_denies_private_fake_and_cleartext_targets`)在本机有系统代理(127.0.0.1:7897)时,IPv4-mapped IPv6 目标会被代理转发导致断言失败;CI 干净环境通过。
- `swarm_coordinator_review_rejects_forged_reference_and_ignores_unrelated_latest_turn`(host,30s 边界)对负载敏感:全套件并发下偶发超时,隔离复跑稳定通过。上游同步代码,不改动,以 CI 环境仲裁。

`swarm_coordinator_review_rejects_forged_reference_and_ignores_unrelated_latest_turn`(host,30s 边界)对负载敏感:全套件并发下偶发超时,隔离复跑 1.2s 稳定通过。上游同步代码,不改动,以 CI 环境仲裁。

## 下一步

1. gh-adapter:轮询/租约认领/评论 upsert,事件 → durable inbox 的单向映射;
2. Xandbox sandbox backend:fail-closed 的"VM 预隔离"实现,替换原生 Bubblewrap/Seatbelt 路径;
3. headless 二进制:最小化组合根(仅 runtime + provider + platform)。
