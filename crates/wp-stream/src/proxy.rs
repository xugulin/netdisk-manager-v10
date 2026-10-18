//! 本地 HTTP 流代理（T12）+ 磁盘预读缓存（T13）+ 链接过期热换（T14）。
//!
//! 播放器只看到 `http://127.0.0.1:<port>/s/<token>`：
//! * 收到 `Range` 请求 → 查磁盘分块缓存 → 命中直接本地返回；
//! * 未命中 → 按块向 `DirectLink` 拉取（头注入），边缓存边回给播放器；
//! * 后台预读：从播放位置起预取 `readahead_bytes` 窗口（多连接，默认 2）；
//! * 上游 401/403/410、超时/网络错误、或 `expire_at` 将到 → 重新 `direct_link`，
//!   从**当前块起始偏移**续传（每块最多换链 `max_link_restarts` 次，默认 3）。
//!
//! HTTP 细节：
//! * 支持 `GET`/`HEAD`；单段 `Range`（`bytes=a-b` / `a-` / `-N`）；多段返回 `400`（明确不支持）；
//! * 命中返回 `206 + Content-Range`，`Accept-Ranges: bytes` 透传语义；越界返回 `416`；
//! * 支持 HTTP/1.1 keep-alive（同连接可连续多个 Range 请求，seek 时不重复建连）；
//! * 仅监听 `127.0.0.1`，用随机 token 路径做访问控制（不监听 0.0.0.0）。

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use wp_core::model::DirectLink;
use wp_core::{Error, Result};

use crate::cache::{CacheConfig, CacheEntry, CacheManager, ChunkState};
use crate::range::{self, RangeError};
use crate::upstream::{LinkProvider, UpstreamClient, UpstreamConfig};
use crate::{StreamConfig, StreamHandle, StreamStats};

/// 请求头上限。
const MAX_HEADER_BYTES: usize = 32 * 1024;
/// 读请求头的超时（防慢速连接占住任务）。
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// 预读调度 tick。
const PREFETCH_TICK: Duration = Duration::from_millis(100);
/// 链接到期前提前换链的窗口（Lead 要求 30–60s，取 45s）。
const LINK_EXPIRE_MARGIN_SECS: i64 = 45;
/// keep-alive 单连接最多复用请求数。
const MAX_REQUESTS_PER_CONN: usize = 1000;

/// 生效配置（把 [`StreamConfig`] 收敛到合法范围）。
#[derive(Debug, Clone, Copy)]
struct EffectiveConfig {
    chunk_bytes: u64,
    readahead_bytes: u64,
    parallel_fetches: usize,
    max_link_restarts: usize,
}

impl EffectiveConfig {
    fn from(config: &StreamConfig) -> Self {
        Self {
            chunk_bytes: config.chunk_bytes.max(1),
            readahead_bytes: config.readahead_bytes,
            parallel_fetches: config.parallel_fetches.max(1),
            max_link_restarts: config.max_link_restarts,
        }
    }
}

/// 代理共享状态（含监听端口与会话表）。
pub(crate) struct ProxyInner {
    cfg: EffectiveConfig,
    client: UpstreamClient,
    cache: Arc<CacheManager>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    shutdown: Arc<AtomicBool>,
    accept_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    port: u16,
}

