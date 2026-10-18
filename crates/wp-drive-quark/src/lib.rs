//! 夸克网盘适配器（Cookie 认证 + clouddrive API + OSS 分片直传）。
//!
//! 对外唯一入口：[`create`]。
//!
//! 实现要点（与 Python 原项目对齐，并对已知坑做了 Rust 侧修正）：
//! * 身份完全由 Cookie 承载；必需 `__kps/__uid`，下载还需要 `__puus`；
//! * `__puus` 会滚动刷新，捕获后写回 `凭证.json`（0600 原子写）；
//! * 路径没有官方接口，逐级列目录解析成 fid，带 10 秒 TTL 缓存；
//! * `file/sort` 有秒级可见性延迟：空列表/查路径会重试；
//! * 删除**绝不**传 `current_dir_fid`，只用 `action_type=2 + filelist`；
//! * 上传 4 MiB 分片，`X-Oss-Hash-Ctx` 从第 2 片起强制携带；
//! * 合并 200/203 都算成功；OSS 日期用 locale 无关的 RFC1123；
//! * 下载必须带 Cookie/Referer，403 回调拒绝时刷新 `__puus` 重试一次。

mod auth;
mod download;
mod files;
mod recycle;
mod session;
mod share;
mod thumbnail;
mod upload;

use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wp_core::error::ErrorKind;
use wp_core::model::{
    AccountInfo, AuthCaps, DirectLink, DownloadOptions, DriveKind, OpReport, ProgressFn,
    RecycleEntry, RemoteEntry, ShareDetail, ShareInfo, UploadOptions,
};
use wp_core::{Error, Result};
use wp_drive::{Drive, DriveCaps, DriveInit, QrChallenge, QrPoll, ShareOptions};

use crate::session::Api;

// ───────────────────────── 协议常量 ─────────────────────────

pub(crate) const API_BASE: &str = "https://drive-pc.quark.cn/1/clouddrive";
pub(crate) const ACCOUNT_INFO_URL: &str = "https://pan.quark.cn/account/info";
pub(crate) const CAS_TOKEN_URL: &str = "https://uop.quark.cn/cas/ajax/getTokenForQrcodeLogin";
pub(crate) const CAS_STATUS_URL: &str =
    "https://uop.quark.cn/cas/ajax/getServiceTicketByQrcodeToken";
pub(crate) const QR_BASE_URL: &str = "https://su.quark.cn/4_eMHBJ";
pub(crate) const QR_BIZ: &str = "S:custom|OPT:SAREA@0|OPT:IMMERSIVE@1|OPT:BACK_BTN_STYLE@0";
pub(crate) const CAS_CLIENT_ID: &str = "532";
pub(crate) const CAS_VERSION: &str = "1.2";
pub(crate) const QR_TIMEOUT_SECS: u64 = 300;

pub(crate) const UA_QQ: &str = "Mozilla/5.0 (Windows NT 10.0; WOW64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/94.0.4606.71 Safari/537.36 Core/1.94.225.400 QQBrowser/12.2.5544.400";
pub(crate) const UA_LOGIN: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/139.0.0.0 Safari/537.36";
pub(crate) const OSS_UA: &str =
    "aliyun-sdk-js/1.0.0 Chrome Mobile 139.0.0.0 on Google Nexus 5 (Android 6.0)";
pub(crate) const OSS_DEFAULT_BUCKET: &str = "ul-zb";

/// 路径缓存 TTL（原 Python 版无 TTL，这里按产品要求 10 秒）。
pub(crate) const CACHE_TTL: Duration = Duration::from_secs(10);

// ───────────────────────── 驱动结构 ─────────────────────────

pub(crate) struct QuarkDrive {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) api: Api,
    pub(crate) login_http: reqwest::Client,
    pub(crate) oss_http: reqwest::Client,
    pub(crate) download_http: reqwest::Client,
    pub(crate) thumb_http: reqwest::Client,
    pub(crate) qr: Mutex<Option<auth::QrSession>>,
    pub(crate) cache: Mutex<files::PathCache>,
    pub(crate) dir_lock: tokio::sync::Mutex<()>,
}

