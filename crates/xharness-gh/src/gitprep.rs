//! Review 工作区准备:clone 仓库 → fetch PR head → checkout → 生成
//! diff.patch / diff.stat。全部通过 tokio::process 调 git,失败显式报错。

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 内联 diff 的上限;超出部分只保留在 diff.patch 里由 agent 自行读取。
pub const MAX_INLINE_DIFF_CHARS: usize = 24_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrContext {
    pub number: u64,
    pub title: String,
    pub head_sha: String,
    pub base_sha: String,
    pub head_ref: String,
    pub base_ref: String,
    /// `git diff --stat`(merge-base..head)。
    pub diff_stat: String,
    /// workspace 相对路径,完整 diff。
    pub diff_path: String,
    /// 内联 diff 是否被截断。
    pub diff_truncated: bool,
    /// 内联 diff 前缀(与 diff_path 内容一致,可能截断)。
    pub diff_inline: String,
    /// 宿主预跑静态检查的输出(ground truth);None = 未跑/不适用。
    #[serde(default)]
    pub checks_inline: Option<String>,
    /// 完整检查输出在 workspace 的路径。
    #[serde(default)]
    pub checks_path: Option<String>,
}

#[derive(Debug)]
pub struct PrepOutcome {
    pub context: PrContext,
}

async fn git(workspace: &Path, args: &[&str]) -> Result<String, String> {
    let output = tokio::process::Command::new("git")
        .current_dir(workspace)
        .args(args)
        .output()
        .await
        .map_err(|error| {
            format!(
                "git {} 启动失败: {error}",
                args.first().copied().unwrap_or("")
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "git {} 失败: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub struct PullSpec<'a> {
    pub number: u64,
    pub head_sha: &'a str,
    pub base_sha: &'a str,
    pub head_ref: &'a str,
    pub base_ref: &'a str,
    pub title: &'a str,
}

/// 在 workspace 准备 PR head 检出与 diff。origin 可以是 https URL 或本地
/// 路径(测试 fixture)。workspace 必须尚不存在或为空目录。
pub async fn prepare_review_workspace(
    origin: &str,
    workspace: &Path,
    pull: PullSpec<'_>,
) -> Result<PrepOutcome, String> {
    std::fs::create_dir_all(workspace).map_err(|error| error.to_string())?;
    git(workspace, &["init", "-q"]).await?;
    git(workspace, &["remote", "add", "origin", origin]).await?;
    // blob:none:历史与树完整,diff 所需 blob 按需取,小仓库同样适用。
    git(
        workspace,
        &["fetch", "-q", "--filter=blob:none", "origin", pull.head_sha],
    )
    .await?;
    git(
        workspace,
        &["fetch", "-q", "--filter=blob:none", "origin", pull.base_sha],
    )
    .await?;
    git(workspace, &["checkout", "-q", "--detach", pull.head_sha]).await?;
    let merge_base = git(workspace, &["merge-base", pull.base_sha, pull.head_sha]).await?;
    let stat = git(workspace, &["diff", "--stat", &merge_base, pull.head_sha]).await?;
    let diff = git(workspace, &["diff", &merge_base, pull.head_sha]).await?;
    let diff_path = "diff.patch";
    std::fs::write(workspace.join(diff_path), &diff).map_err(|error| error.to_string())?;
    let (diff_inline, diff_truncated) = if diff.chars().count() > MAX_INLINE_DIFF_CHARS {
        (
            diff.chars().take(MAX_INLINE_DIFF_CHARS).collect::<String>(),
            true,
        )
    } else {
        (diff.clone(), false)
    };
    Ok(PrepOutcome {
        context: PrContext {
            number: pull.number,
            title: pull.title.to_owned(),
            head_sha: pull.head_sha.to_owned(),
            base_sha: pull.base_sha.to_owned(),
            head_ref: pull.head_ref.to_owned(),
            base_ref: pull.base_ref.to_owned(),
            diff_stat: stat,
            diff_path: diff_path.to_owned(),
            diff_truncated,
            diff_inline,
            checks_inline: None,
            checks_path: None,
        },
    })
}

/// 仓库感知的快速静态检查(Rust 项目:cargo check + clippy;其他生态
/// 暂跳过)。这是本地 loop 的独有优势:评审评论发出前,宿主先跑过构建。
/// 超时 240s;完整输出写 checks.log,内联截断到 8000 字符。
pub async fn run_quick_checks(workspace: &Path) -> Option<(String, String)> {
    if !workspace.join("Cargo.toml").exists() {
        return None;
    }
    let mut combined = String::new();
    for (label, args) in [
        (
            "cargo check --all-targets",
            vec!["check", "--all-targets", "--message-format=short"],
        ),
        (
            "cargo clippy --all-targets",
            vec!["clippy", "--all-targets", "--message-format=short"],
        ),
    ] {
        combined.push_str(&format!("\n$ {label}\n"));
        match tokio::time::timeout(
            Duration::from_secs(240),
            tokio::process::Command::new("cargo")
                .current_dir(workspace)
                .args(&args)
                .output(),
        )
        .await
        {
            Ok(Ok(output)) => {
                combined.push_str(&String::from_utf8_lossy(&output.stdout));
                let stderr = String::from_utf8_lossy(&output.stderr);
                // short 模式把诊断写 stderr;错误才有价值。
                for line in stderr.lines() {
                    if line.contains("error") || line.contains("warning:") {
                        combined.push_str(line);
                        combined.push('\n');
                    }
                }
                combined.push_str(&format!("(exit {})\n", output.status.code().unwrap_or(-1)));
            }
            Ok(Err(_)) => combined.push_str("(cargo 启动失败)\n"),
            Err(_) => combined.push_str("(超时 240s,跳过)\n"),
        }
    }
    let _ = std::fs::write(workspace.join("checks.log"), &combined);
    let inline: String = combined.chars().take(8_000).collect();
    Some((inline, "checks.log".to_owned()))
}
