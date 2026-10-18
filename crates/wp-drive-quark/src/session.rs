//! 会话、Cookie 凭证持久化与统一 HTTP 请求层。
//!
//! 夸克的身份完全由 Cookie 承载（没有 OAuth refresh token）：
//! * 必需字段：`__kps`、`__uid`；
//! * 下载额外需要：`__puus`（CAS 扫码不直接下发，要再请求一次 `member` 补）；
//! * 服务端会在响应里滚动刷新 `__puus`，这里捕获并**写回** `凭证.json`
//!   （Python 原实现只更新内存，重启后下载容易 403）。
//!
//! 凭证文件沿用 Python 项目的 schema：
//! `{schema, cookies, cookie_string, source, saved_at, expires_at}`，
//! 原子写入并显式收紧到 0600。

use crate::{
    as_i64, as_str, build_url, check_business, classify_http_error, now_ms, now_ts, API_BASE, UA_QQ,
};
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, COOKIE, ORIGIN, REFERER, SET_COOKIE};
use reqwest::{Method, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wp_core::{Error, Result};
use wp_net::CookieJar;

/// 固定凭证文件名（与 Python 版一致）。
pub const CRED_FILE: &str = "凭证.json";
/// 默认有效期：7 天。
pub const DEFAULT_TTL_SECS: i64 = 7 * 24 * 3600;
/// 关键 Cookie：只有这些参与过期时间计算（`ctoken` 仅约 10 分钟，不能算）。
pub const CRITICAL_KEYS: [&str; 5] = ["__pus", "__puus", "__kps", "__uid", "__ktd"];
/// 必需 Cookie。
pub const REQUIRED_KEYS: [&str; 2] = ["__kps", "__uid"];
/// 登录态判断的安全余量（秒）。
pub const SAFETY_MARGIN_SECS: i64 = 60;

/// 任务失败后不可重试的服务端关键字。
const NON_RETRYABLE: [&str; 7] = [
    "permission denied",
    "access denied",
    "forbidden",
    "unauthorized",
    "file not found",
    "share expired",
    "share not found",
];

/// 落盘的凭证结构（Python 兼容）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CredentialFile {
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub cookies: BTreeMap<String, String>,
    #[serde(default)]
    pub cookie_string: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub saved_at: i64,
    #[serde(default)]
    pub expires_at: i64,
}

/// 内存会话。
#[derive(Debug, Clone, Default)]
pub struct Session {
    pub jar: CookieJar,
    pub source: String,
    pub saved_at: i64,
    pub expires_at: i64,
    /// 兼容手工导入的凭证：没有 saved_at 时用文件 mtime 算 7 天。
    pub file_mtime: Option<i64>,
}

impl Session {
    pub fn from_file(cf: CredentialFile, file_mtime: Option<i64>) -> Self {
        let mut jar = CookieJar::new();
        for (k, v) in cf.cookies {
            if !k.is_empty() {
                jar.set(k, v);
            }
        }
        if jar.is_empty() && !cf.cookie_string.is_empty() {
            jar = CookieJar::parse(&cf.cookie_string);
        }
        Self {
            jar,
            source: cf.source,
            saved_at: cf.saved_at,
            expires_at: cf.expires_at,
            file_mtime,
        }
    }

    pub fn to_file(&self) -> CredentialFile {
        CredentialFile {
            schema: 1,
            cookies: self.jar.map.clone(),
            cookie_string: self.jar.header(),
            source: self.source.clone(),
            saved_at: self.saved_at,
            expires_at: self.expires_at,
        }
    }

    pub fn cookie_header(&self) -> String {
        self.jar.header()
    }

    /// 必需字段是否齐全（不做时效判断）。
    pub fn validate(&self) -> std::result::Result<(), String> {
        let missing: Vec<&str> = REQUIRED_KEYS
            .iter()
            .copied()
            .filter(|k| self.jar.get(k).map(|v| v.is_empty()).unwrap_or(true))
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(format!("缺少必需 Cookie 字段：{}", missing.join("、")))
        }
    }

    /// 是否仍在有效期内（安全余量 60 秒）。
    pub fn is_valid(&self) -> bool {
        if self.validate().is_err() {
            return false;
        }
        let now = now_ts();
        if self.expires_at > 0 {
            return now + SAFETY_MARGIN_SECS < self.expires_at;
        }
        let base = if self.saved_at > 0 {
            self.saved_at
        } else {
            self.file_mtime.unwrap_or(0)
        };
        base > 0 && now - base < DEFAULT_TTL_SECS
    }

    pub fn has_puus(&self) -> bool {
        self.jar
            .get("__puus")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    pub fn cookie_count(&self) -> usize {
        self.jar.len()
    }
}

