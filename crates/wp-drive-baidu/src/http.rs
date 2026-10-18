//! HTTP 原语：公共 query、显式 Cookie、errno 检查、连接层重试、`-6` 自愈。
//!
//! 研究文档 §2：
//! * Cookie 每请求显式发送，顺序 `BDUSS; BDUSS_BFESS; STOKEN`，不用 cookie jar；
//! * 公共 query：`clienttype=0&app_id=250528&web=1&dp-logid=<16+4>`；
//! * 写操作带 `channel=chunlei`，需要时带 `bdstoken`；
//! * 只对连接建立失败重试（3 次，0.5/1/2s），读超时不重试；
//! * `errno:-6` 且需要认证时，刷新一次 bdstoken 后重放一次。

use crate::{lock, map_errno, BaiduDrive, APP_ID, PAN, PASSPORT, UA};
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, COOKIE, ORIGIN, REFERER};
use reqwest::{Method, Response};
use serde_json::Value;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use wp_core::{Error, Result};
use wp_net::client::{build_client, http_status_error, net_err, ClientOptions};
use wp_net::crypto::base64_encode;

/// 一次业务请求的可选行为。
#[derive(Debug, Clone, Default)]
pub(crate) struct ReqOpts {
    /// 是否要求已登录（失败直接 Auth），并触发 `-6` 自愈。
    pub auth: bool,
    /// 是否带 `channel=chunlei`。
    pub channel: bool,
    /// 是否带 `bdstoken`（缺失时先刷新一次）。
    pub bdstoken: bool,
    /// 额外允许的 errno（如 rapidupload 的 404）。
    pub allow_errno: Vec<i64>,
    /// 额外 Cookie（分享的 `BDCLND`）。
    pub extra_cookie: Option<String>,
    /// 覆盖默认请求超时。
    pub timeout: Option<Duration>,
}

impl ReqOpts {
    /// 已登录业务请求，默认带 channel。
    pub fn authed() -> Self {
        Self {
            auth: true,
            channel: true,
            ..Default::default()
        }
    }

    /// 匿名请求（passport/登录/引导）。
    pub fn anon() -> Self {
        Self::default()
    }

    pub fn channel(mut self) -> Self {
        self.channel = true;
        self
    }

    pub fn no_channel(mut self) -> Self {
        self.channel = false;
        self
    }

    pub fn with_bdstoken(mut self) -> Self {
        self.bdstoken = true;
        self
    }

    pub fn allow(mut self, code: i64) -> Self {
        self.allow_errno.push(code);
        self
    }
}

/// locateupload 域名缓存（60 秒有效）。
#[derive(Debug, Clone)]
pub(crate) struct DomainCache {
    pub servers: Vec<String>,
    pub index: usize,
    pub fetched_at: Instant,
}

/// HTTP 客户端集合。
pub(crate) struct Http {
    /// 普通 API（总超时 60s，连接 10s）。
    pub api: reqwest::Client,
    /// 下载专用：不设总超时，避免大文件被 reqwest 整体超时打断。
    pub download: reqwest::Client,
    /// passport 不跟随重定向，用于抓取 qrbdusslogin 的 302 Set-Cookie。
    pub passport: reqwest::Client,
    /// passport 跟随重定向（二维码图片、pan 域会话、子域种植）。
    pub passport_follow: reqwest::Client,
    pub dp: DpLogId,
}

fn client_options(timeout_secs: u64, connect_secs: u64, follow: bool) -> ClientOptions {
    let mut opts = ClientOptions::new(UA);
    opts.timeout_secs = timeout_secs;
    opts.connect_timeout_secs = connect_secs;
    opts.follow_redirects = follow;
    // 百度协议不依赖系统代理；Python 版 httpx 默认 trust_env，这里保留
    // 环境变量代理能力（no_proxy=false 才不强制直连）。
    opts.no_proxy = false;
    opts.http2 = false;
    opts
}

impl Http {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            api: build_client(client_options(60, 10, true))?,
            download: build_client(client_options(0, 30, true))?,
            passport: build_client(client_options(60, 10, false))?,
            passport_follow: build_client(client_options(60, 10, true))?,
            dp: DpLogId::new(),
        })
    }
}

// ───────────────────────────── dp-logid / logid ─────────────────────────────

/// `dp-logid` 生成器：16 位随机数字前缀 + 4 位序号，序号超过 9999 换前缀。
pub(crate) struct DpLogId {
    state: Mutex<(String, u32)>,
}

impl DpLogId {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new((random_digits16(), 0)),
        }
    }

    pub(crate) fn next(&self) -> String {
        let mut guard = lock(&self.state);
        if guard.1 >= 9999 {
            guard.0 = random_digits16();
            guard.1 = 0;
        }
        guard.1 += 1;
        format!("{}{:04}", guard.0, guard.1)
    }

    /// superfile2 每片使用“新随机前缀 + 0001”（文档 §2.6）。
    pub(crate) fn fresh(&self) -> String {
        format!("{}0001", random_digits16())
    }
}

