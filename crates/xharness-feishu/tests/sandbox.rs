//! 验证 WorkspaceWrite 沙箱真的生效，而不只是"配置了一个枚举"。
//!
//! 这类测试的价值在于:沙箱配置写错了、bwrap 探测失败退化成无沙箱、
//! 或者平台层悄悄改了默认值,都不会让编译或其它测试失败,只会让隔离
//! 静默消失。所以必须真的去写宿主文件并断言它失败。

use std::sync::atomic::{AtomicUsize, Ordering};

use xharness_feishu::FeishuToolFactory;
use xharness_host::{PermissionPreset, SessionToolFactory};
use xharness_tools::ToolRequest;

static SEQ: AtomicUsize = AtomicUsize::new(0);

/// 真正驱动沙箱的测试需要 bwrap；没有它时**必须跳过，而不是失败**。
///
/// 这条是 CI 教出来的：GitHub runner 没装 bubblewrap，测试直接以
/// "native sandbox is unavailable: cannot find bwrap in PATH" 失败，把上游同步
/// 整条卡住了（sync-upstream 会跑全 workspace 测试）。
///
/// 更麻烦的是假通过：`network_is_denied_by_default` 在没有 bwrap 时也"通过"了，
/// 因为沙箱根本起不来、命令必然失败 —— 它测的其实是"失败了"，不是"网络被拒绝"。
/// 所以两个测试都必须在 bwrap 缺席时显式跳过。
fn bwrap_available() -> bool {
    std::process::Command::new("bwrap")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 跳过时的统一说明，避免"静默通过"看起来像验过了。
macro_rules! require_bwrap {
    () => {
        if !bwrap_available() {
            eprintln!("跳过：本机没有 bwrap，沙箱相关断言无法验证");
            return;
        }
    };
}

fn fresh_workspace(tag: &str) -> std::path::PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("xh-fs-sbx-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("workspace");
    dir
}

async fn executor_for(
    cwd: &std::path::Path,
    permission: PermissionPreset,
) -> xharness_tools::ToolExecutor {
    FeishuToolFactory::default()
        .executor("fs-sbx", &cwd.to_string_lossy(), permission)
        .await
        .expect("executor")
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_write_is_confined_to_the_workspace() {
    require_bwrap!();
    let cwd = fresh_workspace("confined");
    let exec = executor_for(&cwd, PermissionPreset::WorkspaceWrite).await;

    // write 工具只接受 workspace 内的相对路径（绝对路径会被判 invalid）。
    let inside = cwd.join("allowed.txt");
    let request = ToolRequest::new(
        "write",
        serde_json::json!({ "path": "allowed.txt", "content": "ok" }).to_string(),
    );
    let result = exec.execute(request).await;
    assert!(result.is_ok(), "workspace 内写入应当成功: {:?}", result);
    assert!(inside.exists(), "文件应当真的被创建");

    // 这里**故意不做**"写宿主"的断言。
    //
    // 试过两种逃逸方式，都不能证明沙箱生效：
    //   1. `..` 穿越 / 绝对路径 —— xharness-fs 词法检查就拒绝了；
    //   2. 符号链接指向 $HOME —— 同样被 xharness-fs 拒绝（OS 层确认会跟随，
    //      说明是工具层拦的）。
    // 两类断言在关掉沙箱后依然通过，属于假阳性。真正能区分沙箱的是下面
    // 的 bash 测试和网络测试（关掉沙箱后它们会失败，已验证）。
    let _ = std::fs::remove_dir_all(&cwd);
}

#[tokio::test(flavor = "multi_thread")]
async fn bash_cannot_write_outside_workspace_under_sandbox() {
    require_bwrap!();
    let cwd = fresh_workspace("bash");
    let exec = executor_for(&cwd, PermissionPreset::WorkspaceWrite).await;

    // shell 工具名随平台不同(bash/zsh/powershell),取注册表里的真实名字。
    let names = registered_tool_names(&cwd).await;
    let shell = names
        .iter()
        .find(|n| matches!(n.as_str(), "bash" | "zsh" | "sh" | "powershell"))
        .cloned();
    let Some(shell) = shell else {
        eprintln!("当前平台没有 shell 工具,跳过");
        return;
    };

    // workspace 内可写
    let ok = exec
        .execute(ToolRequest::new(
            &shell,
            serde_json::json!({ "command": "echo hi > inside.txt && cat inside.txt" }).to_string(),
        ))
        .await;
    assert!(ok.is_ok(), "workspace 内 shell 执行应当成功: {:?}", ok);

    // shell 直接写宿主只读区。bwrap 必须拦住。
    // 同样必须选宿主可写的位置；$HOME 才是有意义的靶子。
    let home = std::env::var("HOME").expect("HOME");
    let target_str = format!("{home}/fs-sbx-bash.txt");
    let host_target = std::path::Path::new(&target_str);
    let _ = std::fs::remove_file(host_target);
    let escape = exec
        .execute(ToolRequest::new(
            &shell,
            serde_json::json!({
                "command": format!("echo pwned > {target_str} 2>/dev/null; test -f {target_str} && echo WROTE || echo BLOCKED")
            })
            .to_string(),
        ))
        .await;
    // 以宿主文件是否存在为唯一判据：shell 的退出码不可靠（重定向失败也会吞掉）。
    assert!(
        !host_target.exists(),
        "沙箱必须阻止 shell 写宿主只读区: {:?}",
        escape
    );
    let _ = std::fs::remove_file(host_target);

    let _ = std::fs::remove_dir_all(&cwd);
}

#[tokio::test(flavor = "multi_thread")]
async fn network_is_denied_by_default() {
    require_bwrap!();
    let cwd = fresh_workspace("net");
    let exec = executor_for(&cwd, PermissionPreset::WorkspaceWrite).await;
    let names = registered_tool_names(&cwd).await;
    let shell = names
        .iter()
        .find(|n| matches!(n.as_str(), "bash" | "zsh" | "sh" | "powershell"))
        .cloned();
    let Some(shell) = shell else { return };

    // 沙箱默认 NetworkAccess::Deny,即使宿主能上网,沙箱内也应当失败。
    let probe = exec
        .execute(ToolRequest::new(
            &shell,
            serde_json::json!({ "command": "curl -s --max-time 3 -o /dev/null -w '%{http_code}' https://example.com || echo BLOCKED" }).to_string(),
        ))
        .await;
    let text = format!("{probe:?}");
    assert!(
        !text.contains("200"),
        "默认应当禁止出网,却拿到了 200: {text}"
    );

    let _ = std::fs::remove_dir_all(&cwd);
}

/// ToolExecutor 不暴露 specs，所以按 factory 的同一路径重建一个 bundle 只为读名字。
async fn registered_tool_names(cwd: &std::path::Path) -> Vec<String> {
    use std::sync::Arc;
    use xharness_coding_tools::CodingToolBundle;
    use xharness_jobs::{JobRegistry, JobRegistryConfig};
    use xharness_platform::{NativePlatform, PlatformConfig};
    use xharness_web::WebRuntime;

    let platform =
        Arc::new(NativePlatform::new(PlatformConfig::new(cwd).full_access()).expect("platform"));
    let bundle = CodingToolBundle::new(
        platform,
        Arc::new(JobRegistry::new(JobRegistryConfig::default())),
        Arc::new(WebRuntime::default()),
        "probe",
        "probe",
    );
    bundle
        .specs()
        .iter()
        .map(|s| s.definition.name.clone())
        .collect()
}

// ---- 挂载遮挡：验证明明是"沙箱里读不到"，而不是"该文件本来就不存在" ----

#[test]
fn secrets_under_home_are_masked_but_workspace_is_not() {
    use xharness_feishu::sandbox_shim::masked_paths_with;

    let home = std::path::Path::new("/home/wzu");
    let workspace = std::path::Path::new("/home/wzu/fs-state/workspaces/fs-oc_x");
    // 注入全部存在，验证列表完整性（真实环境只遮存在的见下一个测试）
    let masked = masked_paths_with(Some(home), workspace, |_| true, |_| false);

    // 必须遮的关键位置
    for must in [
        "/home/wzu/bots",
        "/home/wzu/.ssh",
        "/home/wzu/.aws",
        "/etc/shadow",
    ] {
        assert!(
            masked.iter().any(|p| p == std::path::Path::new(must)),
            "{must} 应当被遮挡，实际: {masked:?}"
        );
    }

    // workspace 自己不能被遮，否则 agent 无处可写
    assert!(
        !masked.iter().any(|p| p == workspace),
        "workspace 被误遮: {masked:?}"
    );
}

#[test]
fn a_workspace_inside_a_masked_dir_removes_that_mask() {
    use xharness_feishu::sandbox_shim::masked_paths_with;

    // workspace 若正好在 fs-state 下，不能把 fs-state 整个遮掉
    let home = std::path::Path::new("/home/wzu");
    let workspace = std::path::Path::new("/home/wzu/fs-state/workspaces/fs-x");
    // 注入全部存在，验证列表完整性（真实环境只遮存在的见下一个测试）
    let masked = masked_paths_with(Some(home), workspace, |_| true, |_| false);
    assert!(
        !masked
            .iter()
            .any(|p| p == std::path::Path::new("/home/wzu/fs-state")),
        "祖先目录被遮会让 workspace 一起消失: {masked:?}"
    );
}

#[test]
fn masking_works_without_a_home_directory() {
    use xharness_feishu::sandbox_shim::masked_paths_with;
    let masked = masked_paths_with(None, std::path::Path::new("/tmp/ws"), |_| true, |_| false);
    // 没有 HOME 时仍要遮绝对路径
    assert!(masked
        .iter()
        .any(|p| p == std::path::Path::new("/etc/shadow")));
    assert!(!masked.iter().any(|p| p.starts_with("/home")));
}

#[test]
fn nonexistent_paths_are_not_masked() {
    use xharness_feishu::sandbox_shim::masking_arguments_with;

    // 实测过：对不存在的目录用 --tmpfs，bwrap 会在只读父目录下 mkdir 并以
    // "Read-only file system" 失败，整个沙箱命令都跑不起来。
    let home = std::path::Path::new("/home/wzu");
    let ws = std::path::Path::new("/tmp/ws");
    let args = masking_arguments_with(
        Some(home),
        ws,
        |p| p == std::path::Path::new("/home/wzu/bots"),
        |_| false,
    );
    let rendered: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(
        rendered.contains(&"/home/wzu/bots".to_owned()),
        "{rendered:?}"
    );
    assert!(
        !rendered.contains(&"/home/wzu/.aws".to_owned()),
        "不存在的路径不应当被遮挡: {rendered:?}"
    );
}

#[test]
fn files_are_masked_with_dev_null_not_tmpfs() {
    use xharness_feishu::sandbox_shim::masking_arguments_with;

    // .npmrc 是文件：用 --tmpfs 挂在文件上会报 "Not a directory"，整条 bwrap 失败。
    let home = std::path::Path::new("/home/wzu");
    let ws = std::path::Path::new("/tmp/ws");
    let args = masking_arguments_with(
        Some(home),
        ws,
        |p| p == std::path::Path::new("/home/wzu/.npmrc"),
        |p| p == std::path::Path::new("/home/wzu/.npmrc"),
    );
    let rendered: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(rendered.contains(&"--ro-bind".to_owned()), "{rendered:?}");
    assert!(rendered.contains(&"/dev/null".to_owned()), "{rendered:?}");
    assert!(!rendered.contains(&"--tmpfs".to_owned()), "{rendered:?}");
}
