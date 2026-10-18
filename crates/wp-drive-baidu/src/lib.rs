//! 百度网盘适配器（Rust 重写版）。
//!
//! 协议依据：`docs/research/百度_协议.md`（Python 源项目只读分析）。
//! 关键约束：
//! * 认证载体是 Cookie：`BDUSS`(+`BDUSS_BFESS`) + `STOKEN`，另有会变化的
//!   `bdstoken`；每个业务请求都必须同时带 BDUSS 与 STOKEN，否则 `errno:-6`。
//! * 公共 query 为 `clienttype=0&app_id=250528&web=1&dp-logid=...`，
//!   写操作另带 `channel=chunlei` / `bdstoken`。
//! * `errno:-6` 只允许补一次 bdstoken 后重放；HTTP 401 不重放；
//!   连接失败重试 3 次（0.5/1/2s），读超时不重试。
//! * 上传走 rapidupload → precreate → superfile2 → create，MD5 有“纯净/展示态”
//!   两套表示；下载走 `filemetas?dlink=1` 取服务端签好的直链，必须带 Cookie。
//!
//! 对外唯一入口是 [`create`]。

pub mod session;

mod auth;
mod download;
mod files;
mod http;
mod share;
mod upload;

use crate::http::Http;
use crate::session::{QrSession, Session};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use wp_core::model::{
    AccountInfo, AuthCaps, DirectLink, DownloadOptions, DriveKind, OpReport, ProgressFn,
    RemoteEntry, UploadOptions,
};
use wp_core::util::now_ts;
use wp_core::{Error, Result};
use wp_drive::{Drive, DriveCaps, DriveInit, QrChallenge, QrPoll, ShareOptions};

/// 网盘业务域名。
pub(crate) const PAN: &str = "https://pan.baidu.com";
/// 登录/账号域名。
pub(crate) const PASSPORT: &str = "https://passport.baidu.com";
/// 上传接入点（locateupload）。
pub(crate) const PCS_FILE: &str = "https://d.pcs.baidu.com/rest/2.0/pcs/file";
/// 网盘 Web 固定 app id。
pub(crate) const APP_ID: &str = "250528";
/// 浏览器 UA（与抓包环境一致，见文档 §2.3）。
pub(crate) const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36";
/// 扫码登录总超时 / 二维码有效期（秒）。
pub(crate) const QR_TTL_SECS: u64 = 300;
/// 上传分片大小：4 MiB。
pub(crate) const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
/// 秒传 slice-md5 / data_content 取样长度：256 KiB。
pub(crate) const SLICE_SIZE: u64 = 256 * 1024;

/// 百度网盘驱动实例。
pub struct BaiduDrive {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) data_dir: PathBuf,
    /// 当前会话（只存一份，登录/刷新/登出都改它）。
    pub(crate) session: Mutex<Session>,
    /// 扫码登录临时会话（只存一份）。
    pub(crate) qr: Mutex<Option<QrSession>>,
    /// 已确认存在的远端目录，避免重复 stat/create。
    pub(crate) dir_cache: Mutex<HashSet<String>>,
    /// locateupload 域名缓存。
    pub(crate) domain: Mutex<Option<http::DomainCache>>,
    pub(crate) http: Http,
}

/// 取锁；即使之前有 panic 中毒也继续用（凭证结构简单，不会半更新）。
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl BaiduDrive {
    pub(crate) fn session_snapshot(&self) -> Session {
        lock(&self.session).clone()
    }

    pub(crate) fn session_valid(&self) -> bool {
        lock(&self.session).valid()
    }

    pub(crate) fn cookie_header(&self) -> String {
        lock(&self.session).cookie_header()
    }

    pub(crate) fn bdstoken_value(&self) -> String {
        lock(&self.session).bdstoken.clone()
    }

    pub(crate) fn uk_value(&self) -> Option<i64> {
        lock(&self.session).uk
    }

    /// 保存新会话（先落盘再替换内存；0600）。
    pub(crate) fn set_session(&self, s: Session) -> Result<()> {
        s.save(&self.data_dir)?;
        *lock(&self.session) = s;
        Ok(())
    }

    /// 修改会话并落盘。落盘失败只告警（内存仍然可用，下次刷新会再存）。
    pub(crate) fn mutate_session<F: FnOnce(&mut Session)>(&self, f: F) {
        let mut guard = lock(&self.session);
        f(&mut guard);
        guard.updated = now_ts();
        if let Err(e) = guard.save(&self.data_dir) {
            wp_core::log_warn!("百度", "会话落盘失败：{e}");
        }
    }

    pub(crate) fn cached_dir(&self, path: &str) -> bool {
        lock(&self.dir_cache).contains(path)
    }

    pub(crate) fn mark_dir(&self, path: &str) {
        lock(&self.dir_cache).insert(path.to_string());
    }

    pub(crate) fn clear_dir_cache(&self) {
        lock(&self.dir_cache).clear();
    }

    pub(crate) fn dp_next(&self) -> String {
        self.http.dp.next()
    }
}

