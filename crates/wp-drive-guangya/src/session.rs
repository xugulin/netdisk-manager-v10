//! 令牌会话：access/refresh token 持久化、提前刷新、单飞。
//!
//! 持久化格式（`data_dir/session.json`，serde JSON）：
//!
//! ```json
//! {"access_token":"...","refresh_token":"...","expires_at":1760000000.0,
//!  "sub":"...","saved_at":"2026-10-18 21:30:00"}
//! ```
//!
//! 与 Python 的行为差异（文档 §3.3/§8.3 建议项）：
//! * 提前 300 秒刷新；
//! * 刷新加异步单飞锁，401 时可主动 `force_refresh` 后重放一次；
//! * 不用中文键名（新数据目录没有历史包袱）。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use wp_core::{Error, Result};

use crate::{classify_http, snippet};

/// 提前刷新余量（秒）。
pub const REFRESH_MARGIN: f64 = 300.0;

/// 令牌数据。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Session {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    /// Unix 秒（浮点，保留服务端/本地累加的小数）。
    #[serde(default)]
    pub expires_at: f64,
    #[serde(default)]
    pub sub: String,
    #[serde(default)]
    pub saved_at: String,
}

impl Session {
    #[allow(dead_code)]
    pub fn needs_refresh(&self, now: f64, margin: f64) -> bool {
        self.access_token.is_empty() || now >= self.expires_at - margin
    }

    #[allow(dead_code)]
    pub fn is_expired(&self, now: f64) -> bool {
        self.access_token.is_empty() || now >= self.expires_at
    }
}

/// 会话仓库。
pub struct SessionStore {
    path: PathBuf,
    inner: Mutex<Option<Session>>,
    /// 刷新单飞锁：同一时刻只有一个任务向认证中心换令牌。
    refresh_lock: tokio::sync::Mutex<()>,
}

impl SessionStore {
    /// 从文件加载（文件不存在/损坏 → 未登录）。
    pub fn load(path: PathBuf) -> Self {
        let session = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Session>(&text).ok())
            .filter(|s| !s.access_token.trim().is_empty());
        Self {
            path,
            inner: Mutex::new(session),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn snapshot(&self) -> Option<Session> {
        self.lock().clone()
    }

    #[allow(dead_code)]
    pub fn has_login(&self) -> bool {
        self.lock()
            .as_ref()
            .map(|s| !s.access_token.is_empty())
            .unwrap_or(false)
    }

    pub fn sub(&self) -> String {
        self.lock()
            .as_ref()
            .map(|s| s.sub.clone())
            .unwrap_or_default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Session>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 登录成功后保存（空 refresh/sub 保留旧值）。
    pub fn save_login(
        &self,
        access_token: &str,
        expires_in: i64,
        refresh_token: Option<&str>,
        sub: Option<&str>,
    ) -> Result<()> {
        if access_token.trim().is_empty() {
            return Err(Error::auth("登录响应缺少 access_token"));
        }
        let mut session = self.lock().clone().unwrap_or_default();
        session.access_token = access_token.trim().to_string();
        session.expires_at = now_secs() + expires_in.max(0) as f64;
        if let Some(r) = refresh_token {
            if !r.trim().is_empty() {
                session.refresh_token = r.trim().to_string();
            }
        }
        if let Some(s) = sub {
            if !s.trim().is_empty() {
                session.sub = s.trim().to_string();
            }
        }
        session.saved_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        self.persist(&session)?;
        *self.lock() = Some(session);
        Ok(())
    }

    /// 退出登录：只清本地（Python 也没有服务端 revoke）。
    pub fn clear(&self) -> Result<()> {
        *self.lock() = None;
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// 取可用 access_token；必要时先刷新。
    pub async fn token(
        &self,
        client: &reqwest::Client,
        account_headers: &reqwest::header::HeaderMap,
    ) -> Result<String> {
        let snap = self
            .snapshot()
            .ok_or_else(|| Error::auth("光鸭云盘未登录，请先扫码或短信登录"))?;
        let now = now_secs();
        if !snap.access_token.is_empty() && snap.expires_at > now + REFRESH_MARGIN {
            return Ok(snap.access_token);
        }
        if snap.refresh_token.trim().is_empty() {
            // 没有 refresh_token 时，只要还没真正过期就先继续用。
            if !snap.access_token.is_empty() && snap.expires_at > now {
                return Ok(snap.access_token);
            }
            return Err(Error::auth("光鸭云盘登录已过期，请重新登录"));
        }
        self.refresh(client, account_headers, false).await
    }

    /// 401 自愈：强制换一次令牌（有 refresh_token 才可能成功）。
    pub async fn force_refresh(
        &self,
        client: &reqwest::Client,
        account_headers: &reqwest::header::HeaderMap,
    ) -> Result<String> {
        let snap = self
            .snapshot()
            .ok_or_else(|| Error::auth("光鸭云盘未登录，请先扫码或短信登录"))?;
        if snap.refresh_token.trim().is_empty() {
            return Err(Error::auth("登录已失效且没有 refresh_token，请重新登录"));
        }
        self.refresh(client, account_headers, true).await
    }

    async fn refresh(
        &self,
        client: &reqwest::Client,
        account_headers: &reqwest::header::HeaderMap,
        force: bool,
    ) -> Result<String> {
        let _guard = self.refresh_lock.lock().await;
        let snap = self
            .snapshot()
            .ok_or_else(|| Error::auth("光鸭云盘未登录，请先扫码或短信登录"))?;
        if !force && snap.expires_at > now_secs() + REFRESH_MARGIN {
            return Ok(snap.access_token);
        }
        if snap.refresh_token.trim().is_empty() {
            return Err(Error::auth("登录已失效且没有 refresh_token，请重新登录"));
        }

        let url = format!("{}/v1/auth/token", crate::ACCOUNT_BASE);
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": snap.refresh_token,
            "client_id": crate::CLIENT_ID,
        });
        let resp = client
            .post(url)
            .headers(account_headers.clone())
            .json(&body)
            .send()
            .await
            .map_err(wp_net::client::net_err)?;
        let status = resp.status().as_u16();
        let req_id = resp
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let text = resp.text().await.map_err(wp_net::client::net_err)?;
        if !(200..300).contains(&status) {
            // refresh_token 无效/过期属于认证错误，要引导用户重新登录；
            // 429/5xx 仍按可重试的服务端错误处理。
            if matches!(status, 400 | 401 | 403) {
                return Err(Error::auth(format!(
                    "刷新令牌失败（HTTP {status}）：{}",
                    snippet(&text, 240)
                )));
            }
            return Err(classify_http(status, &text, req_id.as_deref()));
        }
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| Error::server(status as i64, format!("刷新响应不是 JSON：{e}")))?;
        let data = value
            .get("data")
            .filter(|d| d.is_object())
            .unwrap_or(&value);
        let access = data
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if access.is_empty() {
            return Err(Error::auth("刷新令牌响应缺少 access_token，请重新登录"));
        }
        let expires_in = data
            .get("expires_in")
            .and_then(as_i64)
            .filter(|n| *n > 0)
            .unwrap_or(7200);
        let new_refresh = data.get("refresh_token").and_then(|v| v.as_str());
        let new_sub = data.get("sub").and_then(|v| v.as_str());
        self.save_login(&access, expires_in, new_refresh, new_sub)?;
        Ok(access)
    }

