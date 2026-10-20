//! 光鸭云盘适配器（Rust）。
//!
//! 协议依据 `docs/research/光鸭_协议.md`，并对照 Python 源项目
//! `光鸭云盘适配器/` 的只读实现。差异遵循“规格与源码不一致时以源码为准”，
//! 已知改进点都写在对应模块注释里。
//!
//! # 对外入口
//! 只有 [`create`]：
//! ```no_run
//! use wp_drive::DriveInit;
//! let init = DriveInit {
//!     id: "guangya".into(),
//!     name: "光鸭云盘".into(),
//!     data_dir: std::path::PathBuf::from("/tmp/guangya"),
//!     threads: 0,
//!     verbose: false,
//! };
//! let drive = wp_drive_guangya::create(init).unwrap();
//! ```
//!
//! # 能力
//! * 扫码 / 短信 / 令牌登录，access+refresh 令牌持久化与 401 自愈；
//! * 浏览、分页列目录、递归建目录、删除（进回收站）、直链、分享；
//! * 回收站列表 / 清空、云添加任务创建 / 列表、上传/操作记录；
//! * 上传：MD5 → `get_res_center_token` → OSS V1 签名单 PUT（小文件）或
//!   multipart（大文件）→ 业务完成确认 → 轮询任务；
//! * 下载：取签名直链 → 单连接断点续传 / 多段并发（`wp-net` 下载器）；
//! * 缩略图：列表条目映射 `thumbnail` URL，按需下载原始字节（5 MB 上限）。
//!
//! 不支持的 Drive 能力（无服务端接口）：重命名、移动/复制、搜索、回收站单项
//! 恢复/彻底删除、分享解析/转存、分类浏览。
//!
//! # 线程安全
//! 内部实例 `GuangyaDrive` 只使用 `Mutex<Option<...>>` 保护扫码/短信临时状态，
//! 会话由 [`session::SessionStore`] 自己保护，满足 `Drive: Send + Sync`。

mod auth;
#[allow(dead_code)]
mod cloud_add;
mod device;
mod download;
mod files;
mod records;
mod recycle;
mod session;
mod share;
mod sign;
mod thumbnail;
mod upload;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::Method;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use wp_core::model::{AuthCaps, DriveKind};
use wp_core::{Error, Result};
use wp_drive::{Drive, DriveCaps, DriveInit};

/// 业务 API。
pub(crate) const API_BASE: &str = "https://api.guangyapan.com";
/// 认证中心。
pub(crate) const ACCOUNT_BASE: &str = "https://account.guangyapan.com";
/// Web Origin / Referer。
pub(crate) const WEB_ORIGIN: &str = "https://www.guangyapan.com";
/// Authing 客户端 ID（登录响应 JWT 的 aud 同值）。
pub(crate) const CLIENT_ID: &str = "aMe-8VSlkrbQXpUR";
/// `x-client-version`。
pub(crate) const CLIENT_VERSION: &str = "0.0.1";
/// 登录/用户信息请求的 `x-sdk-version`。
pub(crate) const SDK_LOGIN: &str = "9.1.3";
/// 刷新令牌请求的 `x-sdk-version`（源码就是 9.0.2，保持原样）。
pub(crate) const SDK_REFRESH: &str = "9.0.2";

const API_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36";

/// 业务码白名单（缺省/code=0、147=上传中、156=秒传、200）。
pub(crate) const DEFAULT_ALLOW_CODES: &[i64] = &[0, 147, 156, 200];

/// 进行中的扫码会话。
#[derive(Debug, Clone)]
pub(crate) struct QrPending {
    pub device_code: String,
    pub interval_secs: u64,
    pub expires_at: i64,
    pub next_poll_at: i64,
}

/// 已发送短信、等待校验的会话。
#[derive(Debug, Clone)]
pub(crate) struct SmsPending {
    pub phone: String,
    pub verification_id: String,
    pub expires_at: i64,
}

/// 光鸭云盘驱动实例。
pub(crate) struct GuangyaDrive {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) device: device::DeviceIdentity,
    pub(crate) session: session::SessionStore,
    /// JSON API / 认证请求（总超时 60s）。
    pub(crate) api: reqwest::Client,
    /// OSS / 下载（长超时、跟随 302）。
    pub(crate) xfer: reqwest::Client,
    pub(crate) qr: Mutex<Option<QrPending>>,
    pub(crate) sms: Mutex<Option<SmsPending>>,
    pub(crate) verbose: bool,
}

