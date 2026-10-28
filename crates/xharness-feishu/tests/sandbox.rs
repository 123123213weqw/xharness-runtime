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
fn sandbox_works() -> bool {
    // 两次被 CI 教：先是没有 bubblewrap，装上之后 GitHub runner 又不允许创建
    // netns（"bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted"）。
    // 所以不能只查 --version，必须真的起一个最小沙箱。
    let installed = std::process::Command::new("bwrap")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !installed {
        return false;
    }
    std::process::Command::new("bwrap")
        .args([
            "--die-with-parent",
            "--unshare-all",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--",
            "true",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 跳过时的统一说明，避免"静默通过"看起来像验过了。
macro_rules! require_bwrap {
    () => {
        if !sandbox_works() {
            eprintln!("跳过：bwrap 不可用（未安装或无法创建命名空间），沙箱断言无法验证");
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

/// 出站隔离只有在"装了专用 UID + iptables 规则 + 白名单代理"的机器上才验得了。
/// CI 里三样都没有，所以显式跳过，而不是让断言空转。
fn dedicated_egress_available() -> bool {
    std::process::Command::new("/usr/bin/sudo")
        .args([
            "-n",
            "--",
            "/usr/bin/setpriv",
            "--reuid=1007",
            "--regid=995",
            "--clear-groups",
            "/usr/bin/id",
            "-u",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn shell_runs_as_the_dedicated_uid_without_sudo() {
    require_bwrap!();
    if !dedicated_egress_available() {
        eprintln!("跳过：本机没有沙箱专用 UID，无法验证降权");
        return;
    }
    let cwd = fresh_workspace("uid");
    let exec = executor_for(&cwd, PermissionPreset::WorkspaceWrite).await;
    let names = registered_tool_names(&cwd).await;
    let Some(shell) = names
        .iter()
        .find(|n| matches!(n.as_str(), "bash" | "zsh" | "sh" | "powershell"))
        .cloned()
    else {
        return;
    };

    // 降权是整条网络隔离的根基：iptables 按 uid 1007 放行唯一出口。
    // 如果这里还是 wzu，规则就完全不起作用（实测踩过：规则 0 命中）。
    let out = exec
        .execute(ToolRequest::new(
            &shell,
            serde_json::json!({ "command": "id -u; id -g; sudo -n true 2>&1 || echo NO-SUDO" })
                .to_string(),
        ))
        .await;
    let text = format!("{out:?}");
    assert!(text.contains("1007"), "沙箱内 UID 应为 1007: {text}");
    assert!(text.contains("995"), "沙箱内 GID 应为 995: {text}");
    assert!(text.contains("NO-SUDO"), "沙箱内不得能提权: {text}");

    let _ = std::fs::remove_dir_all(&cwd);
}

#[tokio::test]
async fn egress_goes_only_through_the_whitelist_proxy() {
    require_bwrap!();
    if !dedicated_egress_available() {
        eprintln!("跳过：本机没有沙箱专用 UID，无法验证出站过滤");
        return;
    }
    let cwd = fresh_workspace("egress");
    let exec = executor_for(&cwd, PermissionPreset::WorkspaceWrite).await;
    let names = registered_tool_names(&cwd).await;
    let Some(shell) = names
        .iter()
        .find(|n| matches!(n.as_str(), "bash" | "zsh" | "sh" | "powershell"))
        .cloned()
    else {
        return;
    };

    // 三件事同时成立才算隔离有效：
    //   白名单域名通 / 裸代理口不通 / mihomo 控制口不通
    let script = concat!(
        "echo WL:$(curl -s -o /dev/null -w '%{http_code}' --max-time 12 https://api.github.com 2>/dev/null || echo ERR);",
        "echo OPEN-PROXY:$(curl -s -o /dev/null -w '%{http_code}' -x http://127.0.0.1:7897 --max-time 5 https://api.github.com 2>/dev/null || echo BLOCKED);",
        "echo CTRL:$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 http://127.0.0.1:9090/proxies 2>/dev/null || echo BLOCKED)"
    );
    let out = exec
        .execute(ToolRequest::new(
            &shell,
            serde_json::json!({ "command": script }).to_string(),
        ))
        .await;
    let text = format!("{out:?}");

    // 阻断是安全断言，任何时候都必须成立。
    // curl 失败时同时打印 000 和 BLOCKED，所以用"不得是 200"作为判据更精确。
    assert!(
        !text.contains("OPEN-PROXY:200"),
        "裸代理口必须被拒（拿到 200 说明 iptables 没生效）: {text}"
    );
    assert!(!text.contains("CTRL:200"), "mihomo 控制口必须被拒: {text}");

    // 可达性依赖 shim 注入的代理环境；没配代理时沙箱本就无出口，跳过这一条。
    match std::env::var("XH_SANDBOX_PROXY") {
        Ok(proxy) if !proxy.is_empty() => {
            assert!(
                text.contains("WL:200"),
                "配了代理时白名单域名应当可达: {text}"
            );
        }
        _ => eprintln!("跳过可达性断言：XH_SANDBOX_PROXY 未设置（沙箱此时本就无出口）"),
    }

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
fn files_are_masked_with_an_empty_regular_file() {
    use xharness_feishu::sandbox_shim::{empty_mask_source, masking_arguments_with};

    // .npmrc 这类是文件：用 --tmpfs 挂文件上会报 "Not a directory"。
    let home = std::path::Path::new("/home/wzu");
    let ws = std::path::Path::new("/tmp/ws");
    let npmrc = std::path::Path::new("/home/wzu/.npmrc");
    let args = masking_arguments_with(Some(home), ws, |p| p == npmrc, |p| p == npmrc);
    let rendered: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    assert!(rendered.contains(&"--ro-bind".to_owned()), "{rendered:?}");
    assert!(!rendered.contains(&"--tmpfs".to_owned()), "{rendered:?}");

    // 关键：绑定源不能是 /dev/null。
    // 实测 `--ro-bind /dev/null <file>` 之后，沙箱内以非特权 UID 打开该路径会
    // EACCES（不是读到空），git 读 .gitconfig 失败会直接 fatal，clone 全废。
    assert!(
        !rendered.contains(&"/dev/null".to_owned()),
        "不得再用 /dev/null 做文件遮罩源: {rendered:?}"
    );
    let source = empty_mask_source();
    assert!(
        rendered.contains(&source.to_string_lossy().into_owned()),
        "应当绑定空文件源: {rendered:?}"
    );
    let meta = std::fs::metadata(&source).expect("空文件源必须真实存在");
    assert!(meta.is_file(), "空文件源必须是普通文件");
    assert_eq!(meta.len(), 0, "空文件源必须为空");
}
