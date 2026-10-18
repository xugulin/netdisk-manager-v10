//! 网盘流代理：把 `DirectLink`（url+headers）包装成本地 HTTP 地址，
//! 并提供磁盘预读缓存、链接过期热换、Range 透传，为播放器隔离网络抖动。
//!
//! 任务 T12–T14 实现：`StreamProxy::start/open/stats/close`、`probe_range`。

use std::path::PathBuf;
use std::sync::Arc;
use wp_core::{Error, Result};
use wp_drive::Drive;

/// 代理配置。
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// 缓存目录（建议 `数据/播放器/缓存/stream`）。
    pub cache_dir: PathBuf,
    /// 预读窗口（字节，按码率自适应前先用固定值）。
    pub readahead_bytes: u64,
    /// 缓存上限（LRU 淘汰），0=不落盘。
    pub max_cache_bytes: u64,
    /// 分块大小。
    pub chunk_bytes: u64,
    /// 并行预取连接数（上游支持 Range 时）。
    pub parallel_fetches: usize,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            cache_dir: PathBuf::new(),
            readahead_bytes: 64 * 1024 * 1024,
            max_cache_bytes: 20 * 1024 * 1024 * 1024,
            chunk_bytes: 4 * 1024 * 1024,
            parallel_fetches: 2,
        }
    }
}

/// 打开后的流句柄（交给播放器）。
#[derive(Debug, Clone)]
pub struct StreamHandle {
    /// 本地 URL：http://127.0.0.1:<port>/s/<token>
    pub url: String,
    pub token: String,
    pub display: String,
    pub total_bytes: u64,
}

/// 流统计（UI 缓冲条/诊断面板）。
#[derive(Debug, Clone, Default)]
pub struct StreamStats {
    pub buffered_bytes: u64,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub bitrate_bps: f64,
    pub upstream_restarts: u64,
    pub errors: u64,
    pub complete: bool,
}

/// Range 探测结果（CLI `probe-range` 用；M0-E1）。
#[derive(Debug, Clone, Default)]
pub struct RangeProbe {
    pub status_head: Option<u16>,
    pub content_length: Option<u64>,
    pub accept_ranges: Option<String>,
    pub status_first: Option<u16>,
    pub first_ok: bool,
    pub status_mid: Option<u16>,
    pub mid_ok: bool,
    pub error: Option<String>,
}

/// 本地流代理。
pub struct StreamProxy {
    config: StreamConfig,
}

impl StreamProxy {
    pub fn new(config: StreamConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &StreamConfig {
        &self.config
    }

    /// 启动代理（绑定 127.0.0.1:0，返回实际端口）。
    pub async fn start(&self) -> Result<u16> {
        Err(Error::unsupported("StreamProxy 尚未实现（T12 任务）"))
    }

    /// 为指定网盘文件打开流（内部取 DirectLink、启动预读）。
    pub async fn open(&self, _drive: Arc<dyn Drive>, _remote_path: &str) -> Result<StreamHandle> {
        Err(Error::unsupported("StreamProxy::open 尚未实现（T12 任务）"))
    }

    pub fn stats(&self, _token: &str) -> Option<StreamStats> {
        None
    }

    pub fn close(&self, _token: &str) {}
}

/// 直接探测一个 DirectLink 的 Range/HEAD 能力（M0 实验 + CLI 验收）。
pub async fn probe_range(_drive: Arc<dyn Drive>, _remote_path: &str) -> Result<RangeProbe> {
    Err(Error::unsupported("probe_range 尚未实现（T12 任务）"))
}
