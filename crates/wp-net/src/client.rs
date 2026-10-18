//! HTTP 客户端构建与响应处理。

use serde_json::Value;
use std::time::Duration;
use wp_core::{Error, Result};

/// HTTP 客户端参数。
#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub user_agent: String,
    /// 整体超时（秒，0 = 不设）
    pub timeout_secs: u64,
    /// 连接超时（秒）
    pub connect_timeout_secs: u64,
    /// 是否自动跟随重定向
    pub follow_redirects: bool,
    /// 是否忽略系统代理（百度/夸克直连更稳；国内代理反而拖慢）
    pub no_proxy: bool,
    /// 是否启用 http2（三家适配器实测都建议 false）
    pub http2: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36".into(),
            timeout_secs: 60,
            connect_timeout_secs: 15,
            follow_redirects: true,
            no_proxy: true,
            http2: false,
        }
    }
}

impl ClientOptions {
    pub fn new(ua: impl Into<String>) -> Self {
        Self {
            user_agent: ua.into(),
            ..Default::default()
        }
    }
    pub fn timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }
    pub fn follow_redirects(mut self, yes: bool) -> Self {
        self.follow_redirects = yes;
        self
    }
    pub fn no_proxy(mut self, yes: bool) -> Self {
        self.no_proxy = yes;
        self
    }
}

/// 构建客户端；参数非法时返回错误。
pub fn build_client(opts: ClientOptions) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .user_agent(&opts.user_agent)
        .danger_accept_invalid_certs(false)
        .cookie_store(true);
    if opts.timeout_secs > 0 {
        b = b.timeout(Duration::from_secs(opts.timeout_secs));
    }
    b = b.connect_timeout(Duration::from_secs(opts.connect_timeout_secs.max(1)));
    b = if opts.follow_redirects {
        b.redirect(reqwest::redirect::Policy::limited(10))
    } else {
        b.redirect(reqwest::redirect::Policy::none())
    };
    if opts.no_proxy {
        b = b.no_proxy();
    }
    if !opts.http2 {
        b = b.http1_only();
    }
    b.build().map_err(net_err)
}

/// 用默认参数建客户端。
pub fn build_client_default() -> reqwest::Client {
    build_client(ClientOptions::default()).expect("默认 reqwest 客户端构建失败")
}

/// reqwest 错误 → 统一错误（连接/超时/解码都算网络错误，可重试）。
pub fn net_err(e: reqwest::Error) -> Error {
    if e.is_timeout() {
        Error::network(format!("请求超时：{e}"))
    } else if e.is_connect() {
        Error::network(format!("连接失败：{}", root_msg(&e)))
    } else if e.is_decode() {
        Error::network(format!("响应解码失败：{e}"))
    } else if e.is_redirect() {
        Error::network(format!("重定向异常：{e}"))
    } else {
        Error::network(e.to_string())
    }
}

fn root_msg(e: &reqwest::Error) -> String {
    // reqwest 的 Display 已包含 URL；这里尽量给出最底层原因
    let mut msg = e.to_string();
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(s) = src {
        msg = s.to_string();
        src = s.source();
    }
    msg
}

/// 非 2xx 响应 → 统一错误。
pub fn http_status_error(status: u16, body: &str) -> Error {
    let snippet: String = body.chars().take(240).collect();
    if status == 401 || status == 403 {
        // 403 在网盘里经常是「Cookie 失效/风控」，按认证错误处理更贴近实际
        if snippet.contains("登录") || snippet.contains("login") || snippet.contains("auth") {
            return Error::auth(format!("HTTP {status}：{snippet}"));
        }
    }
    Error::server(status as i64, format!("HTTP {status}：{snippet}"))
}

/// 检查响应状态；非 2xx 读 body 生成错误。
pub async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let code = status.as_u16();
    let body = resp.text().await.unwrap_or_default();
    Err(http_status_error(code, &body))
}

/// 解析 JSON 响应（非 2xx 已在 `check_status` 处理）。
pub async fn json_value(resp: reqwest::Response) -> Result<Value> {
    let resp = check_status(resp).await?;
    let text = resp.text().await.map_err(net_err)?;
    parse_json_text(&text)
}

/// 宽松解析：先按 JSON 解析，失败时把文本前 200 字塞进错误。
pub fn parse_json_text(text: &str) -> Result<Value> {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => Ok(v),
        Err(e) => {
            let snippet: String = text.chars().take(200).collect();
            Err(Error::other(format!("响应不是合法 JSON（{e}）：{snippet}")))
        }
    }
}

/// 发送请求并解析 JSON。
pub async fn send_json(req: reqwest::RequestBuilder) -> Result<Value> {
    let resp = req.send().await.map_err(net_err)?;
    json_value(resp).await
}

/// 兼容旧名。
pub async fn json_value_loose(resp: reqwest::Response) -> Result<Value> {
    json_value(resp).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_ok() {
        let v = parse_json_text(r#"{"a":1}"#).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn parse_json_err_has_snippet() {
        let e = parse_json_text("<html>bad</html>").unwrap_err();
        assert!(e.to_string().contains("bad"));
    }

    #[test]
    fn status_error_auth() {
        let e = http_status_error(403, r#"{"msg":"请登录后使用"}"#);
        assert!(e.is_auth());
        let e = http_status_error(500, "oops");
        assert!(e.retryable());
    }
}