/// 创建光鸭云盘适配器。
pub fn create(init: DriveInit) -> Result<Arc<dyn Drive>> {
    std::fs::create_dir_all(&init.data_dir)?;
    let device = device::DeviceIdentity::load_or_create(&init.data_dir)?;
    let session = session::SessionStore::load(init.data_dir.join("session.json"));

    let api = wp_net::client::build_client(wp_net::client::ClientOptions {
        user_agent: API_UA.to_string(),
        timeout_secs: 60,
        connect_timeout_secs: 10,
        follow_redirects: true,
        no_proxy: false,
        http2: false,
    })?;
    let xfer = wp_net::client::build_client(wp_net::client::ClientOptions {
        user_agent: BROWSER_UA.to_string(),
        // 0 = 不设总超时；大文件上传/下载靠连接超时兜底。
        timeout_secs: 0,
        connect_timeout_secs: 30,
        follow_redirects: true,
        no_proxy: false,
        http2: false,
    })?;

    let name = if init.name.trim().is_empty() {
        "光鸭云盘".to_string()
    } else {
        init.name
    };
    if init.verbose {
        wp_core::log_debug!(
            "光鸭",
            "初始化实例 {}（data_dir={}，device={}）",
            init.id,
            init.data_dir.display(),
            &device.device_id[..8.min(device.device_id.len())]
        );
    }
    Ok(Arc::new(GuangyaDrive {
        id: init.id,
        name,
        device,
        session,
        api,
        xfer,
        qr: Mutex::new(None),
        sms: Mutex::new(None),
        verbose: init.verbose,
    }))
}

impl GuangyaDrive {
    /// 业务 API 每请求头（`did/dt/smid/traceparent` + content-type）。
    pub(crate) fn api_headers(&self, content_type: Option<&str>) -> Result<HeaderMap> {
        let trace = self.device.traceparent();
        let mut h = headers(&[
            ("accept", "application/json, text/plain, */*"),
            ("accept-language", "zh-CN,zh;q=0.8"),
            ("origin", WEB_ORIGIN),
            ("referer", "https://www.guangyapan.com/"),
            ("user-agent", API_UA),
            ("did", &self.device.device_id),
            ("dt", &self.device.dt),
            ("smid", &self.device.smid),
            ("traceparent", &trace),
        ])?;
        if let Some(ct) = content_type {
            h.insert(
                CONTENT_TYPE,
                HeaderValue::from_str(ct)
                    .map_err(|e| Error::other(format!("Content-Type 非法：{e}")))?,
            );
        }
        Ok(h)
    }

    /// 认证中心请求头（不含 `did/dt/smid/traceparent`，但带 `x-device-id`）。
    pub(crate) fn account_headers(&self, sdk_version: &str) -> Result<HeaderMap> {
        headers(&[
            ("accept", "*/*"),
            ("content-type", "application/json"),
            ("origin", WEB_ORIGIN),
            ("referer", "https://www.guangyapan.com/"),
            ("user-agent", API_UA),
            ("x-client-id", CLIENT_ID),
            ("x-client-version", CLIENT_VERSION),
            ("x-device-id", &self.device.device_id),
            ("x-device-model", "chrome%2F147.0.0.0"),
            ("x-device-name", "PC-Chrome"),
            ("x-net-work-type", "NONE"),
            ("x-os-version", "MacIntel"),
            ("x-platform-version", "1"),
            ("x-protocol-version", "301"),
            ("x-provider-name", "NONE"),
            ("x-sdk-version", sdk_version),
        ])
    }

    /// 取有效 access_token（过期则先刷新）。
    pub(crate) async fn bearer(&self) -> Result<String> {
        let headers = self.account_headers(SDK_REFRESH)?;
        self.session.token(&self.api, &headers).await
    }

    /// 业务 API POST（自动 Bearer、401 刷新后重放一次、业务码校验）。
    pub(crate) async fn api_post(
        &self,
        path: &str,
        body: Value,
        allow_codes: &[i64],
    ) -> Result<Value> {
        self.api_post_opt(path, Some(body), allow_codes).await
    }

    /// 业务 API POST，不带请求体（`clear_recycle_bin` 明确要求无 body）。
    pub(crate) async fn api_post_no_body(&self, path: &str, allow_codes: &[i64]) -> Result<Value> {
        self.api_post_opt(path, None, allow_codes).await
    }