impl ProxyInner {
    /// 绑定 `127.0.0.1:0` 并启动 accept 循环。
    pub(crate) async fn start(config: &StreamConfig) -> Result<Arc<Self>> {
        let cfg = EffectiveConfig::from(config);
        let cache_dir = if config.cache_dir.as_os_str().is_empty() {
            std::env::temp_dir().join("wp-stream-cache")
        } else {
            config.cache_dir.clone()
        };
        let cache = CacheManager::new(CacheConfig {
            dir: cache_dir,
            chunk_bytes: cfg.chunk_bytes,
            max_bytes: config.max_cache_bytes,
        })?;
        let client = UpstreamClient::new(UpstreamConfig {
            connect_timeout: Duration::from_secs(config.connect_timeout_secs.max(1)),
            read_timeout: Duration::from_secs(config.request_timeout_secs.max(1)),
        })?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| Error::other(format!("绑定本地流代理端口失败：{e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| Error::other(format!("读取本地代理端口失败：{e}")))?
            .port();
        let inner = Arc::new(Self {
            cfg,
            client,
            cache,
            sessions: Mutex::new(HashMap::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            accept_task: Mutex::new(None),
            port,
        });
        let me = inner.clone();
        let accept_handle = tokio::spawn(async move { accept_loop(me, listener).await });
        if let Ok(mut slot) = inner.accept_task.lock() {
            *slot = Some(accept_handle);
        }
        wp_core::log_info!("wp-stream", "本地流代理已启动：127.0.0.1:{port}");
        Ok(inner)
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    pub(crate) fn session_count(&self) -> usize {
        self.sessions.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// 为网盘文件打开流：取直链 → 确定大小 → 建缓存项 → 注册会话并启动预读。
    pub(crate) async fn open(
        &self,
        provider: Arc<dyn LinkProvider>,
        remote_path: &str,
    ) -> Result<StreamHandle> {
        let link = provider.direct_link(remote_path).await?;
        self.open_with_link(provider, remote_path, link).await
    }

    /// 用已取到的直链打开流（不再请求 `direct_link`；HLS 直链仍返回 direct 句柄）。
    pub(crate) async fn open_with_link(
        &self,
        provider: Arc<dyn LinkProvider>,
        remote_path: &str,
        link: DirectLink,
    ) -> Result<StreamHandle> {
        let display = if link.file_name.is_empty() {
            wp_core::util::base_name(remote_path)
        } else {
            link.file_name.clone()
        };
        // HLS/m3u8（如夸克大文件云端转码）：代理不重写播放列表/分片，直接把原始 URL+headers
        // 交给播放器直连（mpv 支持 --http-header-fields 注入鉴权头）。
        if link.is_hls {
            wp_core::log_info!("wp-stream", "HLS 直连（不走 Range 代理）：{display}");
            return Ok(StreamHandle {
                url: link.url,
                token: String::new(),
                display,
                total_bytes: link.size,
                direct: true,
                headers: link.headers,
            });
        }
        let total = if link.size > 0 {
            link.size
        } else {
            resolve_size(&self.client, &link).await?
        };
        let cache = self.cache.open(&provider.cache_key(), remote_path, total)?;
        let token = wp_core::util::random_hex(16);
        let content_type = content_type_for(&display).to_string();
        let session = Arc::new(Session {
            display: display.clone(),
            remote_path: remote_path.to_string(),
            provider,
            cache,
            client: self.client.clone(),
            cfg: self.cfg,
            total,
            content_type,
            link: AsyncMutex::new(LinkState {
                link,
                generation: 0,
            }),
            position: AtomicU64::new(0),
            downloaded: AtomicU64::new(0),
            served: AtomicU64::new(0),
            restarts: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            started: Instant::now(),
        });
        session.spawn_prefetch();
        {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| Error::other("会话表锁中毒"))?;
            sessions.insert(token.clone(), session);
        }
        Ok(StreamHandle {
            url: format!("http://127.0.0.1:{}/s/{}", self.port, token),
            token,
            display,
            total_bytes: total,
            direct: false,
            headers: Vec::new(),
        })
    }

    pub(crate) fn stats(&self, token: &str) -> Option<StreamStats> {
        self.sessions
            .lock()
            .ok()
            .and_then(|s| s.get(token).cloned())
            .map(|s| s.stats())
    }

    pub(crate) fn close(&self, token: &str) {
        if let Ok(mut sessions) = self.sessions.lock() {
            if let Some(session) = sessions.remove(token) {
                session.closed.store(true, Ordering::Relaxed);
                wp_core::log_debug!("wp-stream", "关闭流会话：{}（{}）", token, session.display);
            }
        }
    }

    /// 同步关闭全部会话（Drop 里也能调用）。
    pub(crate) fn close_all_sync(&self) {
        if let Ok(mut sessions) = self.sessions.lock() {
            for (_, session) in sessions.drain() {
                session.closed.store(true, Ordering::Relaxed);
            }
        }
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// 停止代理：关闭会话并跳出 accept 循环。
    pub(crate) async fn shutdown(&self) {
        self.close_all_sync();
        let handle = self
            .accept_task
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(handle) = handle {
            handle.abort();
        }
    }
}

/// HEAD 拿不到大小时，用 `Range: bytes=0-0` 探测总大小。
async fn resolve_size(client: &UpstreamClient, link: &DirectLink) -> Result<u64> {
    if let Ok(raw) = client.head(link).await {
        if raw.is_success() {
            if let Some(n) = raw.content_length {
                if n > 0 {
                    return Ok(n);
                }
            }
        }
    }
    match client.probe_get(link, 0, 0).await {
        Ok(raw) if raw.status == 206 => raw
            .content_range
            .as_deref()
            .and_then(UpstreamClient::parse_content_range_total)
            .ok_or_else(|| Error::other("上游 206 缺少可解析的 Content-Range")),
        Ok(raw) if raw.status == 200 => raw
            .content_length
            .ok_or_else(|| Error::other("上游未返回 Content-Length，无法确定文件大小")),
        Ok(raw) => Err(Error::other(format!(
            "无法确定文件大小：上游 HTTP {} {}",
            raw.status,
            raw.error_snippet.trim()
        ))),
        Err(e) => Err(Error::other(format!("无法确定文件大小：{e}"))),
    }
}

async fn accept_loop(inner: Arc<ProxyInner>, listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                let me = inner.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(me, stream).await {
                        wp_core::log_debug!("wp-stream", "代理连接结束：{e}");
                    }
                });
            }
            Err(e) => {
                if inner.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                wp_core::log_warn!("wp-stream", "accept 失败：{e}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

/// 一条 TCP 连接：循环处理 keep-alive 请求。
async fn handle_connection(inner: Arc<ProxyInner>, stream: TcpStream) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    let mut reader = BufReader::new(stream);
    for _ in 0..MAX_REQUESTS_PER_CONN {
        if inner.shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
        let Some(req) = read_request(&mut reader).await? else {
            return Ok(());
        };
        let close = dispatch(&inner, reader.get_mut(), &req).await?;
        if close || !req.keep_alive() {
            return Ok(());
        }
    }
    Ok(())
}

/// 解析后的 HTTP 请求。
#[derive(Debug, Default)]
struct HttpRequest {
    method: String,
    target: String,
    version: String,
    headers: HashMap<String, String>,
    malformed: bool,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|s| s.as_str())
    }
    fn keep_alive(&self) -> bool {
        let conn = self.header("connection").unwrap_or("").to_ascii_lowercase();
        if conn.contains("close") {
            return false;
        }
        self.version.eq_ignore_ascii_case("HTTP/1.1") || conn.contains("keep-alive")
    }
}

async fn read_request(reader: &mut BufReader<TcpStream>) -> io::Result<Option<HttpRequest>> {
    let mut line = String::new();
    // 跳过前导空行
    loop {
        line.clear();
        let n = tokio::time::timeout(REQUEST_READ_TIMEOUT, reader.read_line(&mut line))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "读请求行超时"))??;
        if n == 0 {
            return Ok(None);
        }
        if !line.trim().is_empty() {
            break;
        }
    }
    let mut req = HttpRequest::default();
    let first_line = line.trim_end_matches(['\r', '\n']);
    let mut parts = first_line.split_whitespace();
    match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(v)) => {
            req.method = m.to_string();
            req.target = t.to_string();
            req.version = v.to_string();
        }
        _ => {
            req.malformed = true;
            return Ok(Some(req));
        }
    }
    let mut total = first_line.len();
    loop {
        line.clear();
        let n = tokio::time::timeout(REQUEST_READ_TIMEOUT, reader.read_line(&mut line))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "读请求头超时"))??;
        if n == 0 {
            return Ok(None);
        }
        total += n;
        if total > MAX_HEADER_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "请求头过大"));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            req.headers
                .insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    Ok(Some(req))
}

