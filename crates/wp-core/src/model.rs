//! 统一数据模型。
//!
//! 设计约定（沿用 V2 `v8_3/核心/模型.py` 的核心思想）：
//! * 所有网盘路径都用「统一逻辑路径」：以 `/` 开头的 POSIX 风格字符串，根目录是 `/`；
//!   各适配器负责把它翻译成自己的 fid / 路径参数。
//! * 所有结构体都可安全转 JSON（serde），供任务持久化、日志、界面使用。
//! * 字段名用英文（Rust/JSON 侧），界面文案在 GUI 层做中文映射。

use serde::{Deserialize, Serialize};

/// 网盘类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriveKind {
    Baidu,
    Quark,
    Guangya,
    /// 本机文件系统（用于「本机」浏览 / 跨盘对照 / 无账号演练）
    Local,
    /// 假网盘：内存样例数据，演示与测试用
    Fake,
}

impl DriveKind {
    pub const ALL: [DriveKind; 5] = [
        DriveKind::Baidu,
        DriveKind::Quark,
        DriveKind::Guangya,
        DriveKind::Local,
        DriveKind::Fake,
    ];

    /// 稳定标识串（配置 / 任务库里的 `类型` 字段）。
    pub fn slug(self) -> &'static str {
        match self {
            DriveKind::Baidu => "baidu",
            DriveKind::Quark => "quark",
            DriveKind::Guangya => "guangya",
            DriveKind::Local => "local",
            DriveKind::Fake => "fake",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DriveKind::Baidu => "百度网盘",
            DriveKind::Quark => "夸克网盘",
            DriveKind::Guangya => "光鸭云盘",
            DriveKind::Local => "本机",
            DriveKind::Fake => "演示网盘",
        }
    }

    /// 界面图标（与 V2 的 🅱 / 🅠 / 🦆 对应；egui 默认字体不含 emoji，GUI 层再换图形）
    pub fn icon(self) -> &'static str {
        match self {
            DriveKind::Baidu => "B",
            DriveKind::Quark => "Q",
            DriveKind::Guangya => "G",
            DriveKind::Local => "L",
            DriveKind::Fake => "D",
        }
    }

    /// 品牌色（顶栏导航按钮用）。
    pub fn brand_color(self) -> [u8; 3] {
        match self {
            DriveKind::Baidu => [41, 98, 255],
            DriveKind::Quark => [94, 78, 240],
            DriveKind::Guangya => [24, 168, 150],
            DriveKind::Local => [110, 120, 135],
            DriveKind::Fake => [180, 140, 60],
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|k| k.slug() == s)
    }

    /// 是否是需要登录的云盘（本机/演示不算）。
    pub fn is_cloud(self) -> bool {
        !matches!(self, DriveKind::Local | DriveKind::Fake)
    }
}

/// 网盘远端条目（文件 / 目录）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RemoteEntry {
    /// 网盘侧唯一 id（百度 fs_id / 夸克 fid / 光鸭 file_id；本机为路径）
    #[serde(default)]
    pub id: String,
    /// 显示名（不含路径）
    pub name: String,
    /// 统一逻辑路径（以 / 开头，根为 /）
    pub path: String,
    pub is_dir: bool,
    /// 字节数；目录为 0
    #[serde(default)]
    pub size: u64,
    /// 修改时间（Unix 秒）；未知为 None
    #[serde(default)]
    pub modified: Option<i64>,
    /// 创建时间（Unix 秒）
    #[serde(default)]
    pub created: Option<i64>,
    /// 原始响应（调试/字段缺失兜底用，不持久化到任务库）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

impl RemoteEntry {
    pub fn dir(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            is_dir: true,
            ..Default::default()
        }
    }

    pub fn file(name: impl Into<String>, path: impl Into<String>, size: u64) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            is_dir: false,
            size,
            ..Default::default()
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn with_modified(mut self, ts: i64) -> Self {
        self.modified = Some(ts);
        self
    }

    /// 大小展示文本（目录显示「—」）。
    pub fn size_text(&self) -> String {
        if self.is_dir {
            "—".to_string()
        } else {
            crate::util::format_size(self.size)
        }
    }

    /// 修改时间展示文本。
    pub fn modified_text(&self) -> String {
        match self.modified {
            Some(ts) => crate::util::format_time(ts),
            None => "—".to_string(),
        }
    }
}

/// 容量信息。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Quota {
    pub total: u64,
    pub used: u64,
    pub free: u64,
}

