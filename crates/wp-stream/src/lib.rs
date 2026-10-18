//! 网盘流代理：把 `DirectLink`（url + headers）包装成本地 HTTP 地址，
//! 并提供磁盘预读缓存、链接过期热换、Range 透传，为播放器隔离网络抖动。
//!
//! 任务 T12–T14 实现：
//! * [`StreamProxy::start`]：启动本地 tokio HTTP 服务（`127.0.0.1:0` + `/s/<token>`）；
//! * [`StreamProxy::open`]：取 `Drive::direct_link` → 建缓存 → 返回可播 URL；
//! * [`StreamProxy::stats`] / [`StreamProxy::close`]：缓冲/吞吐/换链统计与关闭；
//! * [`probe_range`]：M0-E1 真机 Range 能力探测（CLI `probe-range`）。
//!
//! 典型用法：
//! ```no_run
//! # async fn demo(drive: std::sync::Arc<dyn wp_drive::Drive>) -> wp_core::Result<()> {
//! use wp_stream::{StreamConfig, StreamProxy};
//! let proxy = StreamProxy::new(StreamConfig::default());
//! let handle = proxy.open(drive, "/电影/示例.mp4").await?;
//! println!("播放地址：{}", handle.url); // 交给 libmpv / VLC
//! println!("{:?}", proxy.stats(&handle.token));
//! proxy.close(&handle.token);
//! # Ok(()) }
//! ```
//!
//! 实现分层：
//! * [`range`]：HTTP Range 解析（单段）；
//! * `cache`：磁盘分块缓存 + 同块去重 + LRU；
//! * `upstream`：直链注入头、200/206 处理、错误分类；
//! * `proxy`：本地 HTTP 服务、预读、换链；
//! * `probe`：Range 能力探测。

mod cache;
pub mod playback;
mod probe;
mod proxy;
pub mod range;
mod upstream;

use std::path::PathBuf;
use std::sync::Arc;

use wp_core::model::DirectLink;
use wp_core::{Error, Result};
use wp_drive::Drive;

pub use cache::{cache_key, CacheConfig, CacheEntry, CacheManager, ChunkState};
pub use playback::{resolve_playback, resolve_playback_with_provider, PlaybackSource};
pub use probe::{probe_range, probe_with_provider, ProbeStep, RangeProbe};
pub use upstream::{
    DriveLinkProvider, LinkProvider, RawResponse, TransportError, UpstreamClient, UpstreamConfig,
};

