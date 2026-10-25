//! 最小 GitHub REST 客户端:只为 gh-host 的回写面服务。
//!
//! upsert 语义:每个 durable session 占据一个评论槽,槽位由固定 marker
//! 识别(`<!-- xh-gh:session:{id} -->`)。重复写入 = PATCH 同一条评论,
//! 中断重放天然安全。

use serde::{Deserialize, Serialize};

const API_ROOT: &str = "https://api.github.com";

pub fn session_marker(session_id: &str) -> String {
    format!("<!-- xh-gh:session:{session_id} -->")
}

#[derive(Clone)]
pub struct GhClient {
    http: reqwest::Client,
    token: String,
    root: String,
}

#[derive(Debug, Deserialize)]
pub struct Comment {
    pub id: u64,
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub struct RepoComment {
    pub id: u64,
    pub body: String,
    pub issue_url: String,
    pub user: User,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    /// 存在即表示该 issue 是 Pull Request。
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct PullRef {
    pub sha: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
}

#[derive(Debug, Deserialize)]
pub struct Pull {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub head: PullRef,
    pub base: PullRef,
}

impl RepoComment {
    pub fn issue_number(&self) -> Option<u64> {
        self.issue_url.rsplit('/').next()?.parse().ok()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum UpsertOutcome {
    Created { id: u64 },
    Updated { id: u64 },
}

#[derive(Serialize)]
struct CommentBody<'a> {
    body: &'a str,
}

impl GhClient {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent(concat!("xharness-gh/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("reqwest client"),
            token: token.into(),
            root: API_ROOT.to_owned(),
        }
    }

    /// 测试与未来 GHE 场景:覆盖 API 根地址。
    pub fn with_api_root(mut self, root: impl Into<String>) -> Self {
        self.root = root.into().trim_end_matches('/').to_owned();
        self
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.root))
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
    }

    /// 仓库级最近评论(触发器轮询用):id、body、所属 issue 号、作者。
    pub async fn list_repo_comments(&self, repo: &str) -> Result<Vec<RepoComment>, String> {
        let response = self
            .request(
                reqwest::Method::GET,
                &format!("/repos/{repo}/issues/comments?per_page=30&sort=created&direction=desc"),
            )
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("list repo comments 失败: {status}"));
        }
        response
            .json::<Vec<RepoComment>>()
            .await
            .map_err(|error| error.to_string())
    }

    /// 单个 issue 的标题/正文(触发器组装任务用)。
    pub async fn get_issue(&self, repo: &str, issue: u64) -> Result<Issue, String> {
        let response = self
            .request(
                reqwest::Method::GET,
                &format!("/repos/{repo}/issues/{issue}"),
            )
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("get issue 失败: {status}"));
        }
        response
            .json::<Issue>()
            .await
            .map_err(|error| error.to_string())
    }

    /// Pull Request 详情(head/base SHA);issue 号对非 PR 返回 404。
    pub async fn get_pull(&self, repo: &str, number: u64) -> Result<Pull, String> {
        let response = self
            .request(
                reqwest::Method::GET,
                &format!("/repos/{repo}/pulls/{number}"),
            )
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("get pull 失败: {status}"));
        }
        response
            .json::<Pull>()
            .await
            .map_err(|error| error.to_string())
    }

    async fn list_issue_comments(&self, repo: &str, issue: u64) -> Result<Vec<Comment>, String> {
        let response = self
            .request(
                reqwest::Method::GET,
                &format!("/repos/{repo}/issues/{issue}/comments?per_page=100"),
            )
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("list comments 失败: {status}"));
        }
        response
            .json::<Vec<Comment>>()
            .await
            .map_err(|error| error.to_string())
    }

    async fn create_comment(&self, repo: &str, issue: u64, body: &str) -> Result<u64, String> {
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/repos/{repo}/issues/{issue}/comments"),
            )
            .json(&CommentBody { body })
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("创建评论失败: {status}"));
        }
        let created: Comment = response.json().await.map_err(|error| error.to_string())?;
        Ok(created.id)
    }

    async fn patch_comment(&self, repo: &str, comment_id: u64, body: &str) -> Result<(), String> {
        let response = self
            .request(
                reqwest::Method::PATCH,
                &format!("/repos/{repo}/issues/comments/{comment_id}"),
            )
            .json(&CommentBody { body })
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("更新评论失败: {status}"));
        }
        Ok(())
    }

    /// 按 marker 定位会话评论槽并写入:存在则 PATCH,否则 POST。
    pub async fn upsert_session_comment(
        &self,
        repo: &str,
        issue: u64,
        marker: &str,
        body: &str,
    ) -> Result<UpsertOutcome, String> {
        let full = format!("{marker}\n\n{body}");
        for comment in self.list_issue_comments(repo, issue).await? {
            if comment.body.starts_with(marker) {
                self.patch_comment(repo, comment.id, &full).await?;
                return Ok(UpsertOutcome::Updated { id: comment.id });
            }
        }
        let id = self.create_comment(repo, issue, &full).await?;
        Ok(UpsertOutcome::Created { id })
    }

    pub async fn add_label(&self, repo: &str, issue: u64, label: &str) -> Result<(), String> {
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/repos/{repo}/issues/{issue}/labels"),
            )
            .json(&serde_json::json!({"labels": [label]}))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        if status.is_success() || status == reqwest::StatusCode::NOT_MODIFIED {
            Ok(())
        } else {
            Err(format!("打 label 失败: {status}"))
        }
    }

    pub async fn remove_label(&self, repo: &str, issue: u64, label: &str) -> Result<(), String> {
        let response = self
            .request(
                reqwest::Method::DELETE,
                &format!("/repos/{repo}/issues/{issue}/labels/{label}"),
            )
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(format!("移除 label 失败: {status}"))
        }
    }
}
