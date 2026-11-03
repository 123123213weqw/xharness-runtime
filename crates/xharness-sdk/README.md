# XHarness Rust SDK（0.1）

放在 **xharness-runtime 原仓库**，不另开 SDK 仓库。它是现有
`DurableLoopAgentRuntime` 的进程内薄入口，不是另一套 Agent 引擎或 HTTP 服务。

```text
产品：SuperAgent / 其他自动化 Agent
                  ↓
             xharness-sdk
                  ↓
      既有 host / agent / core / tools / session
```

SDK 不依赖 SuperAgent、GitHub/飞书接入、工单或业务数据库。现阶段内部仍依赖
`xharness-host`，并没有声称已把 host 完全拆开。版本 0.1 尚不是稳定 1.0 API；
公开的原生类型保持同一 Rust 类型身份，而非复制一套请求/事件类型。

## 第一版能力

| 能力 | 入口 | 语义 |
|---|---|---|
| 装配 | `RuntimeBuilder::new(...).build()` | 显式注入模型、工具工厂、Store 和 LeaseManager；不启动模型/来源/恢复扫描 |
| 空会话 | `create_session` | 仅创建原生 journal，不激活执行；已有会话返回原生冲突 |
| 读取 | `session` | 从同一权威 Store 读取，不维护 SDK 聊天历史 |
| 排队 | `queue` | 持久准入但不唤醒；不是执行完成回执 |
| 执行 | `start_turn` | 原生准入、预装事件观察者、唤醒；用户消息需要稳定 ID |
| 事件和控制 | `Turn::next_event/send/cancel/result` | 原生事件、控制与结果；取消不等于任务已清理完成 |
| 单会话恢复 | `resume_session/take_resumed_turn` | 读取已有 journal，不重新发送旧输入 |
| 关闭 | `shutdown` | 消耗 Runtime owner，调用原生有界关闭，返回清理报告 |

模型注册、权限 preset、工具参数验证、错误和会话格式沿用原生组件。
Builder 必须提供可路由的默认模型；不会把未配置模型当成成功的 Runtime。
默认事件容量 256，可通过 `NonZeroUsize` 显式设置。长时间不读取事件仍可能落后，
SDK 不额外承诺无损事件缓存；权威记录仍是 Session journal。

## 最小例子

可编译的离线例子：`examples/offline_echo.rs`。模型是明确的内存协议夹具，
不连接真实模型，也不调用任何付费服务。会话实际落在 JSONL 中：

```rust,ignore
let runtime = RuntimeBuilder::new(
    route, models, tool_factory, store, leases,
).build()?;

let mut turn = runtime.start_turn(request_with_stable_message_id).await?;
while let Some(event) = turn.next_event().await {
    // 投影给自己的界面或日志；不另建可执行聊天历史。
}
let result = turn.result().await;
drop(turn);
let shutdown_report = runtime.shutdown(grace).await;
```

所有 Rust 编译、检查、测试、例子执行仅在 V100 上同步当前源码后进行。
例如在远程项目目录：

```sh
cargo test --locked -p xharness-sdk --all-targets
cargo run --locked -p xharness-sdk --example offline_echo -- /absolute/isolated/demo-state
```

## 工具变化怎么办

工具列表由产品提供的 `SessionToolFactory` 决定，SDK 不固定工具名。
一个 executor 的工具声明和 handler 必须来自同一份受控 registry。工厂会收到
实际 session、cwd 和 `PermissionPreset`；文件/进程工具必须继续安装原生策略、
guard 和审批管线。SDK 不会仅凭 preset 自动给任意自定义 handler 加沙箱。

第一版不提供工具热替换：排空当前任务、关闭旧 Runtime，再用配套的新工具
Schema/handler 构造新 Runtime。默认不把运行中的调用改派到新版 handler。
恢复旧的待执行工具调用前，产品应验证工具兼容性；不兼容要阻止恢复并处理迁移，
本 SDK 不会自动重写旧调用参数，也不声称已有通用工具版本兼容校验器。

## 所有权与边界

- Store、LeaseManager、模型和工具实例由宿主注入。持久性取决于 Store 后端；
  `MemorySessionStore` 不能冒充跨进程持久化。跨进程互斥用共享作用域的文件 lease。
- SDK 只保留同一个 Store 的引用；无第二 Inbox、重试循环、聊天库或业务调度器。
- 自定义工具工厂仍负责自己的资源、权限策略和有界 `shutdown`。
- `Turn` 是观察/控制句柄；丢弃它不是取消。替换 Runtime 前显式关闭，检查报告，
  结束/释放旧 Turn，避免旧持有者继续占用 lease。
- 原生事件/请求类型在 0.1 中公开；升级仍需检查变化，不承诺 ABI 或任意版本混用。
- SDK 本身不持久化产品的模型、cwd、权限和 Prompt 选择。宿主须保存并在恢复时
  提供同一会话的已验证配置；不得从不可信输入临时提高权限。
- 完整 SuperAgent 迁入 SDK、受控 followup/nearest-step 扩展和 host 内部解耦是
  后续适配；本提交不覆盖产品仓库里的既有受控补丁，也不切换线上实例。

## 版本跟随

未来产品仓库通过 `git + rev = "<完整提交 SHA>"` 消费本 crate；同一产品若仍直接
使用其他 XHarness crate，应统一到相同来源/提交，避免混用不同类型版本。
在 SDK 提交被合并/发布前，不写一个不存在的发布版本。当前先在 Runtime 仓库
完成源代码和离线验证，再进行产品迁移与独立的生产灰度。