fn random_digits16() -> String {
    let n = uuid::Uuid::new_v4().as_u128() % 10_000_000_000_000_000u128;
    format!("{n:016}")
}

/// superfile2 的 `logid`：base64("<13 位毫秒><1 位随机>.<17 位随机>")。
pub(crate) fn gen_logid() -> String {
    let ms = chrono::Utc::now().timestamp_millis();
    let raw_uuid = uuid::Uuid::new_v4().as_u128();
    let rand1 = raw_uuid % 10;
    let rand17 = (raw_uuid / 10) % 100_000_000_000_000_000u128;
    let raw = format!("{ms}{rand1}.{rand17:017}");
    base64_encode(raw.as_bytes())
}

// ───────────────────────────── JSON 解析 ─────────────────────────────

/// 解析 JSON 响应。非 2xx 在调用方先处理；这里把“响应不是合法 JSON”
/// 归为服务端错误（可读片段），与 Python 版的“接口错误”对齐。
pub(crate) fn parse_json_body(status: u16, text: &str) -> Result<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(Error::server(status as i64, "响应为空（不是合法 JSON）"));
    }
    serde_json::from_str(trimmed).map_err(|e| {
        let snippet: String = trimmed.chars().take(300).collect();
        Error::server(
            status as i64,
            format!("响应不是合法 JSON（{e}）：{snippet}"),
        )
    })
}

// ───────────────────────────── 请求执行 ─────────────────────────────

impl BaiduDrive {
    /// GET 业务 API（带公共 query / Cookie / errno 检查 / -6 自愈）。
    pub(crate) async fn api_get(
        &self,
        path: &str,
        params: &[(String, String)],
        opts: ReqOpts,
    ) -> Result<Value> {
        self.api_with_heal(Method::GET, path, params, None, opts)
            .await
    }

    /// POST 表单业务 API（`application/x-www-form-urlencoded`）。
    pub(crate) async fn api_post_form(
        &self,
        path: &str,
        params: &[(String, String)],
        form: &[(String, String)],
        opts: ReqOpts,
    ) -> Result<Value> {
        self.api_with_heal(Method::POST, path, params, Some(form), opts)
            .await
    }

