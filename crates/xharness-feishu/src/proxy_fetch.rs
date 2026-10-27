//! 带代理降级的 `web_fetch`。
//!
//! 背景：`xharness-web` 的 `fetch_client()` 硬编码 `.no_proxy()`，注释写明理由——
//! 解析出的 IP 就是 SSRF 边界，走前向代理会让代理自己解析域名、绕过这个钉住。
//! 结果是在被墙网络里 github.com 直接超时（TCP 能建连、TLS 握手被丢），
//! 而 api.github.com 恰好能用，表现为“有时能抓有时不能”。
//!
//! 那个 crate 在上游闭包内，改了会被同步覆盖，所以在这里覆盖 `web_fetch` 工具：
//!
//! 1. **先直连**，与上游行为完全一致。
//! 2. 仅当直连是**传输层失败**（超时/连接错误）且配置了代理时，才用代理重试。
//! 3. 两条路径都先本地解析并校验是公网地址，`is_public_ip` 复用上游实现。
//!
//! 必须说清的残余风险：走代理时是代理去解析域名的，我们这一步的校验发生在
//! 代理之前，所以 DNS rebinding（先返回公网、代理再解析到内网）理论上可行。
//! 缓解办法是只把可信代理写进配置——`XH_FS_HTTP_PROXY` 是显式配置，不是从
//! 环境里捡来的 ambient proxy。

use std::net::IpAddr;
use std::time::Duration;

use serde_json::json;
use xharness_tools::{
    ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolHandlerError, ToolOutput, ToolSpec,
};

/// 可接受的响应类型。
///
/// 比上游多了 `application/json`：实测经节点抓 github.com 的 HTML 只有约 26 KB/s
/// （365KB 要 14–30 秒），而 `api.github.com` 的 JSON 是 0.7 秒、体积小一个数量级、
/// 还是结构化数据。禁掉 JSON 等于逼 agent 走最慢的那条路。
///
/// 仍然排除图片/二进制/压缩包：那些没有文本表示，塞进上下文只会浪费 token。
const ALLOWED_TYPES: &[&str] = &[
    "text/html",
    "text/plain",
    "text/markdown",
    "text/x-markdown",
    "application/json",
    "application/x-ndjson",
];

/// 原文上限：超过就停止读取，避免把整个 5MB 页面拉进内存。
const MAX_BYTES: usize = 2 * 1024 * 1024;

/// 交给模型的正文上限。
///
/// 这个数字是踩出来的：第一版用 512KB，抓 github 仓库页返回 393KB 的裸 HTML，
/// 直接把上下文撑爆，turn 以 `status: stopped` 结束 —— 模型一句话都没来得及说。
/// 上游 `WebConfig::DEFAULT_MAX_TEXT_CHARS` 是 8000，按它对齐。
pub const MAX_TEXT_CHARS: usize = 8_000;

/// JSON 上限。结构化数据信息密度高，给宽一些。
pub const MAX_JSON_CHARS: usize = 24_000;