impl Quota {
    pub fn used_ratio(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.used as f64 / self.total as f64).clamp(0.0, 1.0) as f32
        }
    }
    /// 形如 `12.3 GB / 2.0 TB（已用 0.6%）`
    pub fn summary(&self) -> String {
        if self.total == 0 {
            return "容量未知".into();
        }
        format!(
            "{} / {}（已用 {:.1}%）",
            crate::util::format_size(self.used),
            crate::util::format_size(self.total),
            self.used_ratio() * 100.0
        )
    }
}

/// 账号信息。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AccountInfo {
    /// 展示用名称（昵称 / 用户名）
    pub display_name: String,
    /// 登录账号（手机号/邮箱，可能为空）
    pub user_name: String,
    /// 用户 id
    pub user_id: String,
    /// 会员/等级文本（如「超级会员 SVIP」；未知为空）
    pub vip: String,
    /// 容量
    pub quota: Option<Quota>,
    /// 原始响应里的其它有用字段
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl AccountInfo {
    pub fn is_logged_in(&self) -> bool {
        !self.display_name.is_empty() || !self.user_id.is_empty()
    }
}

/// 登录方式能力集（各适配器不同）。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthCaps {
    /// 扫码登录（服务端或本地生成二维码）
    pub qr: bool,
    /// 手动粘贴 Cookie / 导航栏 Cookie
    pub cookie: bool,
    /// 短信验证码登录
    pub sms: bool,
    /// 账号密码登录
    pub password: bool,
    /// Token 登录
    pub token: bool,
}

impl AuthCaps {
    pub fn qr_only() -> Self {
        Self {
            qr: true,
            ..Default::default()
        }
    }
    pub fn any(&self) -> bool {
        self.qr || self.cookie || self.sms || self.password || self.token
    }
    pub fn summary(&self) -> String {
        let mut v = Vec::new();
        if self.qr {
            v.push("扫码");
        }
        if self.cookie {
            v.push("Cookie");
        }
        if self.sms {
            v.push("短信");
        }
        if self.password {
            v.push("密码");
        }
        if self.token {
            v.push("Token");
        }
        if v.is_empty() {
            "无".into()
        } else {
            v.join(" / ")
        }
    }
}

/// 扫码登录状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum QrState {
    /// 等待扫码
    #[default]
    Waiting,
    /// 已扫码，等待手机端确认
    Scanned,
    /// 二维码已过期
    Expired,
    /// 登录成功
    Success,
    /// 取消
    Canceled,
}

impl QrState {
    pub fn label(self) -> &'static str {
        match self {
            QrState::Waiting => "等待扫码",
            QrState::Scanned => "已扫码，请在手机上确认",
            QrState::Expired => "二维码已过期",
            QrState::Success => "登录成功",
            QrState::Canceled => "已取消",
        }
    }
}

/// 下载直链信息。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DirectLink {
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub expire_at: Option<i64>,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub file_name: String,
}

/// 分享信息。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShareInfo {
    pub share_id: String,
    pub url: String,
    /// 提取码
    pub password: String,
    #[serde(default)]
    pub expire_at: Option<i64>,
    #[serde(default)]
    pub title: String,
}

/// 传输任务类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskKind {
    /// 本地上传到网盘
    Upload,
    /// 网盘下载到本地
    Download,
    /// 跨盘 / 同盘复制（A 下载到临时文件 → B 上传）
    CrossCopy,
}

impl TaskKind {
    pub fn label(self) -> &'static str {
        match self {
            TaskKind::Upload => "上传",
            TaskKind::Download => "下载",
            TaskKind::CrossCopy => "跨盘传输",
        }
    }
}

/// 任务状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Queued,
    Running,
    Paused,
    Done,
    Failed,
    Canceled,
}

impl TaskState {
    pub fn label(self) -> &'static str {
        match self {
            TaskState::Queued => "排队中",
            TaskState::Running => "进行中",
            TaskState::Paused => "已暂停",
            TaskState::Done => "已完成",
            TaskState::Failed => "失败",
            TaskState::Canceled => "已取消",
        }
    }
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            TaskState::Done | TaskState::Failed | TaskState::Canceled
        )
    }
    pub fn is_active(self) -> bool {
        matches!(self, TaskState::Running | TaskState::Queued)
    }
}