/// 代理配置。
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// 缓存目录（建议 `数据/播放器/缓存/stream`；空 = 系统临时目录）。
    pub cache_dir: PathBuf,
    /// 预读窗口（字节，按码率自适应前先用固定值）。0 = 关闭后台预读。
    pub readahead_bytes: u64,
    /// 缓存上限（LRU 淘汰），0 = 不限制。
    pub max_cache_bytes: u64,
    /// 分块大小。
    pub chunk_bytes: u64,
    /// 并行预取连接数（上游支持 Range 时）。
    pub parallel_fetches: usize,
    /// 单块最多换链重试次数（T14：默认 3）。
    pub max_link_restarts: usize,
    /// 单次上游请求读超时（秒）。
    pub request_timeout_secs: u64,
    /// 上游建连超时（秒）。
    pub connect_timeout_secs: u64,
    /// 弱网自适应分块/首字节（task-18，默认开）：
    /// 冷读**先回响应头**，再用 `startup_first_bytes` 小块直发首字节（不必等整个 4MB 块），
    /// 之后按 `chunk_bytes` 稳定分块；实测带宽低于 `adaptive_weak_mbps` 时自动降级预读窗口。
    /// `false` = 旧语义（首块就绪才回响应头）。
    pub adaptive_chunk: bool,
    /// 首字节引导块大小（默认 1MB；0 = 关闭引导，等价旧行为）。
    pub startup_first_bytes: u64,
    /// seek 优先（默认开）：客户端跳转时暂停后台预读，把带宽让给 seek 目标块。
    pub seek_priority: bool,
    /// seek 后暂停后台预读的时长（毫秒，默认 400；弱网自动 ×2）。
    pub seek_pause_ms: u64,
    /// 观测带宽低于该值（Mbps）判为弱网（默认 20.0）。
    pub adaptive_weak_mbps: f64,
    /// 弱网时本会话预读窗口上限（默认 32MB）。
    pub weak_readahead_bytes: u64,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            cache_dir: PathBuf::new(),
            // 4K（40–100Mbps）预读 ≈8.5–25s 缓冲；弱网/移动端建议手动降到 32–64MB
            // （见 docs/PLAYER_PERF.md §4，预读过大会与 seek 抢带宽，自适应优化见 task-18）。
            readahead_bytes: 128 * 1024 * 1024,
            max_cache_bytes: 20 * 1024 * 1024 * 1024,
            chunk_bytes: 4 * 1024 * 1024,
            // 多连接预取聚合 2–5×（PLAYER_PERF 实测 parallel=2 聚合 ≈21.8Mbps）
            parallel_fetches: 4,
            max_link_restarts: 3,
            request_timeout_secs: 30,
            connect_timeout_secs: 10,
            // task-18 弱网自适应：首字节 1MB 小块先流式回给播放器，再按 4MB 稳定分块；
            // 弱网（<20Mbps）自动把预读窗口降到 32MB，减少与 seek 抢带宽。
            adaptive_chunk: true,
            startup_first_bytes: 1024 * 1024,
            seek_priority: true,
            seek_pause_ms: 400,
            adaptive_weak_mbps: 20.0,
            weak_readahead_bytes: 32 * 1024 * 1024,
        }
    }
}

/// 打开后的流句柄（交给播放器）。
#[derive(Debug, Clone)]
pub struct StreamHandle {
    /// 播放地址：普通文件是本地 URL `http://127.0.0.1:<port>/s/<token>`；
    /// HLS 直连时是原始 m3u8 URL（此时 `direct == true`、`token` 为空）。
    pub url: String,
    pub token: String,
    pub display: String,
    pub total_bytes: u64,
    /// 是否「直连播放」而非本地代理（HLS/m3u8 流专用）：
    /// `true` 时播放器应直接 `loadfile url` 并把 `headers` 注入其 HTTP 请求
    /// （mpv `--http-header-fields`）；此时 [`StreamProxy::stats`] 返回 `None`（无代理会话）。
    pub direct: bool,
    /// 直连播放需要注入的 HTTP 头（HLS 常见是 Cookie/Referer/UA）；普通代理流为空。
    pub headers: Vec<(String, String)>,
}

/// 流统计（UI 缓冲条/诊断面板）。
#[derive(Debug, Clone, Default)]
pub struct StreamStats {
    /// 已缓存字节数。
    pub buffered_bytes: u64,
    pub total_bytes: u64,
    /// 累计从上游下载字节。
    pub downloaded_bytes: u64,
    /// 平均下载码率（bit/s）。
    pub bitrate_bps: f64,
    /// 链接热换次数。
    pub upstream_restarts: u64,
    /// 错误次数。
    pub errors: u64,
    /// 是否整个文件都已缓存。
    pub complete: bool,
    /// 是否观测到弱网（首字节引导实测带宽低于阈值，task-18）。
    pub weak_network: bool,
    /// 本会话实际预读窗口（弱网会降级，task-18）。
    pub effective_readahead_bytes: u64,
    /// seek 触发后台预读暂停的次数（task-18 诊断）。
    pub seek_pauses: u64,
    /// 首字节引导累计发送字节（task-18 诊断）。
    pub bootstrap_bytes: u64,
    /// 最近一次首字节引导实测带宽（Mbps）。
    pub bootstrap_mbps: f64,
}

/// 本地流代理。
pub struct StreamProxy {
    config: StreamConfig,
    inner: tokio::sync::OnceCell<Arc<proxy::ProxyInner>>,
}

