//! Review 管道离线验证。用本地 git fixture 做 origin、假 gh API 提供 PR
//! 元数据、即时完成 provider;poll_once 触发后断言 workspace 检出、diff
//! 注入 prompt、结果评论含 P0/P1 分级与复现块、label 正确。

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{extract::State, Json as AxumJson, Router};
use serde_json::{json, Value};
use xharness_agent::FileLeaseManager;
use xharness_context::ToolResultPruningContextPolicy;
use xharness_gh::client::GhClient;
use xharness_gh::factory::{GhToolConfig, GhToolFactory};
use xharness_gh::server::{poll_once, GhHostState};
use xharness_host::{DurableLoopAgentRuntime, ModelRoute};
use xharness_session_jsonl::JsonlSessionStore;

/// 即时完成 provider:直接输出固定 review 文本(带 P0 引证)。
struct InstantReviewProvider;

#[async_trait::async_trait]
impl xharness_core::ModelProvider for InstantReviewProvider {
    fn provider_name(&self) -> &str {
        "mock"
    }
    fn model_name(&self) -> Option<&str> {
        Some("instant")
    }
    async fn stream(
        &self,
        _request: xharness_core::ProviderRequest,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<xharness_core::ProviderStream, xharness_core::ProviderError> {
        let text = "## 总结\n改动把加法写成了减法。\n## P0\n- `src/calc.rs:4` 符号错误:1+1 得 0。已验证:cargo test 失败。\n## P1\n无\n## P2\n无\n## 验证建议\n- 运行 `cargo test`";
        let stream = async_stream::stream! {
            yield Ok(xharness_core::ProviderEvent::TextDelta(text.to_owned()));
            yield Ok(xharness_core::ProviderEvent::Completed {
                finish_reason: Some(xharness_core::FinishReason::Stop),
                usage: None,
                provider_items: Vec::new(),
            });
        };
        Ok(Box::pin(stream))
    }
}

fn git(dir: &PathBuf, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

fn git_out(dir: &PathBuf, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

#[derive(Default, Clone)]
struct FakeGh {
    comments: Arc<Mutex<Vec<String>>>,
    labels: Arc<Mutex<Vec<String>>>,
    base_sha: Arc<String>,
    head_sha: Arc<String>,
}

async fn list_repo_comments(State(fake): State<FakeGh>) -> AxumJson<Value> {
    let comments = fake.comments.lock().unwrap().clone();
    AxumJson(Value::Array(
        comments
            .iter()
            .enumerate()
            .map(|(index, body)| {
                json!({"id": 100 + index, "body": body,
                       "issue_url": "https://api.xh/repos/acme/x/issues/7",
                       "user": {"login": "acme"}})
            })
            .collect(),
    ))
}

async fn get_issue() -> AxumJson<Value> {
    AxumJson(json!({"number": 7, "title": "fix calc", "body": "改了个符号", "pull_request": {}}))
}

async fn get_pull(State(fake): State<FakeGh>) -> AxumJson<Value> {
    AxumJson(json!({
        "number": 7, "title": "fix calc", "body": null,
        "head": {"sha": *fake.head_sha, "ref": "feature"},
        "base": {"sha": *fake.base_sha, "ref": "main"},
    }))
}

async fn list_issue_comments(State(fake): State<FakeGh>) -> AxumJson<Value> {
    let comments = fake.comments.lock().unwrap().clone();
    AxumJson(Value::Array(
        comments
            .iter()
            .enumerate()
            .map(|(index, body)| json!({"id": 500 + index, "body": body}))
            .collect(),
    ))
}

async fn create_comment(
    State(fake): State<FakeGh>,
    AxumJson(payload): AxumJson<Value>,
) -> AxumJson<Value> {
    fake.comments
        .lock()
        .unwrap()
        .push(payload["body"].as_str().unwrap_or_default().to_owned());
    AxumJson(json!({"id": 900, "body": "ok"}))
}

async fn patch_comment(
    State(fake): State<FakeGh>,
    AxumJson(payload): AxumJson<Value>,
) -> AxumJson<Value> {
    fake.comments
        .lock()
        .unwrap()
        .push(payload["body"].as_str().unwrap_or_default().to_owned());
    AxumJson(json!({"id": 900, "body": "ok"}))
}

async fn add_labels(
    State(fake): State<FakeGh>,
    AxumJson(payload): AxumJson<Value>,
) -> AxumJson<Value> {
    if let Some(labels) = payload["labels"].as_array() {
        for label in labels {
            fake.labels
                .lock()
                .unwrap()
                .push(label.as_str().unwrap_or_default().to_owned());
        }
    }
    AxumJson(json!([]))
}

#[tokio::test]
async fn poll_on_pr_comment_builds_review_workspace_and_posts_graded_result() {
    let dir = std::env::temp_dir().join(format!(
        "xh-review-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
    ));
    // git fixture:base 加法,head 改成减法(P0 素材)
    let origin = dir.join("origin");
    std::fs::create_dir_all(origin.join("src")).unwrap();
    std::fs::write(
        origin.join("src/calc.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    )
    .unwrap();
    git(&origin, &["init", "-q", "-b", "main"]);
    git(&origin, &["add", "."]);
    git(&origin, &["commit", "-q", "-m", "base"]);
    let base_sha = git_out(&origin, &["rev-parse", "HEAD"]);
    std::fs::write(
        origin.join("src/calc.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
    )
    .unwrap();
    git(&origin, &["commit", "-q", "-am", "head"]);
    let head_sha = git_out(&origin, &["rev-parse", "HEAD"]);

    let fake = FakeGh {
        base_sha: Arc::new(base_sha),
        head_sha: Arc::new(head_sha),
        ..Default::default()
    };
    let app = Router::new()
        .route(
            "/repos/{owner}/{repo}/issues/comments",
            axum::routing::get(list_repo_comments),
        )
        .route(
            "/repos/{owner}/{repo}/issues/7",
            axum::routing::get(get_issue),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/7",
            axum::routing::get(get_pull),
        )
        .route(
            "/repos/{owner}/{repo}/issues/7/comments",
            axum::routing::get(list_issue_comments).post(create_comment),
        )
        .route(
            "/repos/{owner}/{repo}/issues/comments/{id}",
            axum::routing::patch(patch_comment),
        )
        .route(
            "/repos/{owner}/{repo}/issues/7/labels",
            axum::routing::post(add_labels),
        )
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    fake.comments
        .lock()
        .unwrap()
        .push("/xh 重点看符号运算".to_owned());

    let store = Arc::new(
        JsonlSessionStore::new(dir.join("sessions"))
            .unwrap()
            .for_runtime(),
    );
    let leases = Arc::new(FileLeaseManager::new(dir.join("leases")).unwrap());
    let runtime = Arc::new(DurableLoopAgentRuntime::new(
        "mock",
        "instant",
        Some(Arc::new(InstantReviewProvider)),
        Arc::new(GhToolFactory::default()),
        Arc::new(ToolResultPruningContextPolicy::default()),
        store,
        leases,
        64,
    ));
    let state = GhHostState {
        runtime,
        route: ModelRoute::new("mock", "instant"),
        results: Default::default(),
        gh: Some(GhToolConfig {
            client: Arc::new(GhClient::new("t").with_api_root(format!("http://{addr}"))),
            repo: "acme/x".to_owned(),
            origin: Some(origin.to_string_lossy().to_string()),
        }),
        workspace_root: dir.join("workspaces"),
        task_secret: None,
        seen_comments: Default::default(),
    };

    let dispatched = poll_once(&state).await.expect("poll");
    assert_eq!(dispatched, 1);
    let session_id = "gh-acme--x-7";
    let workspace = dir.join("workspaces").join(session_id);
    let calc = std::fs::read_to_string(workspace.join("src/calc.rs")).unwrap();
    assert!(calc.contains("a - b"), "应检出 head 版本: {calc}");
    let patch = std::fs::read_to_string(workspace.join("diff.patch")).unwrap();
    assert!(
        patch.contains("a + b") && patch.contains("a - b"),
        "diff 应含双向改动"
    );
    let task_json = std::fs::read_to_string(workspace.join("task.json")).unwrap();
    assert!(
        task_json.contains(&fake.head_sha.to_string()),
        "task.json 记录 head sha(复现)"
    );

    let mut done = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if state
            .results
            .lock()
            .unwrap()
            .get(session_id)
            .is_some_and(|outcome| outcome.status == "done")
        {
            done = true;
            break;
        }
    }
    assert!(done, "review turn 应完成");
    let posted = fake.comments.lock().unwrap().join("\n---\n");
    assert!(
        posted.contains("xh-gh Review · PR #7"),
        "结果评论应带 review 头: {posted}"
    );
    assert!(posted.contains("## P0"), "结果应含 P0 分级");
    assert!(posted.contains("src/calc.rs:4"), "finding 应带行号引用");
    assert!(posted.contains("复现信息"), "应含复现块");
    assert!(
        posted.contains(&fake.head_sha.to_string()[..12]),
        "复现块应含 head sha"
    );
    let labels = fake.labels.lock().unwrap().join(",");
    assert!(labels.contains("xh:reviewed"), "labels: {labels}");
    assert!(labels.contains("xh:review-p0"), "P0 应打标: {labels}");
}