/// 传输任务（持久化到 `数据/任务.json`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferTask {
    pub id: String,
    pub kind: TaskKind,
    /// 源网盘标识（下载 / 跨盘传输用；上传时为空）
    #[serde(default)]
    pub src_drive: String,
    /// 目标网盘标识（上传 / 跨盘传输用；下载时为空）
    #[serde(default)]
    pub dst_drive: String,
    /// 源远端路径（下载/跨盘）
    #[serde(default)]
    pub src_path: String,
    /// 目标远端目录（上传/跨盘）
    #[serde(default)]
    pub dst_dir: String,
    /// 本地路径（上传=源文件；下载=目标文件；跨盘=临时文件）
    #[serde(default)]
    pub local_path: String,
    /// 展示用文件名
    pub file_name: String,
    /// 总字节数（0 = 未知）
    #[serde(default)]
    pub total: u64,
    /// 已完成字节数
    #[serde(default)]
    pub done: u64,
    pub state: TaskState,
    /// 最后错误文本
    #[serde(default)]
    pub error: String,
    /// 已重试次数
    #[serde(default)]
    pub retries: u32,
    /// 创建时间（Unix 秒）
    #[serde(default)]
    pub created: i64,
    /// 更新时间（Unix 秒）
    #[serde(default)]
    pub updated: i64,
    /// 运行时速率（字节/秒，不持久化）
    #[serde(skip)]
    pub speed: f64,
    /// 上传时的敏感词处理结果说明
    #[serde(default)]
    pub note: String,
}

impl TransferTask {
    pub fn progress(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f64 / self.total as f64).clamp(0.0, 1.0) as f32
        }
    }

    pub fn progress_text(&self) -> String {
        let pct = self.progress() * 100.0;
        if self.total == 0 {
            return format!("{pct:.0}%");
        }
        format!(
            "{:.0}%（{} / {}）",
            pct,
            crate::util::format_size(self.done),
            crate::util::format_size(self.total)
        )
    }

    pub fn speed_text(&self) -> String {
        if self.state == TaskState::Running && self.speed > 1.0 {
            format!("{}/s", crate::util::format_size(self.speed as u64))
        } else {
            "—".into()
        }
    }
}

/// 传输统计（传输页顶部卡片）。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TransferStats {
    pub active: usize,
    pub queued: usize,
    pub done: usize,
    pub failed: usize,
    /// 当前总速率（字节/秒）
    pub speed: f64,
    pub total_done_bytes: u64,
}

/// 上传选项。
#[derive(Debug, Clone, Default)]
pub struct UploadOptions {
    /// 远端目标文件名（None = 用本地文件名）
    pub rename_to: Option<String>,
    /// 分片大小（0 = 适配器默认）
    pub chunk_size: u64,
    /// 并发分片数（0 = 适配器默认）
    pub concurrency: usize,
    /// 上传前是否做敏感词检查（引擎层已做，适配器一般忽略）
    pub skip_guard: bool,
}

/// 下载选项。
#[derive(Debug, Clone)]
pub struct DownloadOptions {
    /// 下载到本地的文件路径（None = 自动拼下载目录）
    pub save_as: Option<std::path::PathBuf>,
    /// 分段并发数
    pub segments: usize,
    /// 分段阈值（超过才多段）
    pub segment_threshold: u64,
    /// 断点续传
    pub resume: bool,
    /// 限速（字节/秒，0 = 不限）
    pub limit_bps: u64,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            save_as: None,
            segments: 4,
            segment_threshold: 32 * 1024 * 1024,
            resume: true,
            limit_bps: 0,
        }
    }
}

/// 进度回调：`(已传输字节, 总字节)`；返回 false 表示请求取消。
pub type ProgressFn = Box<dyn FnMut(u64, u64) -> bool + Send>;

/// 操作结果（删除/移动等批量操作，部分成功时报告）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpReport {
    pub ok: usize,
    pub failed: usize,
    #[serde(default)]
    pub errors: Vec<String>,
}

impl OpReport {
    pub fn success(&mut self) {
        self.ok += 1;
    }
    pub fn fail(&mut self, why: impl Into<String>) {
        self.failed += 1;
        self.errors.push(why.into());
    }
    pub fn is_ok(&self) -> bool {
        self.failed == 0
    }
    pub fn summary(&self) -> String {
        if self.failed == 0 {
            format!("成功 {} 项", self.ok)
        } else {
            format!("成功 {} 项，失败 {} 项", self.ok, self.failed)
        }
    }
}