/// HTML 去掉脚本/样式后成文，否则 8000 字符里大半是 `function(e){...}`。
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 4);
    let mut rest = html;
    let mut skip_tag: Option<String> = None;
    while let Some(lt) = rest.find('<') {
        let (before, tail) = rest.split_at(lt);
        if skip_tag.is_none() && !before.trim().is_empty() {
            out.push_str(before);
            out.push(' ');
        }
        let Some(gt) = tail.find('>') else { break };
        let tag = tail[1..gt].trim().to_ascii_lowercase();
        let name: String = tag
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_start_matches('/')
            .to_owned();
        if skip_tag.is_none() && matches!(name.as_str(), "script" | "style" | "svg" | "noscript") {
            skip_tag = Some(name);
        } else if let Some(open) = skip_tag.as_deref() {
            if tag.starts_with('/') && name == open {
                skip_tag = None;
            }
        }
        rest = &tail[gt + 1..];
    }
    if skip_tag.is_none() {
        out.push_str(rest);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 从环境读代理；空串视为未配置。
pub fn proxy_from_env() -> Option<String> {
    ["XH_FS_HTTP_PROXY", "XH_FS_HTTPS_PROXY"]
        .iter()
        .find_map(|key| std::env::var(key).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// 构造覆盖版 `web_fetch` 工具。
pub fn web_fetch_spec(proxy: Option<String>) -> ToolSpec {
    ToolSpec::new(
        ToolDefinition::new(
            "web_fetch",
            "抓取一个公网网页并返回纯文本。直连失败且配置了代理时会经代理重试。",
            json!({
                "type": "object",
                "properties": { "url": { "type": "string" } },
                "required": ["url"],
                "additionalProperties": false
            }),
        ),
        move |context: ToolExecutionContext| {
            let proxy = proxy.clone();
            async move {
                let url = context
                    .arguments
                    .get("url")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolHandlerError::new("missing url"))?
                    .to_owned();
                let direct = fetch_once(&url, None).await;
                let outcome = match (direct, proxy) {
                    (Ok(text), _) => Ok((text, "direct")),
                    (Err(error), Some(proxy)) if error.retryable => {
                        match fetch_once(&url, Some(&proxy)).await {
                            Ok(text) => Ok((text, "proxy")),
                            Err(retry_error) => Err(format!(
                                "直连失败({})，经代理也失败({})",
                                error.message, retry_error.message
                            )),
                        }
                    }
                    (Err(error), _) => Err(error.message),
                };
                match outcome {
                    Ok((text, via)) => Ok(ToolOutput::text(format!("[via {via}]\n{text}"))),
                    Err(message) => Err(ToolHandlerError::new(message)),
                }
            }
        },
    )
    .with_concurrency(ToolConcurrency::Parallel)
}

struct FetchError {
    message: String,
    /// 传输层失败才值得换代理重试；4xx/内容类型不对换代理也没用。
    retryable: bool,
}

impl FetchError {
    fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }

    fn transport(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }
}

async fn fetch_once(raw: &str, proxy: Option<&str>) -> Result<String, FetchError> {
    let url =
        reqwest::Url::parse(raw).map_err(|e| FetchError::fatal(format!("URL 无法解析: {e}")))?;
    if url.scheme() != "https" && url.scheme() != "http" {
        return Err(FetchError::fatal("只支持 http/https"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| FetchError::fatal("URL 缺少 host"))?
        .to_owned();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| FetchError::fatal("URL 缺少端口"))?;

    // SSRF 校验在**两条路径之前**都做：拒绝解析到内网的域名。
    if host
        .parse::<IpAddr>()
        .map(|ip| !xharness_web::is_public_ip(ip))
        .unwrap_or(false)
    {
        return Err(FetchError::fatal("目标是内网地址，拒绝访问"));
    }
    if host.parse::<IpAddr>().is_err() {
        let resolved: Vec<IpAddr> = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|e| FetchError::transport(format!("域名解析失败: {e}")))?
            .map(|addr| addr.ip())
            .collect();
        if resolved.is_empty() {
            return Err(FetchError::transport("域名没有解析到任何地址"));
        }
        if resolved.iter().any(|ip| !xharness_web::is_public_ip(*ip)) {
            return Err(FetchError::fatal("域名解析到内网地址，拒绝访问"));
        }
    }

    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(45))
        .user_agent("xharness-feishu/0.1");
    builder = match proxy {
        // 显式配置的代理：它是信任边界的一部分，见模块注释。
        Some(proxy) => builder.proxy(
            reqwest::Proxy::all(proxy)
                .map_err(|e| FetchError::fatal(format!("代理地址无效: {e}")))?,
        ),
        None => builder.no_proxy(),
    };
    let client = builder
        .build()
        .map_err(|e| FetchError::fatal(format!("client 构建失败: {e}")))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| FetchError::transport(format!("请求失败: {e}")))?;
    if !response.status().is_success() {
        return Err(FetchError::fatal(format!(
            "HTTP {}",
            response.status().as_u16()
        )));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !ALLOWED_TYPES.iter().any(|t| content_type == *t) {
        return Err(FetchError::fatal(format!(
            "不支持的内容类型 {content_type:?}；只接受 text/html 与 text/plain"
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| FetchError::transport(format!("读取响应失败: {e}")))?;
    if bytes.len() > MAX_BYTES {
        return Err(FetchError::fatal(format!(
            "响应超过 {}MB 上限，已放弃",
            MAX_BYTES / (1024 * 1024)
        )));
    }
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let text = if content_type.contains("html") {
        html_to_text(&raw)
    } else {
        raw
    };
    // JSON 是结构化数据，8000 字符常常不够看一个仓库详情；HTML 转文本后 8000
    // 已经能覆盖主要内容，再多只是噪音。
    let limit = if content_type.contains("json") {
        MAX_JSON_CHARS
    } else {
        MAX_TEXT_CHARS
    };
    let total = text.chars().count();
    let clipped: String = text.chars().take(limit).collect();
    Ok(if total > limit {
        format!("{clipped}\n\n[已截断：{limit}/{total} 字符]")
    } else {
        clipped
    })
}
