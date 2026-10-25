//! 崩溃恢复验证:gated provider 让 turn 停在运行中,丢弃整个 runtime(模拟
//! 进程死亡),在同一 state 目录上重建 host 并运行 recover_pending——
//! 已落账未完成的输入应续跑至完成,结果可查询。已完成的输入不重复消费。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use xharness_agent::FileLeaseManager;
use xharness_context::ToolResultPruningContextPolicy;
use xharness_gh::factory::GhToolFactory;
use xharness_gh::server::{recover_pending, submit_inner, GhHostState, TaskPayload};
use xharness_host::{DurableLoopAgentRuntime, ModelRoute};
use xharness_session_jsonl::JsonlSessionStore;

/// 门控 provider:门文件出现前 hold 流(模拟慢任务),出现后输出终稿。
struct GateProvider {
    gate: PathBuf,
}

#[async_trait::async_trait]
impl xharness_core::ModelProvider for GateProvider {
    fn provider_name(&self) -> &str {
        "mock"
    }
    fn model_name(&self) -> Option<&str> {
        Some("gated")
    }
    async fn stream(
        &self,
        _request: xharness_core::ProviderRequest,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<xharness_core::ProviderStream, xharness_core::ProviderError> {
        let gate = self.gate.clone();
        let stream = async_stream::stream! {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
            while !gate.exists() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            yield Ok(xharness_core::ProviderEvent::TextDelta("recovered-final".to_owned()));
            yield Ok(xharness_core::ProviderEvent::Completed {
                finish_reason: Some(xharness_core::FinishReason::Stop),
                usage: None,
                provider_items: Vec::new(),
            });
        };
        Ok(Box::pin(stream))
    }
}

fn build_state(dir: &Path, gate: PathBuf) -> GhHostState {
    let store = Arc::new(
        JsonlSessionStore::new(dir.join("sessions"))
            .expect("store")
            .for_runtime(),
    );
    let leases = Arc::new(FileLeaseManager::new(dir.join("leases")).expect("leases"));
    let runtime = Arc::new(DurableLoopAgentRuntime::new(
        "mock",
        "gated",
        Some(Arc::new(GateProvider { gate })),
        Arc::new(GhToolFactory::default()),
        Arc::new(ToolResultPruningContextPolicy::default()),
        store,
        leases,
        64,
    ));
    GhHostState {
        runtime,
        route: ModelRoute::new("mock", "gated"),
        results: Default::default(),
        gh: None,
        workspace_root: dir.join("workspaces"),
        task_secret: None,
        seen_comments: Default::default(),
    }
}

#[tokio::test]
async fn pending_input_survives_host_restart_and_completes() {
    let dir = std::env::temp_dir().join(format!(
        "xh-recovery-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
    ));
    let gate = dir.join("gate.flag");
    let payload = TaskPayload {
        repo: "acme/x".to_owned(),
        issue: 11,
        title: "crash recovery".to_owned(),
        body: "长任务".to_owned(),
        message_tag: Some("comment-777".to_owned()),
        pr: None,
    };

    // 第一段:独立线程 + 独立 runtime 承载 host A;任务卡在门上时
    // 结束线程 = 模拟进程死亡(Runtime drop 取消其全部任务,文件锁释放)。
    {
        let dir_a = dir.clone();
        let gate_a = gate.clone();
        let payload_a = payload.clone();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("runtime A");
            runtime.block_on(async move {
                let state = build_state(&dir_a, gate_a);
                submit_inner(&state, payload_a, false)
                    .await
                    .expect("first submit");
                // 任务已 admit 且 turn 在门上 hold;确认未完成。
                tokio::time::sleep(Duration::from_millis(300)).await;
                assert!(
                    state.results.lock().unwrap().get("gh-acme--x-11").is_none(),
                    "门未开,任务不应完成"
                );
            });
            // runtime 在此 drop:任务被取消,租约释放——"进程死亡"。
        });
        handle.join().expect("phase A");
        assert!(dir.join("workspaces/gh-acme--x-11/task.json").exists());
    }

    // 第二段:同 state 目录重建 host,恢复应驱动 pending 输入;开门后完成。
    {
        let state = build_state(&dir, gate.clone());
        recover_pending(&state).await;
        std::fs::write(&gate, "1").unwrap();
        let mut final_text = String::new();
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let results = state.results.lock().unwrap();
            if let Some(outcome) = results.get("gh-acme--x-11") {
                if outcome.status == "done" {
                    final_text = outcome.final_text.clone();
                    break;
                }
            }
        }
        assert_eq!(final_text, "recovered-final", "恢复后应续跑至完成");

        // 第三段:再次恢复不应把已完成输入再跑一遍(无新完成事件)。
        std::fs::remove_file(&gate).unwrap();
        let state2 = build_state(&dir, gate.clone());
        recover_pending(&state2).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            state2
                .results
                .lock()
                .unwrap()
                .get("gh-acme--x-11")
                .is_none(),
            "已完成输入不得重复消费(inbox 去重)"
        );
    }
    let _ = Value::Null;
    let _ = json!({});
}