/// 分发请求；返回 `true` 表示处理完应关闭连接。
async fn dispatch(
    inner: &Arc<ProxyInner>,
    stream: &mut TcpStream,
    req: &HttpRequest,
) -> io::Result<bool> {
    if req.malformed {
        write_text(stream, 400, "Bad Request", "请求行非法", true, false).await?;
        return Ok(true);
    }
    let method = req.method.to_ascii_uppercase();
    if method != "GET" && method != "HEAD" {
        write_text(
            stream,
            405,
            "Method Not Allowed",
            "仅支持 GET / HEAD",
            false,
            false,
        )
        .await?;
        return Ok(true);
    }
    let head_only = method == "HEAD";
    let path = req.target.split('?').next().unwrap_or("");

    // 健康检查（调试/验收脚本用）
    if path == "/healthz" {
        let body = format!(
            "{{\"ok\":true,\"sessions\":{},\"version\":\"{}\"}}",
            inner.session_count(),
            wp_core::APP_VERSION
        );
        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Content-Length".to_string(), body.len().to_string()),
        ];
        write_head(stream, 200, "OK", &headers, false).await?;
        if !head_only {
            stream.write_all(body.as_bytes()).await?;
        }
        return Ok(false);
    }

    let Some(token) = path.strip_prefix("/s/") else {
        write_text(
            stream,
            404,
            "Not Found",
            "未知路径（应为 /s/<token>）",
            head_only,
            false,
        )
        .await?;
        return Ok(false);
    };
    if token.is_empty() || token.contains('/') {
        write_text(stream, 404, "Not Found", "token 非法", head_only, false).await?;
        return Ok(false);
    }
    let session = inner
        .sessions
        .lock()
        .map_err(|_| io::Error::other("会话表锁中毒"))?
        .get(token)
        .cloned();
    let Some(session) = session else {
        write_text(
            stream,
            404,
            "Not Found",
            "流会话不存在（可能已关闭）",
            head_only,
            false,
        )
        .await?;
        return Ok(false);
    };
    session
        .serve(stream, head_only, req.header("range"), req.keep_alive())
        .await?;
    Ok(false)
}

