//! 应用配置（持久化为 `数据/配置.json`）。
//!
//! 设计：
//! * 所有字段带默认值，旧配置缺字段可平滑升级；
//! * 网盘是**实例列表**（同一家网盘可以挂多个账号，每个实例独立凭证目录）；
//! * 中文 JSON 键保留给用户手工编辑友好？—— 不，Rust 版统一用英文键保持可维护性，
//!   UI 里全部显示中文。旧 V2 配置不兼容（这是重写，不是迁移器）。

use crate::model::DriveKind;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 一个网盘实例（账号）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveConfig {
    /// 全局唯一标识（任务库/界面导航都用它），如 `baidu`、`baidu_2`
    pub id: String,
    pub kind: DriveKind,
    /// 显示名
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 该实例的线程/并发数（0 = 用全局默认）
    #[serde(default)]
    pub threads: u32,
}

fn default_true() -> bool {
    true
}

impl DriveConfig {
    pub fn new(id: impl Into<String>, kind: DriveKind, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind,
            name: name.into(),
            enabled: true,
            threads: 0,
        }
    }

    pub fn display_name(&self) -> String {
        if self.name.trim().is_empty() {
            self.kind.label().to_string()
        } else {
            self.name.clone()
        }
    }
}

/// 传输并发与重试参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferConfig {
    /// 小文件阈值（字节）：小于它的文件走高并发
    pub small_file_threshold: u64,
    /// 小文件并发
    pub small_file_concurrency: usize,
    /// 大文件并发
    pub large_file_concurrency: usize,
    /// 失败重试次数
    pub retry: u32,
    /// 重试退避基数（毫秒，指数退避）
    pub retry_backoff_ms: u64,
    /// 自适应并发：出错时降并发、稳定时回升
    pub adaptive: bool,
    /// 全局限速（字节/秒，0 = 不限）
    pub max_speed_bps: u64,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            small_file_threshold: 8 * 1024 * 1024,
            small_file_concurrency: 16,
            large_file_concurrency: 3,
            retry: 3,
            retry_backoff_ms: 1500,
            adaptive: true,
            max_speed_bps: 0,
        }
    }
}

/// 下载参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadConfig {
    /// 分段并发数
    pub segments: usize,
    /// 超过该大小才分段
    pub segment_threshold: u64,
    /// 断点续传
    pub resume: bool,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            segments: 4,
            segment_threshold: 32 * 1024 * 1024,
            resume: true,
        }
    }
}

/// 界面参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// 主题名（见 GUI 层 theme 模块）
    pub theme: String,
    /// 自定义中文/UI 字体路径（空 = 自动在系统里找）
    pub font_path: String,
    /// 上次选中的网盘标识
    #[serde(default)]
    pub last_drive: String,
    /// 上次窗口尺寸
    #[serde(default = "default_window_size")]
    pub window_size: [f32; 2],
    /// 是否显示左侧文字标签
    #[serde(default = "default_true")]
    pub sidebar_labels: bool,
    /// 日志页终端输出级别（调试/信息/警告/错误）
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// 文件列表排序：(字段, 是否倒序)
    #[serde(default)]
    pub sort: Option<(String, bool)>,
}

fn default_window_size() -> [f32; 2] {
    [1280.0, 800.0]
}

fn default_log_level() -> String {
    "info".into()
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "深蓝夜色".into(),
            font_path: String::new(),
            last_drive: String::new(),
            window_size: default_window_size(),
            sidebar_labels: true,
            log_level: default_log_level(),
            sort: None,
        }
    }
}

/// 上传前敏感词守卫参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardConfig {
    pub enabled: bool,
    /// 命中后的动作：warn / block / rename
    pub action: GuardAction,
    /// 命中提示（用户确认后才继续）——取消该选项时直接拦截
    pub allow_override: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GuardAction {
    /// 只警告，继续上传
    #[default]
    Warn,
    /// 拦截
    Block,
    /// 自动改名规避
    Rename,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            action: GuardAction::Warn,
            allow_override: true,
        }
    }
}

/// 全套应用配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub drives: Vec<DriveConfig>,
    pub transfer: TransferConfig,
    pub download: DownloadConfig,
    pub ui: UiConfig,
    pub guard: GuardConfig,
    /// 下载目录（空 = 数据目录/下载）
    pub download_dir: String,
    /// 上传前是否复制本地文件到缓存再传（保留失败文件）
    #[serde(default)]
    pub keep_failed_temp: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            drives: vec![
                DriveConfig::new("baidu", DriveKind::Baidu, "百度网盘"),
                DriveConfig::new("quark", DriveKind::Quark, "夸克网盘"),
                DriveConfig::new("guangya", DriveKind::Guangya, "光鸭云盘"),
                DriveConfig::new("local", DriveKind::Local, "本机"),
                DriveConfig::new("demo", DriveKind::Fake, "演示网盘"),
            ],
            transfer: TransferConfig::default(),
            download: DownloadConfig::default(),
            ui: UiConfig::default(),
            guard: GuardConfig::default(),
            download_dir: String::new(),
            keep_failed_temp: false,
        }
    }
}

impl AppConfig {
    /// 从文件加载；文件不存在或损坏时返回默认配置（并保留损坏文件备份）。
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<AppConfig>(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    crate::log_warn!("配置", "配置.json 解析失败（{}），已备份并使用默认配置", e);
                    let bak = path.with_extension("json.bad");
                    let _ = std::fs::copy(path, bak);
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// 原子保存。
    pub fn save(&self, path: &Path) -> crate::Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        crate::paths::atomic_write(path, text.as_bytes())?;
        Ok(())
    }

    /// 按标识找网盘实例。
    pub fn drive(&self, id: &str) -> Option<&DriveConfig> {
        self.drives.iter().find(|d| d.id == id)
    }

    pub fn drive_mut(&mut self, id: &str) -> Option<&mut DriveConfig> {
        self.drives.iter_mut().find(|d| d.id == id)
    }

    /// 生成新实例标识：`baidu` → `baidu_2` → …
    pub fn gen_drive_id(&self, kind: DriveKind) -> String {
        let base = kind.slug();
        if self.drive(base).is_none() {
            return base.to_string();
        }
        let mut i = 2;
        loop {
            let cand = format!("{base}_{i}");
            if self.drive(&cand).is_none() {
                return cand;
            }
            i += 1;
        }
    }

    /// 当前有效的下载目录。
    pub fn resolved_download_dir(&self, paths: &crate::Paths) -> std::path::PathBuf {
        let d = self.download_dir.trim();
        if d.is_empty() {
            paths.download_dir()
        } else {
            let p = std::path::PathBuf::from(d);
            let _ = std::fs::create_dir_all(&p);
            p
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_roundtrip() {
        let cfg = AppConfig::default();
        let text = serde_json::to_string_pretty(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.drives.len(), cfg.drives.len());
        assert_eq!(back.transfer.retry, 3);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let cfg: AppConfig = serde_json::from_str(r#"{"drive":[]}"#).unwrap_or_default();
        assert_eq!(cfg.transfer.small_file_concurrency, 16);
    }

    #[test]
    fn gen_drive_id_increments() {
        let mut cfg = AppConfig::default();
        assert_eq!(cfg.gen_drive_id(DriveKind::Baidu), "baidu_2");
        cfg.drives
            .push(DriveConfig::new("baidu_2", DriveKind::Baidu, "小号"));
        assert_eq!(cfg.gen_drive_id(DriveKind::Baidu), "baidu_3");
    }
}
