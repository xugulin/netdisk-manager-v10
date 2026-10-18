//! wp-drive —— 统一网盘适配器接口（trait）与内置驱动（本机 / 演示）。
//!
//! # 设计
//! V2 用「三家 Python 适配器各起一个子进程 + 行式 JSON 桥」是因为 Python 顶层
//! 包名冲突；Rust 没有这个问题，所以这里直接用 trait 对象：
//!
//! ```text
//! GUI / 传输引擎
//!   └── Arc<dyn Drive>
//!         ├── wp-drive-baidu   （百度网盘）
//!         ├── wp-drive-quark   （夸克网盘）
//!         ├── wp-drive-guangya （光鸭云盘）
//!         ├── local            （本机文件系统）
//!         └── fake             （演示用内存网盘，无账号可跑通 GUI）
//! ```
//!
//! # 约定
//! * 路径统一为「逻辑路径」：`/` 开头 POSIX 风格，根是 `/`；适配器内部自己转 fid。
//! * 所有方法都可以在 tokio 任务里并发调用；适配器内部用 `Mutex` 保护自己的会话。
//! * 未登录时 `account()` 返回 `Error::Auth`；GUI 据此弹登录框。
//! * 不支持的操作返回 `Error::Unsupported`（trait 默认实现已如此）。

pub mod fake;
pub mod local;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wp_core::model::{
    AccountInfo, AuthCaps, DirectLink, DownloadOptions, DriveKind, OpReport, ProgressFn, QrState,
    RemoteEntry, ShareInfo, UploadOptions,
};
use wp_core::{Error, Result};

/// 适配器能力集（GUI 据此禁用不支持的按钮）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DriveCaps {
    /// 登录方式
    pub auth: AuthCaps,
    /// 新建文件夹
    pub mkdir: bool,
    /// 重命名
    pub rename: bool,
    /// 移动 / 复制
    pub move_copy: bool,
    /// 搜索
    pub search: bool,
    /// 分享
    pub share: bool,
    /// 直链（播放/预览）
    pub direct_link: bool,
    /// 回收站
    pub recycle: bool,
    /// 上传 / 下载
    pub upload: bool,
    pub download: bool,
}

impl Default for DriveCaps {
    fn default() -> Self {
        Self {
            auth: AuthCaps::default(),
            mkdir: true,
            rename: true,
            move_copy: true,
            search: false,
            share: false,
            direct_link: false,
            recycle: false,
            upload: true,
            download: true,
        }
    }
}

impl DriveCaps {
    pub fn all_cloud() -> Self {
        Self {
            auth: AuthCaps {
                qr: true,
                cookie: true,
                sms: false,
                password: false,
                token: false,
            },
            mkdir: true,
            rename: true,
            move_copy: true,
            search: true,
            share: true,
            direct_link: true,
            recycle: true,
            upload: true,
            download: true,
        }
    }
}

/// 创建适配器实例的参数。
#[derive(Debug, Clone)]
pub struct DriveInit {
    /// 实例标识（如 `baidu` / `baidu_2`）
    pub id: String,
    /// 显示名
    pub name: String,
    /// 该实例的凭证/数据目录（适配器自己保证目录存在）
    pub data_dir: PathBuf,
    /// 并发/线程数（0 = 默认）
    pub threads: u32,
    /// 是否启用调试日志
    pub verbose: bool,
}

/// 扫码登录挑战。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QrChallenge {
    /// 二维码图片（PNG 字节）；服务端下发或适配器本地生成
    pub image_png: Option<Vec<u8>>,
    /// 无图片时给的链接（用户可手动打开）
    pub url: Option<String>,
    /// 设备码/用户码（部分登录方式）
    pub code: Option<String>,
    /// 给用户的提示
    pub hint: String,
    /// 有效期（秒）
    pub expires_in_secs: u64,
}

/// 扫码轮询结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QrPoll {
    pub state: QrState,
    pub message: String,
}

impl QrPoll {
    pub fn waiting(msg: impl Into<String>) -> Self {
        Self {
            state: QrState::Waiting,
            message: msg.into(),
        }
    }
    pub fn scanned(msg: impl Into<String>) -> Self {
        Self {
            state: QrState::Scanned,
            message: msg.into(),
        }
    }
    pub fn expired(msg: impl Into<String>) -> Self {
        Self {
            state: QrState::Expired,
            message: msg.into(),
        }
    }
    pub fn success(msg: impl Into<String>) -> Self {
        Self {
            state: QrState::Success,
            message: msg.into(),
        }
    }
}

/// 分享参数。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShareOptions {
    /// 有效期（天，0 = 永久/默认）
    pub expire_days: u32,
    /// 提取码（空 = 无/随机）
    pub password: Option<String>,
    /// 分享标题
    pub title: String,
}

