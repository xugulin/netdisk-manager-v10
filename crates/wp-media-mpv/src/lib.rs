//! libmpv 后端：运行时动态加载 + 安全封装（FFI 细节后续实现）。
//!
//! 设计：不链接系统 mpv 编译期库，而是用 `libloading` 在运行时加载
//! `libmpv.so.2` / `libmpv-2.dll`（优先程序目录 `播放器运行时/`）。
//! 这样可以做到「源码可控 + 用户可替换 + 无 mpv 环境可回退」。

use std::path::{Path, PathBuf};
use wp_core::{Error, Result};
use wp_media::{EventHandler, MediaEngine, MediaSource, PlaybackSnapshot, PlayerCommand};

/// 渲染模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    /// Mode A：进程内 GL render API（默认；Linux/Windows 统一）。
    EmbeddedGl,
    /// 外部播放窗口（兜底；非默认）。
    ExternalWindow,
}

/// MpvEngine 配置。
#[derive(Debug, Clone)]
pub struct MpvConfig {
    /// 指定 libmpv 路径；None=自动探测。
    pub lib_path: Option<PathBuf>,
    /// hwdec（auto-safe / vaapi / d3d11va / vulkan / no ...）。
    pub hwdec: String,
    /// 网络缓存秒数（mpv 内部，代理模式可小）。
    pub network_cache_secs: u64,
    /// 渲染模式。
    pub render_mode: RenderMode,
    /// 启动额外 mpv 选项（调试用）。
    pub extra_options: Vec<String>,
}

impl Default for MpvConfig {
    fn default() -> Self {
        Self {
            lib_path: None,
            hwdec: "auto-safe".into(),
            network_cache_secs: 10,
            render_mode: RenderMode::EmbeddedGl,
            extra_options: Vec::new(),
        }
    }
}

/// libmpv 动态库句柄（封装加载与版本探测）。
pub struct MpvLibrary {
    _lib: libloading::Library,
    pub version: String,
}

/// 动态加载 libmpv。
pub fn load_library(path: Option<&Path>) -> Result<MpvLibrary> {
    let _ = path;
    Err(Error::unsupported("libmpv 加载尚未实现（T1 任务）"))
}

/// 探测系统/随包的 libmpv 版本（找不到返回 None）。
pub fn probe_version() -> Option<String> {
    None
}

/// libmpv 播放引擎（TODO: T1–T3 实现）。
pub struct MpvEngine {
    _config: MpvConfig,
}

impl MpvEngine {
    pub fn new(config: MpvConfig) -> Result<Self> {
        Ok(Self { _config: config })
    }
}

impl MediaEngine for MpvEngine {
    fn open(&mut self, _source: MediaSource) -> Result<()> {
        Err(Error::unsupported("MpvEngine::open 尚未实现（T1 任务）"))
    }
    fn command(&mut self, _command: PlayerCommand) -> Result<()> {
        Err(Error::unsupported("MpvEngine::command 尚未实现（T1 任务）"))
    }
    fn snapshot(&self) -> PlaybackSnapshot {
        PlaybackSnapshot::default()
    }
    fn set_event_handler(&mut self, _handler: EventHandler) {}
    fn shutdown(&mut self) {}
}