/// 请求认证模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// 必须存在且本地未过期。
    Required,
    /// 只要求 Cookie 存在（用于 `__puus` 强制刷新）。
    AllowExpired,
}

/// 网络 + 会话，可克隆后跨任务并发使用。
#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    data_dir: Arc<PathBuf>,
    session: Arc<Mutex<Session>>,
    /// 临时会话（手动 Cookie 校验）只改内存，不写真实凭证文件。
    persist: bool,
}

impl Api {
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data_dir).map_err(Error::Io)?;
        let (session, _) = load_credentials(&data_dir);
        let http = crate::build_api_client()?;
        Ok(Self {
            http,
            data_dir: Arc::new(data_dir),
            session: Arc::new(Mutex::new(session)),
            persist: true,
        })
    }

    /// 用一份临时会话复制出 Api（用于验证手动 Cookie，不污染当前会话）。
    pub fn clone_with_session(&self, session: Session) -> Self {
        Self {
            http: self.http.clone(),
            data_dir: self.data_dir.clone(),
            session: Arc::new(Mutex::new(session)),
            persist: false,
        }
    }

    pub fn snapshot(&self) -> Session {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn cookie_header(&self) -> String {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cookie_header()
    }

    pub fn has_puus(&self) -> bool {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .has_puus()
    }

    /// 登录态检查：本地凭证存在、字段齐全且未过期。
    pub fn ensure_valid(&self) -> Result<()> {
        let st = self.session.lock().unwrap_or_else(|e| e.into_inner());
        st.validate().map_err(Error::auth)?;
        if st.is_valid() {
            Ok(())
        } else {
            Err(Error::auth(
                "夸克凭证已过期或无效，请重新扫码/粘贴 Cookie 登录",
            ))
        }
    }

    /// 整体替换会话（扫码登录成功时）。
    pub fn replace_credentials(&self, jar: CookieJar, source: &str, expires_at: i64) -> Result<()> {
        let now = now_ts();
        let session = Session {
            jar,
            source: source.to_string(),
            saved_at: now,
            expires_at: if expires_at > 0 {
                expires_at
            } else {
                now + DEFAULT_TTL_SECS
            },
            file_mtime: None,
        };
        session.validate().map_err(Error::auth)?;
        save_session(&self.data_dir, &session)?;
        *self.session.lock().unwrap_or_else(|e| e.into_inner()) = session;
        Ok(())
    }

    /// 退出登录：只删本地凭证。
    pub fn logout(&self) -> Result<()> {
        let path = self.data_dir.join(CRED_FILE);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Io(e)),
        }
        *self.session.lock().unwrap_or_else(|e| e.into_inner()) = Session::default();
        Ok(())
    }

    // ───────────────────────── 请求 ─────────────────────────

    pub async fn get(&self, path: &str, extra: Vec<(String, String)>) -> Result<Value> {
        self.request_inner(AuthMode::Required, API_BASE, Method::GET, path, extra, None)
            .await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.request_inner(
            AuthMode::Required,
            API_BASE,
            Method::POST,
            path,
            Vec::new(),
            Some(&body),
        )
        .await
    }

    /// `GET member`（账号 / 容量 / 会员）。`allow_expired` 供 `__puus` 刷新使用。
    pub async fn member(&self, allow_expired: bool) -> Result<Value> {
        let mode = if allow_expired {
            AuthMode::AllowExpired
        } else {
            AuthMode::Required
        };
        self.request_inner(mode, API_BASE, Method::GET, "member", Vec::new(), None)
            .await
    }

    /// 请求一次 `member` 以让服务端下发/刷新 `__puus`。错误只记日志。
    pub async fn refresh_puus(&self, force: bool) -> bool {
        if !force && self.has_puus() {
            return true;
        }
        let before = self.cookie_header().contains("__puus=");
        match self.member(true).await {
            Ok(_) => {}
            Err(e) => {
                wp_core::log_warn!("夸克", "刷新 __puus 失败：{}", wp_core::error::err_line(&e));
                return before;
            }
        }
        let has = self.has_puus();
        if has {
            wp_core::log_debug!("夸克", "已获取/刷新 __puus");
        }
        has
    }

    async fn request_inner(
        &self,
        mode: AuthMode,
        base: &str,
        method: Method,
        path: &str,
        extra: Vec<(String, String)>,
        body: Option<&Value>,
    ) -> Result<Value> {
        let mut params = vec![
            ("pr".to_string(), "ucpro".to_string()),
            ("fr".to_string(), "pc".to_string()),
            ("uc_param_str".to_string(), String::new()),
            ("__dt".to_string(), "1000".to_string()),
            ("__t".to_string(), now_ms().to_string()),
        ];
        params.extend(extra);
        let url = build_url(base, path, &params)?;

        let mut attempt = 0u32;
        loop {
            // 快照 Cookie，绝不把 MutexGuard 跨 await 持有。
            let cookie = match mode {
                AuthMode::Required => {
                    let st = self.session.lock().unwrap_or_else(|e| e.into_inner());
                    st.validate().map_err(Error::auth)?;
                    if !st.is_valid() {
                        return Err(Error::auth(
                            "夸克凭证已过期或无效，请重新扫码/粘贴 Cookie 登录",
                        ));
                    }
                    let c = st.cookie_header();
                    drop(st);
                    if c.is_empty() {
                        return Err(Error::auth("尚未登录夸克网盘"));
                    }
                    Some(c)
                }
                AuthMode::AllowExpired => {
                    let st = self.session.lock().unwrap_or_else(|e| e.into_inner());
                    st.validate().map_err(Error::auth)?;
                    let c = st.cookie_header();
                    drop(st);
                    if c.is_empty() {
                        return Err(Error::auth("尚未登录夸克网盘"));
                    }
                    Some(c)
                }
            };

            let mut rb = self
                .http
                .request(method.clone(), &url)
                .header(ACCEPT, "application/json, text/plain, */*")
                .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9")
                .header(ORIGIN, "https://pan.quark.cn")
                .header(REFERER, "https://pan.quark.cn/")
                .header(CONTENT_TYPE, "application/json")
                .header(reqwest::header::USER_AGENT, UA_QQ);
            if let Some(c) = cookie {
                rb = rb.header(COOKIE, c);
            }
            if let Some(b) = body {
                rb = rb.json(b);
            }

            match rb.send().await {
                Ok(resp) => {
                    // 先捕获 Set-Cookie（包含错误响应里的滚动 __puus）。
                    self.capture_set_cookies(&resp);
                    let status = resp.status().as_u16();
                    let text = resp.text().await.map_err(wp_net::client::net_err)?;
                    return Self::handle_response(status, &text);
                }
                Err(e) => {
                    // 与 Python 版一致：只重试「连接未建立」；读超时/5xx 不重试，
                    // 避免对建目录/删除/分享这类非幂等 POST 造成重复操作。
                    let retryable = e.is_connect();
                    let err = wp_net::client::net_err(e);
                    if retryable && attempt < 3 {
                        attempt += 1;
                        let wait = 500u64 * (1u64 << (attempt - 1));
                        wp_core::log_debug!(
                            "夸克",
                            "请求失败，{}ms 后重试（第 {} 次）：{}",
                            wait,
                            attempt,
                            wp_core::error::err_line(&err)
                        );
                        tokio::time::sleep(Duration::from_millis(wait)).await;
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }

    fn handle_response(status: u16, text: &str) -> Result<Value> {
        let parsed = serde_json::from_str::<Value>(text);
        if status >= 400 {
            let body = parsed.as_ref().ok();
            return Err(classify_http_error(status, body, text));
        }
        let value = parsed.map_err(|e| {
            let snippet: String = text.chars().take(200).collect();
            Error::other(format!("响应不是合法 JSON（{e}）：{snippet}"))
        })?;
        check_business(&value)?;
        Ok(value)
    }

    /// 捕获并持久化 `__puus` 等关键 Cookie；返回是否有变化。
    fn capture_set_cookies(&self, resp: &Response) -> bool {
        let mut st = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let old: Vec<Option<String>> = CRITICAL_KEYS
            .iter()
            .map(|k| st.jar.get(k).map(str::to_string))
            .collect();
        let mut min_expiry: Option<i64> = None;
        for hv in resp.headers().get_all(SET_COOKIE).iter() {
            let Ok(text) = hv.to_str() else { continue };
            st.jar.apply_set_cookie(text);
            if let Some(name) = set_cookie_name(text) {
                if CRITICAL_KEYS.contains(&name.as_str()) {
                    if let Some(exp) = parse_cookie_expiry(text) {
                        min_expiry = Some(min_expiry.map_or(exp, |m| m.min(exp)));
                    }
                }
            }
        }
        let new: Vec<Option<String>> = CRITICAL_KEYS
            .iter()
            .map(|k| st.jar.get(k).map(str::to_string))
            .collect();
        if old == new {
            return false;
        }
        let now = now_ts();
        st.saved_at = now;
        if let Some(exp) = min_expiry {
            st.expires_at = if st.expires_at > 0 {
                st.expires_at.min(exp)
            } else {
                exp
            };
        } else if st.expires_at <= now {
            st.expires_at = now + DEFAULT_TTL_SECS;
        }
        if !self.persist {
            return true;
        }
        let file = st.to_file();
        drop(st);
        if let Err(e) = save_credential_file(&self.data_dir, &file) {
            wp_core::log_warn!(
                "夸克",
                "刷新 Cookie 后写回凭证失败：{}",
                wp_core::error::err_line(&e)
            );
        } else {
            wp_core::log_debug!("夸克", "已写回刷新的 Cookie");
        }
        true
    }

    // ───────────────────────── 任务 ─────────────────────────

    /// 轮询 `GET task` 直到完成。`timeout` 默认 60 秒。
    pub async fn wait_task(&self, task_id: &str, description: &str) -> Result<Value> {
        self.wait_task_timeout(task_id, description, Duration::from_secs(60))
            .await
    }

    pub async fn wait_task_timeout(
        &self,
        task_id: &str,
        description: &str,
        timeout: Duration,
    ) -> Result<Value> {
        if task_id.is_empty() {
            return Err(Error::other("task_id 为空，无法等待任务"));
        }
        let started = Instant::now();
        let mut retry_index = 0i64;
        loop {
            if started.elapsed() >= timeout {
                return Err(Error::other(format!(
                    "{description}超时（{} 秒，轮询 {retry_index} 次，task_id={task_id}）",
                    timeout.as_secs()
                )));
            }
            let params = vec![
                ("task_id".to_string(), task_id.to_string()),
                ("retry_index".to_string(), retry_index.to_string()),
            ];
            let resp = self.get("task", params).await?;
            retry_index += 1;
            let data = resp.get("data").cloned().unwrap_or(Value::Null);
            let status = as_i64(data.get("status")).unwrap_or(-1);
            let message = as_str(data.get("message")).unwrap_or_default();

            match status {
                2 => return Ok(data),
                3 => {
                    let detail = if message.is_empty() {
                        crate::compact_json(&data)
                    } else {
                        message
                    };
                    return Err(Error::server(3, format!("{description}失败：{detail}")));
                }
                _ => {}
            }

            if !message.is_empty() {
                let lower = message.to_lowercase();
                if lower.contains("capacity limit")
                    || message.contains("容量不足")
                    || NON_RETRYABLE.iter().any(|k| lower.contains(k))
                {
                    return Err(Error::server(
                        status,
                        format!("{description}失败（不可重试）：{message}"),
                    ));
                }
            }

            let gap = as_i64(data.get("tq_gap"))
                .map(|ms| (ms as f64 / 1000.0).clamp(0.2, 5.0))
                .unwrap_or(1.0);
            tokio::time::sleep(Duration::from_secs_f64(gap)).await;
        }
    }
}

/// 从目录加载凭证；不存在或损坏时返回空会话。
pub fn load_credentials(data_dir: &Path) -> (Session, Option<i64>) {
    let path = data_dir.join(CRED_FILE);
    let mtime = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    let Some(mtime) = mtime else {
        return (Session::default(), None);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            wp_core::log_warn!("夸克", "读取凭证失败：{e}");
            return (Session::default(), Some(mtime));
        }
    };
    match serde_json::from_str::<CredentialFile>(&text) {
        Ok(cf) => {
            // 顺手收紧旧文件的权限。
            restrict_permissions(&path);
            (Session::from_file(cf, Some(mtime)), Some(mtime))
        }
        Err(e) => {
            wp_core::log_warn!("夸克", "凭证文件不是合法 JSON，按未登录处理：{e}");
            (Session::default(), Some(mtime))
        }
    }
}

/// 保存会话（原子写 + 0600）。
fn save_session(data_dir: &Path, session: &Session) -> Result<()> {
    save_credential_file(data_dir, &session.to_file())
}

fn save_credential_file(data_dir: &Path, cf: &CredentialFile) -> Result<()> {
    let path = data_dir.join(CRED_FILE);
    let text = serde_json::to_string_pretty(cf)?;
    write_private(&path, text.as_bytes()).map_err(Error::Io)
}

/// 原子写文件，并在 Unix 上强制 0600（防止 umask 022 退化成 0644）。
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.flush()?;
        f.sync_all()?;
    }
    #[cfg(windows)]
    {
        let _ = std::fs::remove_file(path);
    }
    std::fs::rename(&tmp, path)?;
    restrict_permissions(path);
    Ok(())
}

fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// 取 Set-Cookie 的 Cookie 名。
pub fn set_cookie_name(text: &str) -> Option<String> {
    let first = text.split(';').next()?.trim();
    let (name, _) = first.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// 解析 Set-Cookie 的过期时间（Expires / Max-Age）。
pub fn parse_cookie_expiry(text: &str) -> Option<i64> {
    for part in text.split(';').skip(1) {
        let part = part.trim();
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.eq_ignore_ascii_case("expires") {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(value.trim()) {
                return Some(dt.timestamp());
            }
        } else if name.eq_ignore_ascii_case("max-age") {
            if let Ok(secs) = value.trim().parse::<i64>() {
                return Some(now_ts() + secs);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wp-quark-test-{}-{}-{tag}",
            std::process::id(),
            now_ts()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn credential_roundtrip_and_validity() {
        let mut jar = CookieJar::new();
        jar.set("__kps", "abc");
        jar.set("__uid", "xyz");
        jar.set("__puus", "p");
        let now = now_ts();
        let s = Session {
            jar,
            source: "manual".into(),
            saved_at: now,
            expires_at: now + 3600,
            file_mtime: None,
        };
        assert!(s.is_valid());
        assert!(s.has_puus());
        let cf = s.to_file();
        assert_eq!(cf.schema, 1);
        assert!(cf.cookie_string.contains("__kps=abc"));
        let back = Session::from_file(cf, None);
        assert_eq!(back.cookie_header(), s.cookie_header());
        assert_eq!(back.source, "manual");

        let mut bad = s.clone();
        bad.jar.remove("__uid");
        assert!(!bad.is_valid());
        assert!(bad.validate().is_err());

        let mut expired = s;
        expired.expires_at = now - 1;
        assert!(!expired.is_valid());

        let mut no_expiry = back;
        no_expiry.expires_at = 0;
        no_expiry.saved_at = now - 8 * 24 * 3600;
        assert!(!no_expiry.is_valid());
    }

    #[test]
    fn cookie_expiry_and_name() {
        let exp = parse_cookie_expiry(
            "__puus=abc; Path=/; Domain=pan.quark.cn; Expires=Wed, 21 Oct 2026 07:28:00 GMT; HttpOnly",
        );
        assert_eq!(
            exp,
            Some(
                chrono::DateTime::parse_from_rfc2822("Wed, 21 Oct 2026 07:28:00 GMT")
                    .unwrap()
                    .timestamp()
            )
        );
        let before = now_ts();
        let exp = parse_cookie_expiry("ctoken=x; Max-Age=600").unwrap();
        assert!(exp >= before + 590 && exp <= before + 610);
        assert_eq!(
            set_cookie_name("__kps=xyz; Path=/").as_deref(),
            Some("__kps")
        );
        assert_eq!(set_cookie_name(""), None);
    }

    #[test]
    fn private_save_load_and_permission() {
        let dir = tmp_dir("cred");
        let mut jar = CookieJar::new();
        jar.set("__kps", "k");
        jar.set("__uid", "u");
        jar.set("__puus", "p");
        let now = now_ts();
        let s = Session {
            jar,
            source: "qrcode".into(),
            saved_at: now,
            expires_at: now + 3600,
            file_mtime: None,
        };
        save_session(&dir, &s).unwrap();
        let (loaded, mtime) = load_credentials(&dir);
        assert!(mtime.is_some());
        assert!(loaded.is_valid());
        assert_eq!(loaded.source, "qrcode");
        assert_eq!(loaded.cookie_header(), s.cookie_header());
        let raw = std::fs::read_to_string(dir.join(CRED_FILE)).unwrap();
        assert!(raw.contains("\"schema\": 1"));
        assert!(raw.contains("\"cookie_string\""));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(CRED_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "凭证文件必须是 0600");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