/// 创建百度网盘驱动。凭证写在 `init.data_dir/凭证.json`。
pub fn create(init: DriveInit) -> Result<Arc<dyn Drive>> {
    if !init.data_dir.as_os_str().is_empty() {
        std::fs::create_dir_all(&init.data_dir).map_err(Error::Io)?;
    }
    let session = Session::load(&init.data_dir);
    let name = if init.name.trim().is_empty() {
        "百度网盘".to_string()
    } else {
        init.name.clone()
    };
    let http = Http::new()?;
    wp_core::log_info!(
        "百度",
        "初始化「{}」：{}；{}",
        name,
        if session.valid() {
            "已登录"
        } else {
            "未登录"
        },
        session.masked_summary()
    );
    Ok(Arc::new(BaiduDrive {
        id: init.id,
        name,
        data_dir: init.data_dir,
        session: Mutex::new(session),
        qr: Mutex::new(None),
        dir_cache: Mutex::new(HashSet::new()),
        domain: Mutex::new(None),
        http,
    }))
}

/// errno / 错误消息提取。
pub(crate) fn err_text(v: &Value, fallback: &str) -> String {
    for key in ["errmsg", "err_msg", "show_msg", "error_msg", "msg"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            if !s.trim().is_empty() {
                return s.trim().to_string();
            }
        }
    }
    fallback.to_string()
}

/// 百度 errno → 统一错误分类。
pub(crate) fn map_errno(code: i64, v: &Value) -> Error {
    let msg = err_text(v, "");
    match code {
        -6 => Error::auth(if msg.is_empty() {
            "未登录或登录已过期（errno:-6）".to_string()
        } else {
            format!("未登录或登录已过期（errno:-6）：{msg}")
        }),
        -9 => Error::not_found(if msg.is_empty() {
            "目录或文件不存在（errno:-9）".to_string()
        } else {
            format!("目录或文件不存在（errno:-9）：{msg}")
        }),
        132 => Error::server(
            132,
            if msg.is_empty() {
                "操作被风控拦截，需要人工完成短信验证（errno:132）".to_string()
            } else {
                format!("操作被风控拦截，需要人工完成短信验证（errno:132）：{msg}")
            },
        ),
        404 => Error::not_found(if msg.is_empty() {
            "未找到（errno:404）".to_string()
        } else {
            format!("未找到（errno:404）：{msg}")
        }),
        _ => {
            let text = if msg.is_empty() {
                format!("接口返回异常（errno:{code}）")
            } else {
                format!("{msg}（errno:{code}）")
            };
            Error::server(code, text)
        }
    }
}