impl StreamProxy {
    pub fn new(config: StreamConfig) -> Self {
        Self {
            config,
            inner: tokio::sync::OnceCell::new(),
        }
    }

    pub fn config(&self) -> &StreamConfig {
        &self.config
    }

    /// 修改配置（只能在 [`start`](Self::start) 之前生效；已启动时返回错误）。
    pub fn set_config(&mut self, config: StreamConfig) -> Result<()> {
        if self.inner.initialized() {
            return Err(Error::other("流代理已启动，不能再修改配置"));
        }
        self.config = config;
        Ok(())
    }

    /// 启动代理（幂等；绑定 `127.0.0.1:0`，返回实际端口）。
    pub async fn start(&self) -> Result<u16> {
        Ok(self.ensure_started().await?.port())
    }

    /// 当前已绑定端口（未启动返回 None）。
    pub fn port(&self) -> Option<u16> {
        self.inner.get().map(|inner| inner.port())
    }

    async fn ensure_started(&self) -> Result<Arc<proxy::ProxyInner>> {
        self.inner
            .get_or_try_init(|| async { proxy::ProxyInner::start(&self.config).await })
            .await
            .cloned()
    }

    /// 为指定网盘文件打开流（内部取 DirectLink、启动预读）。
    pub async fn open(&self, drive: Arc<dyn Drive>, remote_path: &str) -> Result<StreamHandle> {
        let provider: Arc<dyn LinkProvider> = Arc::new(DriveLinkProvider::new(drive));
        self.open_with_provider(provider, remote_path).await
    }

    /// 同 [`open`](Self::open)，但直接接受 [`LinkProvider`]（单测/自定义取链用）。
    pub async fn open_with_provider(
        &self,
        provider: Arc<dyn LinkProvider>,
        remote_path: &str,
    ) -> Result<StreamHandle> {
        let inner = self.ensure_started().await?;
        inner.open(provider, remote_path).await
    }

    /// 用**已取到的直链**打开代理流（不再重复请求 `direct_link`）。
    ///
    /// 供 [`resolve_playback`] / GUI 在「一次取链 → 判断是否 HLS → 非 HLS 建代理」链路里复用，
    /// 避免同一路径取两次直链（很多网盘 `direct_link` 有额外耗时/配额）。
    /// `provider` 仍用于后续链接过期热换（401/403/410/expire_at）。
    pub async fn open_with_link(
        &self,
        drive: Arc<dyn Drive>,
        remote_path: &str,
        link: DirectLink,
    ) -> Result<StreamHandle> {
        let provider: Arc<dyn LinkProvider> = Arc::new(DriveLinkProvider::new(drive));
        self.open_link_with_provider(provider, remote_path, link)
            .await
    }

    /// 同 [`open_with_link`](Self::open_with_link)，但接受 [`LinkProvider`]（单测/自定义取链用）。
    pub async fn open_link_with_provider(
        &self,
        provider: Arc<dyn LinkProvider>,
        remote_path: &str,
        link: DirectLink,
    ) -> Result<StreamHandle> {
        let inner = self.ensure_started().await?;
        inner.open_with_link(provider, remote_path, link).await
    }

    /// 取流统计（未打开的 token 返回 None）。
    pub fn stats(&self, token: &str) -> Option<StreamStats> {
        self.inner.get().and_then(|inner| inner.stats(token))
    }

    /// 关闭流（停止预读、释放缓存项引用；缓存文件保留给下次命中）。
    pub fn close(&self, token: &str) {
        if let Some(inner) = self.inner.get() {
            inner.close(token);
        }
    }

    /// 当前打开中的流数量。
    pub fn session_count(&self) -> usize {
        self.inner
            .get()
            .map(|inner| inner.session_count())
            .unwrap_or(0)
    }

    /// 停止代理服务（关闭全部会话并跳出 accept 循环）。
    pub async fn shutdown(&self) {
        if let Some(inner) = self.inner.get() {
            inner.shutdown().await;
        }
    }
}

impl Drop for StreamProxy {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.get() {
            inner.close_all_sync();
        }
    }
}
