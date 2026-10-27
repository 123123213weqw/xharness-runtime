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

    // 遮挡必须落在 `--` 之前，否则会被当成目标程序的参数。
    match args.iter().position(|a| a == OsStr::new("--")) {
        Some(at) => {
            for (offset, arg) in mask.into_iter().enumerate() {
                args.insert(at + offset, arg);
            }
        }
        None => args.extend(mask),
    }

    match tokio::process::Command::new(&real)
        .args(&args)
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