/// 写响应头。
async fn write_head(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    headers: &[(String, String)],
    close: bool,
) -> io::Result<()> {
    let mut head = String::with_capacity(256);
    head.push_str(&format!("HTTP/1.1 {status} {reason}\r\n"));
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if close {
        head.push_str("Connection: close\r\n");
    } else {
        head.push_str("Connection: keep-alive\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await
}

/// 直接回一个带文本 body 的错误/提示响应。
async fn write_text(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    msg: &str,
    head_only: bool,
    _keep_alive: bool,
) -> io::Result<()> {
    let headers = vec![
        (
            "Content-Type".to_string(),
            "text/plain; charset=utf-8".to_string(),
        ),
        ("Content-Length".to_string(), msg.len().to_string()),
        ("Cache-Control".to_string(), "no-store".to_string()),
    ];
    write_head(stream, status, reason, &headers, true).await?;
    if !head_only {
        stream.write_all(msg.as_bytes()).await?;
        stream.flush().await?;
    }
    Ok(())
}

/// 一个打开中的流会话。
pub(crate) struct Session {
    display: String,
    remote_path: String,
    provider: Arc<dyn LinkProvider>,
    cache: Arc<CacheEntry>,
    client: UpstreamClient,
    cfg: EffectiveConfig,
    total: u64,
    content_type: String,
    /// 当前直链 + 代次（换链时 +1，用于去重并发换链）。
    link: AsyncMutex<LinkState>,
    /// 播放器已读到的位置（预读窗口起点，字节）。
    position: AtomicU64,
    /// 累计从上游下载字节。
    downloaded: AtomicU64,
    /// 累计回给播放器字节。
    served: AtomicU64,
    /// 换链次数。
    restarts: AtomicU64,
    /// 错误次数。
    errors: AtomicU64,
    closed: AtomicBool,
    started: Instant,
}

struct LinkState {
    link: DirectLink,
    generation: u64,
}

impl Session {
    fn stats(&self) -> StreamStats {
        let downloaded = self.downloaded.load(Ordering::Relaxed);
        let secs = self.started.elapsed().as_secs_f64().max(0.001);
        StreamStats {
            buffered_bytes: self.cache.buffered_bytes(),
            total_bytes: self.total,
            downloaded_bytes: downloaded,
            bitrate_bps: downloaded as f64 * 8.0 / secs,
            upstream_restarts: self.restarts.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            complete: self.cache.is_complete(),
        }
    }

    /// 处理一次 GET/HEAD（Range 可选）。
    async fn serve(
        self: &Arc<Self>,
        stream: &mut TcpStream,
        head_only: bool,
        range_header: Option<&str>,
        keep_alive: bool,
    ) -> io::Result<()> {
        // 1) Range 解析/求值
        let mut range: Option<(u64, u64)> = None;
        if let Some(value) = range_header {
            match range::parse_range_header(value) {
                Ok(spec) => match range::resolve_range(spec, self.total) {
                    Ok((s, e)) => range = Some((s, e)),
                    Err(_) => {
                        self.errors.fetch_add(1, Ordering::Relaxed);
                        return write_416(stream, self.total, keep_alive).await;
                    }
                },
                Err(RangeError::MultiUnsupported) => {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                    return write_text(
                        stream,
                        400,
                        "Bad Request",
                        "不支持多段 Range（wp-stream 仅服务单段：bytes=a-b / a- / -N）",
                        head_only,
                        keep_alive,
                    )
                    .await;
                }
                Err(e) => {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                    return write_text(
                        stream,
                        400,
                        "Bad Request",
                        &e.to_string(),
                        head_only,
                        keep_alive,
                    )
                    .await;
                }
            }
        }
        let (start, end, partial) = match range {
            Some((s, e)) => (s, e, true),
            None => (0, self.total.saturating_sub(1), false),
        };
        let len = if self.total == 0 { 0 } else { end - start + 1 };

        // 2) GET 先保证第一块可用，这样上游错误能干净地回 502（而不是发完头再断流）。
        if !head_only && self.total > 0 {
            let first_chunk = start / self.cache.chunk_bytes().max(1);
            if let Err(e) = self.ensure_chunk(first_chunk).await {
                self.errors.fetch_add(1, Ordering::Relaxed);
                wp_core::log_warn!("wp-stream", "首块拉取失败（{}）：{e}", self.display);
                return write_text(
                    stream,
                    502,
                    "Bad Gateway",
                    &format!("上游读取失败：{e}"),
                    false,
                    false,
                )
                .await;
            }
            self.position.store(start, Ordering::Relaxed);
        }

        // 3) 响应头
        let mut headers: Vec<(String, String)> = vec![
            ("Content-Type".to_string(), self.content_type.clone()),
            ("Accept-Ranges".to_string(), "bytes".to_string()),
            ("Cache-Control".to_string(), "no-store".to_string()),
            ("Server".to_string(), "wp-stream".to_string()),
            ("X-WP-Stream-Total".to_string(), self.total.to_string()),
        ];
        if partial && self.total > 0 {
            headers.push((
                "Content-Range".to_string(),
                range::content_range(start, end, self.total),
            ));
        }
        headers.push(("Content-Length".to_string(), len.to_string()));
        let (status, reason) = if self.total == 0 {
            (200, "OK")
        } else if partial {
            (206, "Partial Content")
        } else {
            (200, "OK")
        };
        write_head(stream, status, reason, &headers, !keep_alive).await?;
        if head_only || self.total == 0 {
            stream.flush().await?;
            return Ok(());
        }

        // 4) 流式回源（按块：先确保块就绪，再从磁盘顺序读）
        self.stream_range(stream, start, end).await
    }

    async fn stream_range(
        self: &Arc<Self>,
        stream: &mut TcpStream,
        start: u64,
        end: u64,
    ) -> io::Result<()> {
        let chunk_bytes = self.cache.chunk_bytes().max(1);
        let mut pos = start;
        let mut buf = vec![0u8; 64 * 1024];
        while pos <= end {
            let idx = pos / chunk_bytes;
            self.ensure_chunk(idx)
                .await
                .map_err(|e| io::Error::other(format!("上游读取失败：{e}")))?;
            let (cs, ce) = self
                .cache
                .chunk_range(idx)
                .ok_or_else(|| io::Error::other("缓存块越界"))?;
            let read_start = pos.max(cs);
            let read_end = end.min(ce);
            let mut off = read_start;
            while off <= read_end {
                let want = ((read_end - off + 1) as usize).min(buf.len());
                let n = self
                    .cache
                    .read_at(off, &mut buf[..want])
                    .await
                    .map_err(|e| io::Error::other(format!("读缓存失败：{e}")))?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "缓存读到的字节数不足",
                    ));
                }
                stream.write_all(&buf[..n]).await?;
                off += n as u64;
                self.served.fetch_add(n as u64, Ordering::Relaxed);
                self.position.store(off, Ordering::Relaxed);
            }
            pos = read_end + 1;
        }
        stream.flush().await
    }

    // ───────────────────────── 缓存/上游 ─────────────────────────

    async fn ensure_chunk(self: &Arc<Self>, idx: u64) -> Result<()> {
        let me = self.clone();
        self.cache
            .ensure_chunk(idx, move || async move { me.download_chunk(idx).await })
            .await
    }

    async fn download_chunk(self: &Arc<Self>, idx: u64) -> Result<()> {
        let Some((start, end)) = self.cache.chunk_range(idx) else {
            return Err(Error::other(format!("缓存块越界：{idx}")));
        };
        let expect = (end - start + 1) as usize;
        let mut attempt: usize = 0;
        loop {
            let (generation, link) = self.link_snapshot().await?;
            let result = self.client.get_range(&link, start, end).await;
            match result {
                Ok(raw) if raw.status == 206 => {
                    if raw.body.len() != expect {
                        // 数据不完整按可重试处理
                    } else {
                        self.cache.write_chunk(idx, raw.body).await?;
                        self.downloaded.fetch_add(expect as u64, Ordering::Relaxed);
                        return Ok(());
                    }
                }
                Ok(raw) if raw.status == 200 && raw.upstream_ignored_range => {
                    if raw.body.len() == expect {
                        wp_core::log_warn!(
                            "wp-stream",
                            "上游忽略 Range（200 回退整文件裁剪）：{} 块 {idx}",
                            self.display
                        );
                        self.cache.write_chunk(idx, raw.body).await?;
                        self.downloaded.fetch_add(expect as u64, Ordering::Relaxed);
                        return Ok(());
                    }
                }
                Ok(raw) if raw.status == 416 => {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                    return Err(Error::other(format!(
                        "上游 416：请求块 {idx} 超出文件大小（本地记录 total={}）",
                        self.total
                    )));
                }
                Ok(raw) if raw.is_auth_expired() || raw.is_transient() => {
                    // 换链后再试
                }
                Ok(raw) => {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                    return Err(Error::other(format!(
                        "上游返回异常状态 {}：{}",
                        raw.status,
                        raw.error_snippet.trim()
                    )));
                }
                Err(_) => {
                    // 超时/网络错误 → 换链后重试
                }
            }

            if attempt >= self.cfg.max_link_restarts {
                self.errors.fetch_add(1, Ordering::Relaxed);
                return Err(Error::network(format!(
                    "上游连续失败（已换链 {attempt} 次），块 {idx} 读取放弃"
                )));
            }
            attempt += 1;
            self.refresh_link(generation).await?;
        }
    }

    /// 取当前直链；若 `expire_at` 将到（45s 内）主动换链。
    async fn link_snapshot(&self) -> Result<(u64, DirectLink)> {
        let mut st = self.link.lock().await;
        if let Some(expire_at) = st.link.expire_at {
            if wp_core::util::now_ts() + LINK_EXPIRE_MARGIN_SECS >= expire_at {
                self.refresh_locked(&mut st).await?;
            }
        }
        Ok((st.generation, st.link.clone()))
    }

    /// 上游拒绝后换链；若别的并发任务已经换过（代次变化）则直接复用。
    async fn refresh_link(&self, stale_generation: u64) -> Result<()> {
        let mut st = self.link.lock().await;
        if st.generation != stale_generation {
            return Ok(());
        }
        self.refresh_locked(&mut st).await
    }

    async fn refresh_locked(&self, st: &mut LinkState) -> Result<()> {
        let new_link = self.provider.direct_link(&self.remote_path).await?;
        st.link = new_link;
        st.generation += 1;
        let n = self.restarts.fetch_add(1, Ordering::Relaxed) + 1;
        wp_core::log_info!("wp-stream", "链接热换成功（第 {n} 次）：{}", self.display);
        Ok(())
    }

    // ───────────────────────── 预读 ─────────────────────────

    fn spawn_prefetch(self: &Arc<Self>) {
        if self.cfg.readahead_bytes == 0 || self.total == 0 {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move { me.prefetch_loop().await });
    }

    /// 后台预读：从播放位置起维护 `readahead_bytes` 窗口，多连接预取缺失块。
    async fn prefetch_loop(self: Arc<Self>) {
        let sem = Arc::new(Semaphore::new(self.cfg.parallel_fetches.max(1)));
        loop {
            if self.closed.load(Ordering::Relaxed) {
                return;
            }
            if self.cache.is_complete() {
                return;
            }
            let pos = self
                .position
                .load(Ordering::Relaxed)
                .min(self.total.saturating_sub(1));
            let window_end = (pos + self.cfg.readahead_bytes).min(self.total);
            let chunk_bytes = self.cache.chunk_bytes().max(1);
            let first = pos / chunk_bytes;
            let last = window_end.saturating_sub(1) / chunk_bytes;
            let mut spawned = 0usize;
            let mut idx = first;
            while idx <= last {
                if idx >= self.cache.chunk_count() {
                    break;
                }
                match self.cache.chunk_state(idx) {
                    ChunkState::Ready | ChunkState::Loading | ChunkState::Failed(_) => {}
                    ChunkState::Missing => {
                        let Ok(permit) = sem.clone().try_acquire_owned() else {
                            break;
                        };
                        spawned += 1;
                        let me = self.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            let _ = me.ensure_chunk(idx).await;
                        });
                    }
                }
                idx += 1;
            }
            if spawned == 0 {
                tokio::time::sleep(PREFETCH_TICK).await;
            }
        }
    }
}

