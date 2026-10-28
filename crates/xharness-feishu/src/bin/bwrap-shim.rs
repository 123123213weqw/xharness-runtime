//! bwrap shim：在 `xharness-sandbox` 的 `--ro-bind / /` 之上，
//! 用 `--tmpfs` 覆盖掉宿主的凭据目录。
//!
//! 为什么需要它见 `xharness_feishu::sandbox_shim` 的文档：那个 crate 在上游
//! 闭包内，改不动；这里通过 `NativeSandbox::with_bwrap_program()` 把自己冒充成
//! bwrap，追加遮挡参数后再 exec 真正的 bwrap。多一层 exec，换来升级零冲突。
//!
//! 环境变量：
//!   XH_BWRAP_REAL       真正的 bwrap 路径（默认 /usr/bin/bwrap）
//!   XH_BWRAP_WORKSPACE  当前 workspace，避免被误遮

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Stdio;

use xharness_feishu::sandbox_shim::masking_arguments;

#[tokio::main]
async fn main() {
    let real = std::env::var("XH_BWRAP_REAL").unwrap_or_else(|_| "/usr/bin/bwrap".to_owned());
    let workspace = std::env::var("XH_BWRAP_WORKSPACE").unwrap_or_default();
    let home = std::env::var("HOME").ok();

    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mask = masking_arguments(
        home.as_deref().map(Path::new),
        Path::new(if workspace.is_empty() {
            "/nonexistent"
        } else {
            &workspace
        }),
    );

    // 注入沙箱内的代理环境。出口只有 127.0.0.1:<whitelist port> 一个，
    // 由 iptables 按 UID 放行；这里让 curl/git/pip 默认就走它。
    let mut extra = mask;
    if let Some(proxy) = std::env::var("XH_SANDBOX_PROXY")
        .ok()
        .filter(|value| !value.is_empty())
    {
        for key in ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"] {
            extra.push(OsString::from("--setenv"));
            extra.push(OsString::from(key));
            extra.push(OsString::from(proxy.clone()));
        }
        extra.push(OsString::from("--setenv"));
        extra.push(OsString::from("no_proxy"));
        extra.push(OsString::from("localhost,127.0.0.1"));
    }
    // sudo 会做 env_reset，服务级 Environment= 里的 PIP_* / GIT_* 到不了沙箱。
    // 这里按白名单逐个转发；绝不用 sudo -E —— 那会把 XH_FS_API_KEY 一起带进去。
    for key in [
        "PIP_INDEX_URL",
        "PIP_TRUSTED_HOST",
        "PIP_CACHE_DIR",
        "GIT_TERMINAL_PROMPT",
    ] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                extra.push(OsString::from("--setenv"));
                extra.push(OsString::from(key));
                extra.push(OsString::from(value));
            }
        }
    }
    if let Some(home) = home.as_deref() {
        // sudo 的 env_reset 会把 HOME 换成 /root，这里改回去，
        // 否则工具会去 /root 下找配置，行为与遮罩计算时的假设不一致。
        extra.push(OsString::from("--setenv"));
        extra.push(OsString::from("HOME"));
        extra.push(OsString::from(home));
    }

    // 遮挡与 setenv 必须落在 `--` 之前，否则会被当成目标程序的参数。
    match args.iter().position(|a| a == OsStr::new("--")) {
        Some(at) => {
            for (offset, arg) in extra.into_iter().enumerate() {
                args.insert(at + offset, arg);
            }
        }
        None => args.extend(extra),
    }

    // 降权：沙箱里的进程必须是 uid 1007（xhsbx），无 sudo、出站受 iptables 限制。
    // 直接 spawn bwrap 只会得到 wzu，iptables 的 uid 匹配就形同虚设。
    let mut command = tokio::process::Command::new("/usr/bin/sudo");
    command
        .arg("-n")
        .arg("--")
        .arg("/usr/bin/setpriv")
        .arg("--reuid=1007")
        .arg("--regid=995")
        .arg("--clear-groups")
        .arg(&real)
        .args(&args);

    match command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
    {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("sandbox shim: 无法执行 {real}: {error}");
            std::process::exit(127);
        }
    }
}