    async fn api_post_opt(
        &self,
        path: &str,
        body: Option<Value>,
        allow_codes: &[i64],
    ) -> Result<Value> {
        let url = format!("{API_BASE}{path}");
        let mut refreshed = false;
        loop {
            let token = self.bearer().await?;
            let content_type = body.as_ref().map(|_| "application/json");
            let mut h = self.api_headers(content_type)?;
            h.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|e| Error::other(format!("Authorization 头非法：{e}")))?,
            );
            let mut req = self.api.post(&url).headers(h);
            if let Some(body) = body.as_ref() {
                req = req.json(body);
            }
            let resp = req.send().await.map_err(wp_net::client::net_err)?;
            let status = resp.status().as_u16();
            let request_id = response_request_id(&resp);
            let text = resp.text().await.map_err(wp_net::client::net_err)?;

            if status == 401 && !refreshed {
                // 401 自愈：单飞刷新后重放一次；刷新失败直接 Auth。
                refreshed = true;
                let refresh_headers = self.account_headers(SDK_REFRESH)?;
                self.session
                    .force_refresh(&self.api, &refresh_headers)
                    .await?;
                continue;
            }
            return parse_api_body(status, &text, request_id.as_deref(), allow_codes);
        }
    }

    /// 认证中心原始请求（登录/刷新/用户信息用；不做业务码校验）。
    pub(crate) async fn account_call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        bearer: Option<&str>,
        sdk_version: &str,
        extra_headers: &[(&str, &str)],
    ) -> Result<(u16, String, Option<String>)> {
        let url = format!("{ACCOUNT_BASE}{path}");
        let mut h = self.account_headers(sdk_version)?;
        if let Some(token) = bearer {
            h.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|e| Error::other(format!("Authorization 头非法：{e}")))?,
            );
        }
        for (k, v) in extra_headers {
            h.insert(
                HeaderName::from_bytes(k.as_bytes())
                    .map_err(|e| Error::other(format!("请求头 {k} 非法：{e}")))?,
                HeaderValue::from_str(v)
                    .map_err(|e| Error::other(format!("请求头 {k} 值非法：{e}")))?,
            );
        }
        let mut req = self.api.request(method, url).headers(h);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.map_err(wp_net::client::net_err)?;
        let status = resp.status().as_u16();
        let request_id = response_request_id(&resp);
        let text = resp.text().await.map_err(wp_net::client::net_err)?;
        Ok((status, text, request_id))
    }
}