/// 当前毫秒时间戳（passport/登录参数用）。
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[async_trait]
impl Drive for BaiduDrive {
    fn kind(&self) -> DriveKind {
        DriveKind::Baidu
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
                ..Default::default()
            },
            mkdir: true,
            rename: true,
            move_copy: true,
            // 百度 Web 有 /api/search；研究文档未覆盖，本实现按公开接口补上，
            // 具体见 files.rs 的注释（待真机确认）。
            search: true,
            share: true,
            direct_link: true,
            // Drive trait 没有回收站方法，故明确标记不支持；
            // 文档 §4.3 的回收站列表/还原只能等 trait 扩展后再暴露。
            recycle: false,
            upload: true,
            download: true,
        }
    }

    async fn account(&self) -> Result<AccountInfo> {
        auth::account(self).await
    }

    async fn auth_caps(&self) -> Result<AuthCaps> {
        Ok(AuthCaps {
            qr: true,
            cookie: true,
            ..Default::default()
        })
    }

    async fn begin_qr(&self) -> Result<QrChallenge> {
        auth::begin_qr(self).await
    }

    async fn poll_qr(&self) -> Result<QrPoll> {
        auth::poll_qr(self).await
    }

    async fn cancel_qr(&self) -> Result<()> {
        auth::cancel_qr(self);
        Ok(())
    }

    async fn login_cookie(&self, text: &str) -> Result<AccountInfo> {
        auth::login_cookie(self, text).await
    }

    async fn logout(&self) -> Result<()> {
        auth::logout(self)
    }

    async fn list(&self, path: &str) -> Result<Vec<RemoteEntry>> {
        files::list(self, path).await
    }

    async fn stat(&self, path: &str) -> Result<RemoteEntry> {
        files::stat(self, path).await
    }

    async fn ensure_dir(&self, path: &str) -> Result<RemoteEntry> {
        files::ensure_dir(self, path).await
    }

    async fn delete(&self, paths: &[String]) -> Result<OpReport> {
        files::delete(self, paths).await
    }

    async fn rename(&self, path: &str, new_name: &str) -> Result<RemoteEntry> {
        files::rename(self, path, new_name).await
    }

    async fn move_entries(&self, paths: &[String], dest_dir: &str) -> Result<OpReport> {
        files::move_or_copy(self, paths, dest_dir, false).await
    }

    async fn copy_entries(&self, paths: &[String], dest_dir: &str) -> Result<OpReport> {
        files::move_or_copy(self, paths, dest_dir, true).await
    }

    async fn search(&self, keyword: &str, dir: &str) -> Result<Vec<RemoteEntry>> {
        files::search(self, keyword, dir).await
    }

    async fn direct_link(&self, path: &str) -> Result<DirectLink> {
        download::direct_link(self, path).await
    }

    async fn download(
        &self,
        remote: &str,
        local: &Path,
        opts: &DownloadOptions,
        progress: ProgressFn,
    ) -> Result<u64> {
        download::download(self, remote, local, opts, progress).await
    }

    async fn upload(
        &self,
        local: &Path,
        remote_dir: &str,
        opts: &UploadOptions,
        progress: ProgressFn,
    ) -> Result<RemoteEntry> {
        upload::upload(self, local, remote_dir, opts, progress).await
    }

    async fn create_share(
        &self,
        paths: &[String],
        opts: &ShareOptions,
    ) -> Result<wp_core::model::ShareInfo> {
        share::create_share(self, paths, opts).await
    }

    async fn list_shares(&self) -> Result<Vec<wp_core::model::ShareInfo>> {
        share::list_shares(self).await
    }

    async fn cancel_shares(&self, ids: &[String]) -> Result<OpReport> {
        share::cancel_shares(self, ids).await
    }

    async fn write_probe(&self) -> Result<String> {
        auth::write_probe(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn errno_mapping() {
        let v = json!({"errno": -6, "errmsg": "未登录"});
        assert!(map_errno(-6, &v).is_auth());
        let v = json!({"errno": 132, "errmsg": "需要验证"});
        match map_errno(132, &v) {
            Error::Server { code, .. } => assert_eq!(code, 132),
            other => panic!("132 应映射为 Server，实际 {other:?}"),
        }
        assert!(matches!(map_errno(-9, &json!({})), Error::NotFound(_)));
        assert!(matches!(map_errno(404, &json!({})), Error::NotFound(_)));
        assert!(matches!(
            map_errno(2, &json!({"errmsg": "params error"})),
            Error::Server { code: 2, .. }
        ));
    }

    #[test]
    fn err_text_prefers_errmsg() {
        let v = json!({"errmsg": "a", "show_msg": "b"});
        assert_eq!(err_text(&v, "x"), "a");
        let v = json!({"show_msg": "b"});
        assert_eq!(err_text(&v, "x"), "b");
        assert_eq!(err_text(&json!({}), "x"), "x");
    }

    #[test]
    fn now_ms_is_millis() {
        let ms = now_ms();
        assert!(ms > 1_700_000_000_000, "应至少是 2023 年之后的毫秒时间戳");
    }
}