    fn persist(&self, session: &Session) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(session)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text.as_bytes())?;
        match std::fs::rename(&tmp, &self.path) {
            Ok(()) => Ok(()),
            Err(_) => {
                let _ = std::fs::remove_file(&tmp);
                std::fs::write(&self.path, text.as_bytes())?;
                Ok(())
            }
        }
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn as_i64(v: &serde_json::Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    if let Some(f) = v.as_f64() {
        return Some(f as i64);
    }
    v.as_str()?.trim().parse::<i64>().ok()
}

/// 从 JWT 里取 `exp`（秒）。令牌导入时用来避免“默认 2 小时”导致的错判。
pub fn jwt_exp(token: &str) -> Option<f64> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("exp").and_then(|v| v.as_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "wp-gy-session-{}-{}",
            std::process::id(),
            wp_core::util::random_hex(4)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn save_load_and_clear() {
        let dir = tmpdir();
        let path = dir.join("session.json");
        let store = SessionStore::load(path.clone());
        assert!(!store.has_login());
        store
            .save_login("access-A", 7200, Some("refresh-A"), Some("user-1"))
            .unwrap();
        assert!(store.has_login());

        let reloaded = SessionStore::load(path.clone());
        let snap = reloaded.snapshot().unwrap();
        assert_eq!(snap.access_token, "access-A");
        assert_eq!(snap.refresh_token, "refresh-A");
        assert_eq!(snap.sub, "user-1");
        assert!(snap.expires_at > now_secs() + 7000.0);

        reloaded.clear().unwrap();
        assert!(!reloaded.has_login());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn margin_and_expiry_logic() {
        let s = Session {
            access_token: "t".into(),
            refresh_token: "r".into(),
            expires_at: 1000.0,
            ..Default::default()
        };
        assert!(!s.needs_refresh(600.0, REFRESH_MARGIN));
        assert!(s.needs_refresh(700.0, REFRESH_MARGIN));
        assert!(!s.is_expired(999.0));
        assert!(s.is_expired(1000.0));
        let empty = Session::default();
        assert!(empty.needs_refresh(0.0, REFRESH_MARGIN));
        assert!(empty.is_expired(0.0));
    }

    #[test]
    fn jwt_exp_parsed() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"sub":"u1","exp":1700000000}"#);
        let token = format!("header.{payload}.signature");
        assert_eq!(jwt_exp(&token), Some(1_700_000_000.0));
        assert_eq!(jwt_exp("not-a-jwt"), None);
    }

    #[test]
    fn empty_access_rejected() {
        let dir = tmpdir();
        let store = SessionStore::load(dir.join("session.json"));
        assert!(store.save_login("   ", 7200, None, None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