/// 网盘适配器统一接口。
#[async_trait]
pub trait Drive: Send + Sync {
    fn kind(&self) -> DriveKind;
    /// 实例标识
    fn id(&self) -> &str;
    /// 显示名
    fn name(&self) -> &str;
    fn caps(&self) -> DriveCaps;

    // ───────────── 账号与登录 ─────────────
    /// 账号信息（含容量）。未登录返回 `Error::Auth`。
    async fn account(&self) -> Result<AccountInfo>;

    /// 登录能力（默认取 caps）。
    async fn auth_caps(&self) -> Result<AuthCaps> {
        Ok(self.caps().auth)
    }

    /// 开始扫码登录。
    async fn begin_qr(&self) -> Result<QrChallenge> {
        Err(Error::unsupported("该网盘不支持扫码登录"))
    }

    /// 轮询扫码状态（一次调用只查一次，GUI 定时循环）。
    async fn poll_qr(&self) -> Result<QrPoll> {
        Err(Error::unsupported("该网盘不支持扫码登录"))
    }

    /// 取消扫码会话。
    async fn cancel_qr(&self) -> Result<()> {
        Ok(())
    }

    /// 手动导入 Cookie / 令牌文本登录。
    async fn login_cookie(&self, _text: &str) -> Result<AccountInfo> {
        Err(Error::unsupported("该网盘不支持 Cookie 登录"))
    }

    /// 发送短信验证码，返回提示文本。
    async fn send_sms(&self, _phone: &str) -> Result<String> {
        Err(Error::unsupported("该网盘不支持短信登录"))
    }

    /// 校验短信验证码并登录。
    async fn verify_sms(&self, _phone: &str, _code: &str) -> Result<AccountInfo> {
        Err(Error::unsupported("该网盘不支持短信登录"))
    }

    /// 令牌登录（光鸭）。
    async fn login_token(&self, _token: &str, _extra: &str) -> Result<AccountInfo> {
        Err(Error::unsupported("该网盘不支持令牌登录"))
    }

    /// 退出登录：只清本地凭证，不动云端。
    async fn logout(&self) -> Result<()>;

    // ───────────── 浏览 ─────────────
    async fn list(&self, path: &str) -> Result<Vec<RemoteEntry>>;
    async fn stat(&self, path: &str) -> Result<RemoteEntry>;

    /// 递归创建目录。
    async fn ensure_dir(&self, path: &str) -> Result<RemoteEntry> {
        let _ = path;
        Err(Error::unsupported("该网盘不支持创建目录"))
    }

    /// 批量删除（文件/目录同一入口）。
    async fn delete(&self, paths: &[String]) -> Result<OpReport>;

    /// 重命名（同目录内改名）。
    async fn rename(&self, path: &str, new_name: &str) -> Result<RemoteEntry>;

    /// 批量移动到目标目录。
    async fn move_entries(&self, paths: &[String], dest_dir: &str) -> Result<OpReport>;

    /// 批量复制到目标目录。
    async fn copy_entries(&self, paths: &[String], dest_dir: &str) -> Result<OpReport>;

    /// 按关键字搜索（目录范围）。
    async fn search(&self, keyword: &str, dir: &str) -> Result<Vec<RemoteEntry>>;

    // ───────────── 传输 ─────────────
    /// 取下载直链（仅取链接，不下载）。
    async fn direct_link(&self, path: &str) -> Result<DirectLink>;

    /// 下载远端文件到本地 `local`（适配器负责断点续传与进度）。
    async fn download(
        &self,
        remote: &str,
        local: &Path,
        opts: &DownloadOptions,
        progress: ProgressFn,
    ) -> Result<u64>;

    /// 上传本地文件到远端目录。
    async fn upload(
        &self,
        local: &Path,
        remote_dir: &str,
        opts: &UploadOptions,
        progress: ProgressFn,
    ) -> Result<RemoteEntry>;

    // ───────────── 分享 ─────────────
    async fn create_share(&self, _paths: &[String], _opts: &ShareOptions) -> Result<ShareInfo> {
        Err(Error::unsupported("该网盘不支持分享"))
    }

    async fn list_shares(&self) -> Result<Vec<ShareInfo>> {
        Err(Error::unsupported("该网盘不支持分享"))
    }

    async fn cancel_shares(&self, _ids: &[String]) -> Result<OpReport> {
        Err(Error::unsupported("该网盘不支持分享"))
    }

    // ───────────── 其它 ─────────────
    /// 写权限探测（可选能力）。
    async fn write_probe(&self) -> Result<String> {
        Err(Error::unsupported("未实现写权限探测"))
    }
}

/// 适配器构造函数类型。
pub type DriveFactory = fn(DriveInit) -> Result<Arc<dyn Drive>>;

/// 判断是否已登录（通过 account()）。
pub async fn is_logged_in(drive: &dyn Drive) -> bool {
    drive.account().await.is_ok()
}
