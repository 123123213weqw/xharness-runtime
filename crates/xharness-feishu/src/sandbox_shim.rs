//! 在 workspace 沙箱之上再遮一层：隐藏宿主的家目录与常见凭据位置。
//!
//! 背景：`xharness-sandbox` 的 `base_arguments()` 用 `--ro-bind / /` 把整个宿主
//! 文件系统只读挂进来。这保证了工具可用（编译器、解释器、/usr/share 都在），
//! 但也意味着沙箱内的 agent 能读到 `~/.ssh`、`~/bots/.env` 这类东西 ——
//! 而 .env 里躺着飞书 Secret、模型 key 和一个带 admin:org 的 GitHub token。
//!
//! 那个 crate 在上游闭包内，每次同步都会被覆盖，所以不能直接改它。这里的做法
//! 是**包装 bwrap 自己**：在它输出的参数末尾追加若干 `--tmpfs <dir>`，
//! 用空目录覆盖掉敏感路径。挂载顺序决定了后加的覆盖先加的，path 就此不可达。
//!
//! 为什么用 tmpfs 而不是 `--ro-bind /dev/null`：tmpfs 给的是空目录，
//! 读目录、`ls`、`find` 都不会报奇怪的错误，只是看不到内容。

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// 应当在沙箱内不可见的路径（相对 `$HOME`）。
///
/// 这里列的是"放进去就等于泄密"的位置，不是全部隐藏 —— 隐藏整个 `$HOME`
/// 会让 agent 连自己的工作数据都找不到，且 workspace 若在 `$HOME` 下会一起消失。
const HIDDEN_UNDER_HOME: &[&str] = &[
    ".ssh",
    ".aws",
    ".config",
    ".gnupg",
    ".docker",
    ".kube",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    "bots",         // 飞书 bot：.env 里有全套凭据
    "score-api-ts", // 另一个 .env：教学项目 key + 数据库 URL
    "fs-state",     // 自己的会话/工作区仍要可见？见下：单独挂
];

/// 即使不在 `$HOME` 下也要遮的路径。
const HIDDEN_ABSOLUTE: &[&str] = &["/etc/shadow", "/etc/sudoers", "/root"];

/// 生成遮挡参数。`workspace` 会被重新挂回去，所以删掉与它冲突的项。
pub fn masking_arguments(home: Option<&Path>, workspace: &Path) -> Vec<OsString> {
    masking_arguments_with(home, workspace, |path| path.exists(), |path| path.is_file())
}

/// 允许注入"是否存在"的判断，测试里不必真的造出这些目录。
pub fn masking_arguments_with(
    home: Option<&Path>,
    workspace: &Path,
    exists: impl Fn(&Path) -> bool,
    is_file: impl Fn(&Path) -> bool,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    let mut mask = |path: &Path| {
        // workspace 必须在沙箱里可见可写，不能被遮掉。
        if workspace.starts_with(path) || path == workspace {
            return;
        }
        // 只遮真实存在的路径：对不存在的目录用 --tmpfs，bwrap 会尝试在只读的
        // 父目录下 mkdir，直接以 "Read-only file system" 失败，整个命令跑不起来。
        if !exists(path) {
            return;
        }
        // 文件和目录要用不同手段：--tmpfs 挂的是目录，挂在文件上会报
        // "Not a directory"（.npmrc、.netrc 这类就是文件）。
        // 文件用空文件覆盖，读出来是空，但路径仍存在，不会让工具报奇怪的错。
        if is_file(path) {
            args.push(OsString::from("--ro-bind"));
            args.push(OsString::from("/dev/null"));
            args.push(path.as_os_str().to_owned());
        } else {
            args.push(OsString::from("--tmpfs"));
            args.push(path.as_os_str().to_owned());
        }
    };

    if let Some(home) = home {
        for name in HIDDEN_UNDER_HOME {
            mask(&home.join(name));
        }
    }
    for path in HIDDEN_ABSOLUTE {
        mask(Path::new(path));
    }
    args
}

/// 供测试断言：给定 HOME 与 workspace，按真实文件系统判断产出哪些 tmpfs。
pub fn masked_paths(home: Option<&Path>, workspace: &Path) -> Vec<PathBuf> {
    masked_paths_with(home, workspace, |path| path.exists(), |path| path.is_file())
}

/// 允许注入"是否存在"的判断，测试里不必真的造出这些目录。
pub fn masked_paths_with(
    home: Option<&Path>,
    workspace: &Path,
    exists: impl Fn(&Path) -> bool,
    is_file: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let args = masking_arguments_with(home, workspace, exists, is_file);
    args.chunks(2)
        .filter(|pair| pair.len() == 2 && pair[0] == OsStr::new("--tmpfs"))
        .map(|pair| PathBuf::from(&pair[1]))
        .collect()
}