#[async_trait]
impl Drive for GuangyaDrive {
    fn kind(&self) -> DriveKind {
        DriveKind::Guangya
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn caps(&self) -> DriveCaps {
        DriveCaps {
            auth: AuthCaps {
                qr: true,
                cookie: false,
                sms: true,
                password: false,
                token: true,
                // 光鸭有原生短信登录接口，无需浏览器通道
                browser: false,
            },
            mkdir: true,
            // 源码与抓包都没有 rename/move/copy/search 接口。
            rename: false,
            move_copy: false,
            search: false,
            share: true,
            direct_link: true,
            // 回收站：有列表和清空；源码没有单项恢复/彻底删除接口。
            recycle: true,
            upload: true,
            download: true,
            recycle_list: true,
            recycle_restore: false,
            recycle_delete: false,
            recycle_clear: true,
            // 云添加：创建任务 + 任务列表；没有暂停/删除任务接口。
            cloud_add: true,
            cloud_tasks: true,
            // 上传/操作记录来自 get_user_action。
            upload_records: true,
            // 分享没有解析/转存接口；也没有按分类列全盘文件的接口。
            share_info: false,
            share_save: false,
            categories: false,
        }
    }

    // account / logout / 登录能力在 auth.rs 实现。
    async fn account(&self) -> Result<wp_core::model::AccountInfo> {
        auth::account_info(self).await
    }

    async fn logout(&self) -> Result<()> {
        auth::logout(self).await
    }

    async fn begin_qr(&self) -> Result<wp_drive::QrChallenge> {
        self.qr_begin().await
    }

    async fn poll_qr(&self) -> Result<wp_drive::QrPoll> {
        self.qr_poll().await
    }

    async fn cancel_qr(&self) -> Result<()> {
        self.qr_cancel().await
    }

    async fn login_token(&self, token: &str, extra: &str) -> Result<wp_core::model::AccountInfo> {
        self.login_with_token(token, extra).await
    }

    async fn send_sms(&self, phone: &str) -> Result<String> {
        self.sms_send(phone).await
    }

    async fn verify_sms(&self, phone: &str, code: &str) -> Result<wp_core::model::AccountInfo> {
        self.sms_verify(phone, code).await
    }

    // 浏览/操作在 files.rs，传输在 upload.rs / download.rs，分享在 share.rs。
    async fn list(&self, path: &str) -> Result<Vec<wp_core::model::RemoteEntry>> {
        files::list(self, path).await
    }

    async fn stat(&self, path: &str) -> Result<wp_core::model::RemoteEntry> {
        files::stat(self, path).await
    }

    async fn ensure_dir(&self, path: &str) -> Result<wp_core::model::RemoteEntry> {
        files::ensure_dir(self, path).await
    }

    async fn delete(&self, paths: &[String]) -> Result<wp_core::model::OpReport> {
        files::delete(self, paths).await
    }

    async fn rename(&self, _path: &str, _new_name: &str) -> Result<wp_core::model::RemoteEntry> {
        Err(Error::unsupported("光鸭云盘未提供重命名接口"))
    }

    async fn move_entries(
        &self,
        _paths: &[String],
        _dest_dir: &str,
    ) -> Result<wp_core::model::OpReport> {
        Err(Error::unsupported("光鸭云盘未提供移动接口"))
    }

    async fn copy_entries(
        &self,
        _paths: &[String],
        _dest_dir: &str,
    ) -> Result<wp_core::model::OpReport> {
        Err(Error::unsupported("光鸭云盘未提供复制接口"))
    }

    async fn search(&self, _keyword: &str, _dir: &str) -> Result<Vec<wp_core::model::RemoteEntry>> {
        Err(Error::unsupported("光鸭云盘未提供搜索接口"))
    }

    async fn direct_link(&self, path: &str) -> Result<wp_core::model::DirectLink> {
        download::direct_link(self, path).await
    }

    async fn download(
        &self,
        remote: &str,
        local: &std::path::Path,
        opts: &wp_core::model::DownloadOptions,
        progress: wp_core::model::ProgressFn,
    ) -> Result<u64> {
        download::download(self, remote, local, opts, progress).await
    }

    async fn fetch_thumbnail(&self, entry: &wp_core::model::RemoteEntry) -> Result<Vec<u8>> {
        thumbnail::fetch(self, entry).await
    }

    async fn upload(
        &self,
        local: &std::path::Path,
        remote_dir: &str,
        opts: &wp_core::model::UploadOptions,
        progress: wp_core::model::ProgressFn,
    ) -> Result<wp_core::model::RemoteEntry> {
        upload::upload(self, local, remote_dir, opts, progress).await
    }

    async fn create_share(
        &self,
        paths: &[String],
        opts: &wp_drive::ShareOptions,
    ) -> Result<wp_core::model::ShareInfo> {
        share::create_share(self, paths, opts).await
    }

    async fn list_shares(&self) -> Result<Vec<wp_core::model::ShareInfo>> {
        share::list_shares(self).await
    }

    async fn cancel_shares(&self, ids: &[String]) -> Result<wp_core::model::OpReport> {
        share::cancel_shares(self, ids).await
    }

    // 回收站在 recycle.rs，上传/操作记录在 records.rs，云添加在 cloud_add.rs。
    // 恢复/彻底删除/分享解析/转存/分类浏览没有服务端接口，沿用 trait 默认
    // 的 Unsupported 实现（caps 中也已标为 false）。
    async fn recycle_list(&self) -> Result<Vec<wp_core::model::RecycleEntry>> {
        recycle::list(self).await
    }

    async fn recycle_clear(&self) -> Result<wp_core::model::OpReport> {
        recycle::clear(self).await
    }

    async fn cloud_add(&self, url: &str, dest_dir: &str) -> Result<String> {
        crate::cloud_add::add(self, url, dest_dir).await
    }

    async fn cloud_tasks(&self) -> Result<Vec<wp_core::model::CloudTask>> {
        crate::cloud_add::tasks(self).await
    }

    async fn upload_records(&self, limit: usize) -> Result<Vec<wp_core::model::UploadRecord>> {
        records::upload_records(self, limit).await
    }

    async fn write_probe(&self) -> Result<String> {
        Err(Error::unsupported(
            "光鸭云盘写权限探测未实现（避免在用户根目录留下探测文件）",
        ))
    }
}

/// 构造请求头；所有值必须是 ASCII（HTTP 协议要求）。
pub(crate) fn headers(pairs: &[(&str, &str)]) -> Result<HeaderMap> {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| Error::other(format!("请求头 {k} 非法：{e}")))?;
        if !v.is_ascii() {
            return Err(Error::other(format!(
                "请求头 {k} 的值含非 ASCII 字符，HTTP 头不允许：{}",
                snippet(v, 80)
            )));
        }
        let value = HeaderValue::from_str(v)
            .map_err(|e| Error::other(format!("请求头 {k} 的值含非法字符：{e}")))?;
        h.insert(name, value);
    }
    Ok(h)
}