    async fn api_with_heal(
        &self,
        method: Method,
        path: &str,
        params: &[(String, String)],
        form: Option<&[(String, String)]>,
        opts: ReqOpts,
    ) -> Result<Value> {
        let mut healed = false;
        loop {
            match self.execute_once(&method, path, params, form, &opts).await {
                Ok(v) => {
                    let code = v.get("errno").and_then(Value::as_i64);
                    match code {
                        None | Some(0) => return Ok(v),
                        Some(c) if opts.allow_errno.contains(&c) => return Ok(v),
                        Some(-6) if opts.auth && !healed => {
                            healed = true;
                            wp_core::log_warn!(
                                "百度",
                                "接口 {path} 返回 -6，刷新 bdstoken 后重放一次"
                            );
                            match self.refresh_template().await {
                                Ok(()) => continue,
                                Err(e) => {
                                    wp_core::log_warn!("百度", "会话刷新失败：{e}");
                                    return Err(map_errno(-6, &v));
                                }
                            }
                        }
                        Some(c) => return Err(map_errno(c, &v)),
                    }
                }
                Err(Error::Auth(msg)) if opts.auth && !healed => {
                    // 例如首次缺少 bdstoken；给会话刷新一次机会。
                    healed = true;
                    if self.refresh_template().await.is_ok() {
                        continue;
                    }
                    return Err(Error::Auth(msg));
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn execute_once(
        &self,
        method: &Method,
        path: &str,
        params: &[(String, String)],
        form: Option<&[(String, String)]>,
        opts: &ReqOpts,
    ) -> Result<Value> {
        if opts.auth && !self.session_valid() {
            return Err(Error::auth("未登录或登录已过期，请重新登录"));
        }

        let mut query = params.to_vec();
        query.push(("clienttype".into(), "0".into()));
        query.push(("app_id".into(), APP_ID.into()));
        query.push(("web".into(), "1".into()));
        query.push(("dp-logid".into(), self.dp_next()));
        if opts.channel {
            query.push(("channel".into(), "chunlei".into()));
        }
        if opts.bdstoken {
            let token = self.ensure_bdstoken().await?;
            query.push(("bdstoken".into(), token));
        }

        let mut cookie = self.cookie_header();
        if let Some(extra) = opts.extra_cookie.as_deref() {
            if !extra.trim().is_empty() {
                if cookie.is_empty() {
                    cookie = extra.to_string();
                } else {
                    cookie.push_str("; ");
                    cookie.push_str(extra);
                }
            }
        }

        let url = format!("{PAN}{path}");
        let mut connect_attempt = 0u32;
        let resp: Response;
        loop {
            let mut rb = self
                .http
                .api
                .request(method.clone(), url.as_str())
                .query(&query);
            if let Some(form) = form {
                rb = rb.form(form);
            }
            if !cookie.is_empty() {
                rb = rb.header(COOKIE, &cookie);
            }
            rb = rb
                .header(ACCEPT, "application/json, text/plain, */*")
                .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9")
                .header(ORIGIN, PAN)
                .header(REFERER, "https://pan.baidu.com/disk/main");
            if let Some(timeout) = opts.timeout {
                rb = rb.timeout(timeout);
            }
            match rb.send().await {
                Ok(r) => {
                    resp = r;
                    break;
                }
                Err(e) if e.is_connect() && connect_attempt < 2 => {
                    connect_attempt += 1;
                    let wait_ms = 500u64 * (1u64 << (connect_attempt - 1));
                    wp_core::log_warn!(
                        "百度",
                        "连接失败，第 {connect_attempt}/2 次重试（{wait_ms}ms 后）：{e}"
                    );
                    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                }
                Err(e) => return Err(net_err(e)),
            }
        }

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(http_status_error(status.as_u16(), &body));
        }
        let text = resp.text().await.map_err(net_err)?;
        parse_json_body(status.as_u16(), &text)
    }

    /// 取 bdstoken；缺失时刷新一次。
    pub(crate) async fn ensure_bdstoken(&self) -> Result<String> {
        let token = self.bdstoken_value();
        if !token.is_empty() {
            return Ok(token);
        }
        self.refresh_template().await?;
        let token = self.bdstoken_value();
        if token.is_empty() {
            return Err(Error::auth("会话缺少 bdstoken，请重新登录"));
        }
        Ok(token)
    }

    /// 会话引导：优先 `GET /api/gettemplatevariable`，失败回退 `GET /api/loginStatus`。
    ///
    /// 只补 CSRF 令牌，**不能续期 BDUSS/STOKEN**。
    ///
    /// ⚠️ 真机实测（2026-10-18 扫码会话 + V2 源码 `后端_百度.py:149/576` 记录）：
    /// 有效会话下 `/api/gettemplatevariable` 也可能回 `errno:-6`，
    /// 而 `/api/loginStatus?clienttype=1&channel=web` 会下发 bdstoken/uk。
    /// 因此必须做双通道回退，否则扫码登录后会误判为「未登录」。
    pub(crate) async fn refresh_template(&self) -> Result<()> {
        match self.refresh_via_template_variable().await {
            Ok(()) => return Ok(()),
            Err(e) => {
                wp_core::log_warn!(
                    "百度",
                    "gettemplatevariable 刷新失败（{e}），回退 /api/loginStatus"
                );
            }
        }
        self.refresh_via_login_status().await
    }

    /// 回退通道：`GET /api/loginStatus?clienttype=1&app_id=250528&web=1&channel=web&version=0`。
    ///
    /// 请求特征（真机 HAR 实测）：**不带**业务公共参数（无 dp-logid / clienttype=0），
    /// 响应 `login_info.{bdstoken,uk,username,vip_type}`。
    async fn refresh_via_login_status(&self) -> Result<()> {
        let url = format!(
            "{PAN}/api/loginStatus?clienttype=1&app_id={APP_ID}&web=1&channel=web&version=0"
        );
        let cookie = self.cookie_header();
        let mut req = self
            .http
            .api
            .get(&url)
            .header(ACCEPT, "application/json, text/plain, */*")
            .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9")
            .header(REFERER, "https://pan.baidu.com/")
            .header("X-Requested-With", "XMLHttpRequest")
            .timeout(Duration::from_secs(20));
        if !cookie.is_empty() {
            req = req.header(COOKIE, cookie);
        }
        let resp = req.send().await.map_err(net_err)?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(net_err)?;
        let v = parse_json_body(status, &text)?;
        let errno = v.get("errno").and_then(Value::as_i64).unwrap_or(-1);
        if errno != 0 {
            return Err(map_errno(errno, &v));
        }
        let info = v
            .get("login_info")
            .and_then(Value::as_object)
            .ok_or_else(|| Error::server(-1, "loginStatus 响应缺少 login_info"))?;
        let bdstoken = info
            .get("bdstoken")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if bdstoken.is_empty() {
            return Err(Error::auth("会话无效：loginStatus 未返回 bdstoken"));
        }
        let uk = info.get("uk").and_then(Value::as_i64);
        let username = info
            .get("username")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        self.mutate_session(|s| {
            s.bdstoken = bdstoken;
            if let Some(u) = uk {
                s.uk = Some(u);
            }
        });
        wp_core::log_info!(
            "百度",
            "会话已通过 /api/loginStatus 刷新：bdstoken({}) uk={:?} username={}",
            self.bdstoken_value().len(),
            self.uk_value(),
            username
        );
        Ok(())
    }

    /// 主通道：`GET /api/gettemplatevariable`，刷新 bdstoken / uk。
    async fn refresh_via_template_variable(&self) -> Result<()> {
        let fields = r#"["bdstoken","token","uk","isdocuser","servertime"]"#;
        let params = vec![("fields".to_string(), fields.to_string())];
        // 这里必须 Box::pin：refresh_template ↔ api_get ↔ execute_once ↔
        // ensure_bdstoken 在类型层面构成递归 future（运行时不会递归，因为
        // 本请求 auth=false/bdstoken=false），加一层间接即可通过编译。
        let v = Box::pin(self.api_get(
            "/api/gettemplatevariable",
            &params,
            ReqOpts::anon().channel(),
        ))
        .await?;
        let result = v
            .get("result")
            .and_then(Value::as_object)
            .ok_or_else(|| Error::server(-1, "gettemplatevariable 响应缺少 result"))?;
        let bdstoken = result
            .get("bdstoken")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if bdstoken.is_empty() {
            return Err(Error::auth("会话无效：gettemplatevariable 未返回 bdstoken"));
        }
        let uk = result.get("uk").and_then(Value::as_i64);
        self.mutate_session(|s| {
            s.bdstoken = bdstoken;
            if let Some(u) = uk {
                s.uk = Some(u);
            }
        });
        wp_core::log_debug!(
            "百度",
            "会话已刷新：bdstoken({}) uk={:?}",
            self.bdstoken_value().len(),
            self.uk_value()
        );
        Ok(())
    }

    /// passport 域 GET 文本（不注入网盘公共参数/Cookie）。
    pub(crate) async fn passport_get_text(
        &self,
        path: &str,
        params: &[(String, String)],
        timeout: Duration,
        follow: bool,
    ) -> Result<String> {
        let url = format!("{PASSPORT}{path}");
        let client = if follow {
            &self.http.passport_follow
        } else {
            &self.http.passport
        };
        let resp = client
            .get(&url)
            .query(params)
            .header(ACCEPT, "application/json, text/plain, */*")
            .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9")
            .header(REFERER, "https://pan.baidu.com/")
            .timeout(timeout)
            .send()
            .await
            .map_err(net_err)?;
        let status = resp.status();
        let text = resp.text().await.map_err(net_err)?;
        if !status.is_success() {
            let snippet: String = text.chars().take(240).collect();
            return Err(http_status_error(status.as_u16(), &snippet));
        }
        Ok(text)
    }

    /// 下载二维码图片（passport 域，跟随重定向）。
    pub(crate) async fn passport_get_bytes(&self, url: &str, timeout: Duration) -> Result<Vec<u8>> {
        let resp = self
            .http
            .passport_follow
            .get(url)
            .header(ACCEPT, "image/png,image/*,*/*")
            .header(REFERER, "https://pan.baidu.com/")
            .timeout(timeout)
            .send()
            .await
            .map_err(net_err)?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(http_status_error(status.as_u16(), &body));
        }
        let bytes = resp.bytes().await.map_err(net_err)?;
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dp_logid_shape_and_sequence() {
        let dp = DpLogId::new();
        let a = dp.next();
        let b = dp.next();
        assert_eq!(a.len(), 20);
        assert_eq!(b.len(), 20);
        assert!(a.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(&a[16..], "0001");
        assert_eq!(&b[16..], "0002");
        assert_eq!(&a[..16], &b[..16], "同一实例前缀应恒定");
        let f = dp.fresh();
        assert_eq!(f.len(), 20);
        assert_eq!(&f[16..], "0001");
    }

    #[test]
    fn logid_is_base64_of_expected_shape() {
        let logid = gen_logid();
        let raw = wp_net::crypto::base64_decode(&logid).unwrap();
        let text = String::from_utf8(raw).unwrap();
        let (left, right) = text.split_once('.').expect("logid 应含点号");
        assert_eq!(left.len(), 14);
        assert_eq!(right.len(), 17);
        assert!(left.chars().all(|c| c.is_ascii_digit()));
        assert!(right.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn json_parse_errors_are_server_errors() {
        assert!(parse_json_body(200, "<html>no</html>").is_err());
        match parse_json_body(200, "not json") {
            Err(Error::Server { code, .. }) => assert_eq!(code, 200),
            other => panic!("应为 Server，实际 {other:?}"),
        }
        let v = parse_json_body(200, r#"{"errno":0}"#).unwrap();
        assert_eq!(v["errno"], 0);
    }
}