async fn write_416(stream: &mut TcpStream, total: u64, keep_alive: bool) -> io::Result<()> {
    let headers = vec![
        (
            "Content-Type".to_string(),
            "text/plain; charset=utf-8".to_string(),
        ),
        (
            "Content-Range".to_string(),
            range::content_range_star(total),
        ),
        ("Content-Length".to_string(), "0".to_string()),
    ];
    write_head(stream, 416, "Range Not Satisfiable", &headers, !keep_alive).await
}

/// 按扩展名猜 Content-Type（播放器主要靠它选解复用器）。
fn content_type_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "m4v" | "f4v" => "video/mp4",
        "mkv" | "mk3d" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "ts" | "m2ts" | "mts" => "video/mp2t",
        "flv" => "video/x-flv",
        "wmv" | "asf" => "video/x-ms-wmv",
        "mpg" | "mpeg" => "video/mpeg",
        "iso" => "application/octet-stream",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "m4a" | "aac" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "srt" => "application/x-subrip",
        "ass" | "ssa" => "text/x-ssa",
        "sup" => "application/octet-stream",
        "zip" | "7z" | "rar" | "tar" | "gz" | "bin" => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types() {
        assert_eq!(content_type_for("/a/b/c.mp4"), "video/mp4");
        assert_eq!(content_type_for("movie.MKV"), "video/x-matroska");
        assert_eq!(content_type_for("noext"), "application/octet-stream");
        assert_eq!(content_type_for("字幕.ass"), "text/x-ssa");
    }

    #[test]
    fn effective_config_clamps() {
        let cfg = StreamConfig {
            chunk_bytes: 0,
            parallel_fetches: 0,
            ..Default::default()
        };
        let e = EffectiveConfig::from(&cfg);
        assert_eq!(e.chunk_bytes, 1);
        assert_eq!(e.parallel_fetches, 1);
    }

    #[test]
    fn keep_alive_defaults() {
        let mut r = HttpRequest {
            version: "HTTP/1.1".into(),
            ..Default::default()
        };
        assert!(r.keep_alive());
        r.headers.insert("connection".into(), "close".into());
        assert!(!r.keep_alive());
        let mut r10 = HttpRequest {
            version: "HTTP/1.0".into(),
            ..Default::default()
        };
        assert!(!r10.keep_alive());
        r10.headers.insert("connection".into(), "keep-alive".into());
        assert!(r10.keep_alive());
    }
}