fn response_request_id(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// 解析业务 API 响应：HTTP 错误 -> 统一错误；业务 code 白名单校验。
pub(crate) fn parse_api_body(
    status: u16,
    text: &str,
    request_id: Option<&str>,
    allow_codes: &[i64],
) -> Result<Value> {
    if !(200..300).contains(&status) {
        return Err(classify_http(status, text, request_id));
    }
    let value: Value = serde_json::from_str(text).map_err(|e| {
        Error::server(
            status as i64,
            format!("响应不是合法 JSON：{e}；{}", snippet(text, 200)),
        )
    })?;
    check_business(&value, allow_codes, request_id)?;
    Ok(value)
}

/// HTTP 状态码 → 错误分类。
pub(crate) fn classify_http(status: u16, body: &str, request_id: Option<&str>) -> Error {
    let msg = snippet(body, 240);
    let rid = request_id
        .map(|r| format!("（request-id: {r}）"))
        .unwrap_or_default();
    match status {
        401 => Error::auth(format!("HTTP 401：{msg}{rid}")),
        403 => {
            let low = body.to_ascii_lowercase();
            if ["登录", "login", "auth", "expired", "token"]
                .iter()
                .any(|k| low.contains(k))
            {
                Error::auth(format!("HTTP 403：{msg}{rid}"))
            } else {
                Error::server(403, format!("HTTP 403：{msg}{rid}"))
            }
        }
        404 => Error::not_found(format!("HTTP 404：{msg}{rid}")),
        _ => Error::server(status as i64, format!("HTTP {status}：{msg}{rid}")),
    }
}

/// 业务码校验：缺省/0/147/156/200 或调用方额外允许的码放行，其余报错。
pub(crate) fn check_business(
    value: &Value,
    allow_codes: &[i64],
    request_id: Option<&str>,
) -> Result<()> {
    let code = value.get("code").and_then(as_i64).unwrap_or(0);
    if code == 0 || DEFAULT_ALLOW_CODES.contains(&code) || allow_codes.contains(&code) {
        return Ok(());
    }
    let msg = value
        .get("msg")
        .and_then(Value::as_str)
        .unwrap_or("接口返回异常");
    let rid = request_id
        .or_else(|| value.get("requestId").and_then(Value::as_str))
        .map(|r| format!("（request-id: {r}）"))
        .unwrap_or_default();
    Err(match code {
        401 => Error::auth(format!("{msg}{rid}")),
        404 => Error::not_found(format!("{msg}{rid}")),
        _ => Error::server(code, format!("{msg}{rid}")),
    })
}

/// `code` 字段可能是数字、字符串或缺失；统一成 i64。
pub(crate) fn as_i64(v: &Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    if let Some(f) = v.as_f64() {
        return Some(f as i64);
    }
    v.as_str()?.trim().parse::<i64>().ok()
}

/// 取 `data` 包裹（不存在/null 时返回自身）。
pub(crate) fn data_or_self(v: &Value) -> &Value {
    v.get("data").filter(|d| !d.is_null()).unwrap_or(v)
}

pub(crate) fn snippet(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

/// 纯逻辑测试：错误分类 / 业务码 / 响应解析。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn http_error_classification() {
        assert!(classify_http(401, "{}", None).is_auth());
        assert!(classify_http(403, r#"{"msg":"请登录后使用"}"#, None).is_auth());
        assert!(!classify_http(403, r#"{"msg":"forbidden"}"#, None).is_auth());
        assert_eq!(
            classify_http(404, "not found", None).kind(),
            wp_core::error::ErrorKind::NotFound
        );
        let e = classify_http(500, "oops", Some("rid-1"));
        assert!(matches!(e, Error::Server { code: 500, .. }));
        assert!(e.retryable());
        assert!(e.to_string().contains("rid-1"));
        assert_eq!(
            classify_http(429, "", None).kind(),
            wp_core::error::ErrorKind::Server
        );
    }

    #[test]
    fn business_code_whitelist() {
        // 缺省 / 0 / 147 / 156 / 200 都算成功
        check_business(&json!({"msg":"success"}), &[], None).unwrap();
        check_business(&json!({"code":0}), &[], None).unwrap();
        check_business(&json!({"code":147,"msg":"上传中"}), &[], None).unwrap();
        check_business(&json!({"code":156,"msg":"上传已完成"}), &[], None).unwrap();
        check_business(&json!({"code":200}), &[], None).unwrap();
        // 159 默认拒绝，显式允许后放行
        let v = json!({"code":159,"msg":"目录已存在","data":{"fileId":"1"}});
        assert!(check_business(&v, &[], None).is_err());
        check_business(&v, &[159], None).unwrap();
        // 其它业务码 -> Server，保留 code/msg
        let e = check_business(&json!({"code":40001,"msg":"风控"}), &[], None).unwrap_err();
        assert!(matches!(e, Error::Server { code: 40001, .. }));
    }

    #[test]
    fn parse_response_and_wrapper() {
        let v = parse_api_body(
            200,
            r#"{"code":0,"msg":"success","data":{"fileId":"9"}}"#,
            None,
            &[],
        )
        .unwrap();
        assert_eq!(data_or_self(&v)["fileId"], "9");
        assert!(parse_api_body(200, "<html>", None, &[]).is_err());
        let auth = parse_api_body(401, r#"{"msg":"expired"}"#, None, &[]).unwrap_err();
        assert!(auth.is_auth());
        // 字符串形式的 code 也认
        assert!(check_business(&json!({"code":"159"}), &[], None).is_err());
    }

    #[test]
    fn headers_reject_non_ascii() {
        assert!(headers(&[("x-test", "中文")]).is_err());
        assert!(headers(&[("x-test", "ok")]).is_ok());
    }

    #[tokio::test]
    async fn create_drive_offline_and_persist_device() {
        let dir = std::env::temp_dir().join(format!(
            "wp-gy-create-{}-{}",
            std::process::id(),
            wp_core::util::random_hex(4)
        ));
        let drive = create(DriveInit {
            id: "guangya-test".into(),
            name: String::new(),
            data_dir: dir.clone(),
            threads: 0,
            verbose: false,
        })
        .unwrap();
        assert_eq!(drive.kind(), DriveKind::Guangya);
        assert_eq!(drive.id(), "guangya-test");
        assert_eq!(drive.name(), "光鸭云盘");
        let caps = drive.caps();
        assert!(caps.auth.qr && caps.auth.sms && caps.auth.token && !caps.auth.cookie);
        assert!(caps.upload && caps.download && caps.share && caps.direct_link);
        assert!(!caps.rename && !caps.move_copy && !caps.search);
        // 新能力：回收站列表/清空、云添加/任务、上传记录可用；
        // 恢复/彻底删除/分享解析/转存/分类浏览无接口。
        assert!(caps.recycle && caps.recycle_list && caps.recycle_clear);
        assert!(!caps.recycle_restore && !caps.recycle_delete);
        assert!(caps.cloud_add && caps.cloud_tasks && caps.upload_records);
        assert!(!caps.share_info && !caps.share_save && !caps.categories);
        // 未登录时 account() 必须是 Auth，且不能发起网络请求。
        let err = drive.account().await.unwrap_err();
        assert!(err.is_auth());
        assert!(dir.join("device.json").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn fetch_thumbnail_without_field_is_readable() {
        let dir = std::env::temp_dir().join(format!(
            "wp-gy-thumb-{}-{}",
            std::process::id(),
            wp_core::util::random_hex(4)
        ));
        let drive = create(DriveInit {
            id: "guangya-thumb-test".into(),
            name: String::new(),
            data_dir: dir.clone(),
            threads: 0,
            verbose: false,
        })
        .unwrap();

        // 没有 thumbnail / 空白 thumbnail 都必须在发请求前返回可读错误。
        let mut entry = wp_core::model::RemoteEntry::file("a.jpg", "/a.jpg", 1);
        for bad in [None, Some(String::new()), Some("   ".into())] {
            entry.thumbnail = bad;
            let err = drive.fetch_thumbnail(&entry).await.unwrap_err();
            assert!(
                err.to_string().contains("该文件无缩略图"),
                "错误信息应可读：{err}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