/// 创建夸克网盘适配器。凭证从 `init.data_dir/凭证.json` 加载，缺失时按未登录处理。
pub fn create(init: DriveInit) -> Result<Arc<dyn Drive>> {
    std::fs::create_dir_all(&init.data_dir).map_err(Error::Io)?;
    let api = Api::new(init.data_dir.clone())?;
    if init.verbose {
        wp_core::log_debug!(
            "夸克",
            "创建适配器 id={} data_dir={}",
            init.id,
            init.data_dir.display()
        );
    }
    let login_http = build_login_client()?;
    let oss_http = build_oss_client()?;
    let download_http = build_download_client()?;
    let thumb_http = build_thumbnail_client()?;
    let id = if init.id.trim().is_empty() {
        "quark".into()
    } else {
        init.id
    };
    let name = if init.name.trim().is_empty() {
        "夸克网盘".into()
    } else {
        init.name
    };
    let drive = QuarkDrive {
        id,
        name,
        api,
        login_http,
        oss_http,
        download_http,
        thumb_http,
        qr: Mutex::new(None),
        cache: Mutex::new(files::PathCache::new(CACHE_TTL)),
        dir_lock: tokio::sync::Mutex::new(()),
    };
    Ok(Arc::new(drive))
}

// ───────────────────────── 客户端 ─────────────────────────

fn base_builder(timeout_secs: u64) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .http1_only()
}

pub(crate) fn build_api_client() -> Result<reqwest::Client> {
    base_builder(3600).build().map_err(wp_net::client::net_err)
}

pub(crate) fn build_login_client() -> Result<reqwest::Client> {
    base_builder(30).build().map_err(wp_net::client::net_err)
}

pub(crate) fn build_oss_client() -> Result<reqwest::Client> {
    base_builder(300).build().map_err(wp_net::client::net_err)
}

fn build_thumbnail_client() -> Result<reqwest::Client> {
    // 缩略图是短请求：整体 20 秒超时，允许 CDN 跳转，但绝不能拖住界面。
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(thumbnail::THUMBNAIL_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::limited(5))
        .no_proxy()
        .http1_only()
        .build()
        .map_err(wp_net::client::net_err)
}

pub(crate) fn build_download_client() -> Result<reqwest::Client> {
    // 不用 wp-net 的默认 builder：它会启用 cookie jar，而夸克下载需要手动
    // 注入完整 Cookie 头（含 __puus），避免 jar 与手写头重复发送。
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(3600))
        .redirect(reqwest::redirect::Policy::limited(10))
        .no_proxy()
        .http1_only()
        .build()
        .map_err(wp_net::client::net_err)
}

// ───────────────────────── 通用小工具 ─────────────────────────

pub(crate) fn now_ts() -> i64 {
    wp_core::util::now_ts()
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) fn as_i64(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().map(|x| x as i64))
            .or_else(|| n.as_f64().map(|x| x as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Bool(b) => Some(if *b { 1 } else { 0 }),
        _ => None,
    }
}

pub(crate) fn as_u64(v: Option<&Value>) -> Option<u64> {
    let n = as_i64(v)?;
    if n < 0 {
        None
    } else {
        Some(n as u64)
    }
}

pub(crate) fn as_str(v: Option<&Value>) -> Option<String> {
    let v = v?;
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

pub(crate) fn as_bool(v: Option<&Value>) -> Option<bool> {
    let v = v?;
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|x| x != 0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// 毫秒时间戳 → Unix 秒（兼容已经是秒的字段）。
pub(crate) fn parse_timestamp_secs(v: Option<&Value>) -> Option<i64> {
    let n = as_i64(v)?;
    if n <= 0 {
        return None;
    }
    if n > 100_000_000_000 {
        Some(n / 1000)
    } else {
        Some(n)
    }
}

pub(crate) fn compact_json(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into())
}

fn text_snippet(text: &str) -> String {
    let mut s: String = text.chars().take(240).collect();
    if text.chars().count() > 240 {
        s.push('…');
    }
    s.replace('\n', " ")
}

fn is_auth_message(lower: &str) -> bool {
    [
        "登录已过期",
        "重新登录",
        "cookie 无效",
        "cookie无效",
        "未登录",
        "登录失效",
        "auth miss",
        "auth not found",
        "require login",
        "unauthorized",
        "token expired",
        "invalid token",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

fn is_not_found_message(lower: &str) -> bool {
    [
        "not found",
        "不存在",
        "已失效",
        "share expired",
        "file not found",
        "路径不存在",
        "已被删除",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// 统一 URL 构造：拼接 base/path，再追加 query。
pub(crate) fn build_url(base: &str, path: &str, params: &[(String, String)]) -> Result<String> {
    let mut full = format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    if path.trim_matches('/').is_empty() {
        full = base.trim_end_matches('/').to_string();
    }
    let mut url =
        url::Url::parse(&full).map_err(|e| Error::other(format!("URL 解析失败（{full}）：{e}")))?;
    if !params.is_empty() {
        let mut q = url.query_pairs_mut();
        for (k, v) in params {
            q.append_pair(k, v);
        }
    }
    Ok(url.into())
}

/// 夸克业务状态判定：成功 ⇔ `status == 200`（或 "ok"/"success"）且 `code ∈ (0, None)`。
///
/// 注意参考实现只判 `status == "error"`，导致 `{"status":400}` 被当成功；
/// 这里显式要求 200。
pub(crate) fn check_business(v: &Value) -> Result<()> {
    let Some(obj) = v.as_object() else {
        return Ok(());
    };
    if let Some(raw) = obj.get("status").filter(|v| !v.is_null()) {
        let code = match raw {
            Value::String(s)
                if s.eq_ignore_ascii_case("ok") || s.eq_ignore_ascii_case("success") =>
            {
                200
            }
            _ => as_i64(Some(raw)).unwrap_or(-1),
        };
        if code != 200 {
            let msg = as_str(obj.get("message"))
                .or_else(|| as_str(obj.get("msg")))
                .unwrap_or_else(|| "接口返回异常".into());
            return Err(classify_business_error(code, &msg));
        }
    }
    if let Some(raw) = obj.get("code") {
        let code = as_i64(Some(raw)).unwrap_or(0);
        if code != 0 {
            let msg = as_str(obj.get("message"))
                .or_else(|| as_str(obj.get("msg")))
                .unwrap_or_else(|| "接口业务码异常".into());
            return Err(classify_business_error(code, &msg));
        }
    }
    Ok(())
}

fn classify_business_error(code: i64, msg: &str) -> Error {
    let lower = msg.to_lowercase();
    if is_auth_message(&lower) {
        return Error::auth(msg.to_string());
    }
    if is_not_found_message(&lower) {
        return Error::not_found(msg.to_string());
    }
    let text = if msg.is_empty() {
        format!("接口返回异常（code={code}）")
    } else {
        msg.to_string()
    };
    Error::server(code, text)
}

/// HTTP ≥400 的错误分类。夸克常用 HTTP 404/400 + JSON body 表达业务错误，
/// 因此先看 body 的 code/message，再看 HTTP 状态。
pub(crate) fn classify_http_error(status: u16, body: Option<&Value>, text: &str) -> Error {
    let obj = body.and_then(Value::as_object);
    let mut msg = obj
        .and_then(|o| o.get("message"))
        .and_then(|v| as_str(Some(v)))
        .or_else(|| obj.and_then(|o| o.get("msg")).and_then(|v| as_str(Some(v))))
        .unwrap_or_default();
    if msg.is_empty() {
        msg = text_snippet(text);
    }
    let code = obj
        .and_then(|o| o.get("code").or_else(|| o.get("status")))
        .and_then(|v| as_i64(Some(v)));
    let lower = msg.to_lowercase();

    if status == 401 || status == 403 {
        return Error::auth(if msg.is_empty() {
            format!("登录已过期或 Cookie 无效（HTTP {status}）")
        } else {
            format!("登录已过期或 Cookie 无效（HTTP {status}）：{msg}")
        });
    }
    if is_auth_message(&lower) {
        return Error::auth(msg);
    }
    if is_not_found_message(&lower) || (status == 404 && code.is_none()) {
        return Error::not_found(if msg.is_empty() {
            format!("HTTP {status}：资源不存在")
        } else {
            msg
        });
    }
    if status == 429 || (500..=599).contains(&status) {
        return Error::server(status as i64, msg);
    }
    if let Some(code) = code {
        return Error::server(code, msg);
    }
    Error::server(status as i64, msg)
}

/// 把响应 `data` 安全取出（缺失返回 Null）。
pub(crate) fn data_of(v: &Value) -> Value {
    v.get("data").cloned().unwrap_or(Value::Null)
}

pub(crate) fn is_retryable_not_found(e: &Error) -> bool {
    e.kind() == ErrorKind::NotFound
}

// ───────────────────────── Drive 实现 ─────────────────────────

#[async_trait]
impl Drive for QuarkDrive {
    fn kind(&self) -> DriveKind {
        DriveKind::Quark
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
                cookie: true,
                sms: false,
                password: false,
                token: false,
            },
            mkdir: true,
            rename: true,
            // 支持移动；复制没有官方接口，copy_entries 返回 Unsupported。
            move_copy: true,
            search: true,
            share: true,
            direct_link: true,
            recycle: true,
            upload: true,
            download: true,
            recycle_list: true,
            recycle_restore: true,
            recycle_delete: true,
            recycle_clear: true,
            // 夸克没有可用的上传记录/云添加/分类浏览接口，保持 Unsupported。
            cloud_add: false,
            cloud_tasks: false,
            upload_records: false,
            share_info: true,
            share_save: true,
            categories: false,
        }
    }

    async fn account(&self) -> Result<AccountInfo> {
        self.fetch_account().await
    }

    async fn begin_qr(&self) -> Result<QrChallenge> {
        self.begin_qr_login().await
    }

    async fn poll_qr(&self) -> Result<QrPoll> {
        self.poll_qr_login().await
    }

    async fn cancel_qr(&self) -> Result<()> {
        self.cancel_qr_login();
        Ok(())
    }

    async fn login_cookie(&self, text: &str) -> Result<AccountInfo> {
        self.login_with_cookie(text).await
    }

    async fn logout(&self) -> Result<()> {
        self.logout_account().await
    }

    async fn list(&self, path: &str) -> Result<Vec<RemoteEntry>> {
        self.list_entries(path).await
    }

    async fn stat(&self, path: &str) -> Result<RemoteEntry> {
        self.stat_entry(path).await
    }

    async fn ensure_dir(&self, path: &str) -> Result<RemoteEntry> {
        self.ensure_dir_entry(path).await
    }

    async fn delete(&self, paths: &[String]) -> Result<OpReport> {
        self.delete_paths(paths).await
    }

    async fn rename(&self, path: &str, new_name: &str) -> Result<RemoteEntry> {
        self.rename_path(path, new_name).await
    }

    async fn move_entries(&self, paths: &[String], dest_dir: &str) -> Result<OpReport> {
        self.move_paths(paths, dest_dir).await
    }

    async fn copy_entries(&self, _paths: &[String], _dest_dir: &str) -> Result<OpReport> {
        Err(Error::unsupported(
            "夸克网盘没有复制接口；可通过分享转存，但转存自己的分享会被服务端拒绝（41017）",
        ))
    }

    async fn search(&self, keyword: &str, dir: &str) -> Result<Vec<RemoteEntry>> {
        self.search_entries(keyword, dir).await
    }

    async fn direct_link(&self, path: &str) -> Result<DirectLink> {
        self.direct_link_for(path).await
    }

    async fn download(
        &self,
        remote: &str,
        local: &Path,
        opts: &DownloadOptions,
        progress: ProgressFn,
    ) -> Result<u64> {
        self.download_to_file(remote, local, opts, progress).await
    }

    async fn fetch_thumbnail(&self, entry: &RemoteEntry) -> Result<Vec<u8>> {
        self.fetch_thumbnail_impl(entry).await
    }

    async fn upload(
        &self,
        local: &Path,
        remote_dir: &str,
        opts: &UploadOptions,
        progress: ProgressFn,
    ) -> Result<RemoteEntry> {
        self.upload_file(local, remote_dir, opts, progress).await
    }

    async fn create_share(&self, paths: &[String], opts: &ShareOptions) -> Result<ShareInfo> {
        self.create_share_impl(paths, opts).await
    }

    async fn list_shares(&self) -> Result<Vec<ShareInfo>> {
        self.list_shares_impl().await
    }

    async fn cancel_shares(&self, ids: &[String]) -> Result<OpReport> {
        self.cancel_shares_impl(ids).await
    }

    // ───────────── 回收站 ─────────────

    async fn recycle_list(&self) -> Result<Vec<RecycleEntry>> {
        self.recycle_list_impl().await
    }

    async fn recycle_restore(&self, ids: &[String]) -> Result<OpReport> {
        self.recycle_restore_impl(ids).await
    }

    async fn recycle_delete(&self, ids: &[String]) -> Result<OpReport> {
        self.recycle_delete_impl(ids).await
    }

    async fn recycle_clear(&self) -> Result<OpReport> {
        self.recycle_clear_impl().await
    }

    // ───────────── 分享增强 ─────────────

    async fn share_info(&self, url: &str, password: &str) -> Result<ShareDetail> {
        self.share_info_impl(url, password).await
    }

    async fn save_shared(&self, url: &str, password: &str, dest_dir: &str) -> Result<OpReport> {
        self.save_shared_impl(url, password, dest_dir).await
    }
}

// 供业务模块拼 query 用，减少重复的 `to_string()`。
pub(crate) fn pair(k: &str, v: impl Into<String>) -> (String, String) {
    (k.to_string(), v.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(url: &str) -> Vec<(String, String)> {
        url::Url::parse(url)
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    #[test]
    fn url_build_with_fixed_query_and_chinese() {
        let params = vec![
            pair("pr", "ucpro"),
            pair("fr", "pc"),
            pair("uc_param_str", ""),
            pair("__dt", "1000"),
            pair("__t", "1700000000123"),
            pair("pdir_fid", "0"),
            pair("q", "中文 1"),
        ];
        let url = build_url(API_BASE, "file/sort", &params).unwrap();
        assert!(url.starts_with("https://drive-pc.quark.cn/1/clouddrive/file/sort?"));
        assert!(url.contains("q=%E4%B8%AD%E6%96%87+1") || url.contains("q=%E4%B8%AD%E6%96%87%201"));
        let pairs = q(&url);
        assert!(pairs.contains(&("pr".into(), "ucpro".into())));
        assert!(pairs.contains(&("fr".into(), "pc".into())));
        assert!(pairs.contains(&("__dt".into(), "1000".into())));
        assert!(pairs.contains(&("__t".into(), "1700000000123".into())));
        assert!(pairs.contains(&("q".into(), "中文 1".into())));

        // 两边斜杠都不会拼出 clouddrivefile。
        let url2 = build_url("https://drive.quark.cn/1/clouddrive/", "/share", &[]).unwrap();
        assert_eq!(url2, "https://drive.quark.cn/1/clouddrive/share");
    }

    #[test]
    fn business_status_rules() {
        assert!(check_business(&serde_json::json!({"status":200,"code":0})).is_ok());
        assert!(check_business(&serde_json::json!({"data":{}})).is_ok());
        assert!(check_business(&serde_json::json!({"status":"ok","code":0})).is_ok());

        let e = check_business(&serde_json::json!({"status":400,"message":"bad"})).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Server);
        assert!(matches!(e, Error::Server { code: 400, .. }));

        let e = check_business(&serde_json::json!({"status":200,"code":41017,"message":"禁止"}))
            .unwrap_err();
        assert!(matches!(e, Error::Server { code: 41017, .. }));

        let e =
            check_business(&serde_json::json!({"code":123,"message":"登录已过期"})).unwrap_err();
        assert!(e.is_auth());
    }

    #[test]
    fn http_error_classification() {
        let auth = classify_http_error(403, None, "forbidden");
        assert!(auth.is_auth());

        let not_found =
            classify_http_error(404, Some(&serde_json::json!({"message":"路径不存在"})), "");
        assert_eq!(not_found.kind(), ErrorKind::NotFound);

        let own_share = classify_http_error(
            404,
            Some(&serde_json::json!({"code":41017,"message":"用户禁止转存自己的分享"})),
            "",
        );
        assert!(matches!(own_share, Error::Server { code: 41017, .. }));

        let busy = classify_http_error(429, None, "too many requests");
        assert!(busy.retryable());

        let missing = classify_http_error(404, None, "");
        assert_eq!(missing.kind(), ErrorKind::NotFound);
    }

    #[test]
    fn timestamp_and_value_helpers() {
        let ms = serde_json::json!(1789322627000i64);
        assert_eq!(parse_timestamp_secs(Some(&ms)), Some(1789322627));
        let secs = serde_json::json!("1789322627");
        assert_eq!(parse_timestamp_secs(Some(&secs)), Some(1789322627));
        assert_eq!(parse_timestamp_secs(Some(&serde_json::json!(0))), None);
        assert_eq!(as_i64(Some(&serde_json::json!("12"))), Some(12));
        assert_eq!(as_u64(Some(&serde_json::json!(-1))), None);
        assert_eq!(as_bool(Some(&serde_json::json!("true"))), Some(true));
        assert_eq!(as_str(Some(&serde_json::json!(5))).as_deref(), Some("5"));
    }

    #[test]
    fn snippet_is_capped() {
        let s = "错".repeat(500);
        let e = classify_http_error(500, None, &s);
        let text = e.to_string();
        assert!(text.chars().count() <= 320);
    }

    #[tokio::test]
    async fn fetch_thumbnail_without_url_is_readable_error() {
        let init = DriveInit {
            id: "q-thumb".into(),
            name: "q-thumb".into(),
            data_dir: std::env::temp_dir()
                .join(format!("wp-quark-thumb-test-{}", std::process::id())),
            threads: 0,
            verbose: false,
        };
        let d = create(init).unwrap();
        let entry = RemoteEntry::file("a.jpg", "/a.jpg", 1);
        let err = d.fetch_thumbnail(&entry).await.unwrap_err();
        assert_eq!(err.to_string(), "该文件无缩略图");
        assert_eq!(err.kind(), ErrorKind::Other);
    }

    #[test]
    fn caps_advertises_real_features() {
        let init = DriveInit {
            id: "q".into(),
            name: "q".into(),
            data_dir: std::env::temp_dir().join("wp-quark-caps-test"),
            threads: 0,
            verbose: false,
        };
        let d = create(init).unwrap();
        let c = d.caps();
        assert!(c.auth.qr && c.auth.cookie);
        assert!(c.upload && c.download && c.share && c.direct_link && c.search);
        assert!(c.recycle);
        assert!(c.recycle_list && c.recycle_restore && c.recycle_delete && c.recycle_clear);
        assert!(c.share_info && c.share_save);
        assert!(!c.upload_records);
        assert!(!c.cloud_add && !c.cloud_tasks && !c.categories);
        assert!(d.id() == "q");
        assert_eq!(d.kind(), DriveKind::Quark);
        assert_eq!(d.name(), "q");
    }
}
