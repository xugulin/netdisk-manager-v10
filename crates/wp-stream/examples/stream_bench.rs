//! 网盘流播放性能压测工具（M1 / task-14，非生产二进制）。
//!
//! 输入 `wp-cli link <远端路径>` 输出的 `DirectLink` JSON，经 `resolve_playback_with_provider`
//! 起本地代理（或 HLS 直连分支），模拟播放器读取模式采集：
//!
//! * 顺序读：吞吐 MB/s、首字节（TTFB）、读中停顿（gap >250ms 次数）；
//! * 预读：预读窗口填满时间、期间下载量；
//! * 随机 seek：前/中/后 + 回看，缓存命中 vs 未命中的首字节延迟；
//! * 模拟播放（paced）：按 4K 码率消费，统计停顿次数/最长请求延迟（可配合弱网限速）；
//! * 弱网：本地限速中继（token bucket）把真实直链限速到 N Mbps，再测起播/卡顿；
//! * HLS：m3u8 拉取、分片下载带宽、起播估算、40% 位置 seek 延迟；
//! * matrix：对多组 `chunk/readahead/parallel` 参数重复上述子项，用于调参。
//!
//! 用法示例：
//! ```text
//! wp-cli --data-dir 出/真机联调数据 --drive guangya link /…/big.mkv > /tmp/link.json
//! cargo run -p wp-stream --example stream_bench -- /tmp/link.json --mode all --read-bytes 32M
//! cargo run -p wp-stream --example stream_bench -- /tmp/link.json --mode weak --limit-bps 20M --play-bps 60M
//! cargo run -p wp-stream --example stream_bench -- /tmp/quark-hls.json --mode hls
//! cargo run -p wp-stream --example stream_bench -- /tmp/link.json --mode matrix --read-bytes 24M
//! ```
//!
//! 只读、不写远端；缓存目录在系统临时目录，结束即删；输出仅含 host/头名，不输出敏感头值。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use wp_core::model::DirectLink;
use wp_stream::{
    resolve_playback_with_provider, LinkProvider, PlaybackSource, StreamConfig, StreamProxy,
};

/// 「卡顿」判定阈值：一次读取请求被阻塞超过该时长，认为播放器会饥饿。
const STALL_THRESHOLD: Duration = Duration::from_millis(300);
/// 顺序读时相邻数据块间隔超过该时长计入「读中停顿」。
const GAP_THRESHOLD: Duration = Duration::from_millis(250);
/// 预读填满等待上限。
const PREFETCH_TIMEOUT: Duration = Duration::from_secs(20);

// ───────────────────────── CLI ─────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    All,
    Seq,
    Prefetch,
    Seek,
    Paced,
    Weak,
    Hls,
    Matrix,
    /// 代理 + 系统 mpv 直连冒烟（测「代理起播」端到端耗时）
    Mpv,
}

#[derive(Debug, Clone)]
struct Options {
    link_file: PathBuf,
    remote_path: String,
    mode: Mode,
    chunk: u64,
    readahead: u64,
    parallel: usize,
    lru: u64,
    read_bytes: u64,
    play_bps: u64,
    seconds: f64,
    limit_bps: u64,
    seek_bytes: u64,
    out: Option<PathBuf>,
    /// task-18：自适应首字节/分块（默认开）
    adaptive: bool,
    startup_first: u64,
    seek_priority: bool,
    /// 4xx 熔断阈值（task-21；0=关闭）
    breaker: usize,
    /// 单块最多换链次数
    max_restarts: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            link_file: PathBuf::new(),
            remote_path: "/bench/video".to_string(),
            mode: Mode::All,
            chunk: 4 * 1024 * 1024,
            readahead: 64 * 1024 * 1024,
            parallel: 2,
            lru: 20 * 1024 * 1024 * 1024,
            read_bytes: 32 * 1024 * 1024,
            play_bps: 60 * 1_000_000, // 4K 中码率 60Mbps
            seconds: 12.0,
            limit_bps: 0,
            seek_bytes: 1024 * 1024,
            out: None,
            adaptive: true,
            startup_first: 1024 * 1024,
            seek_priority: true,
            breaker: 2,
            max_restarts: 3,
        }
    }
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    let (num, mult) = if let Some(v) = lower.strip_suffix("gb") {
        (v, 1024u64 * 1024 * 1024)
    } else if let Some(v) = lower.strip_suffix("mb") {
        (v, 1024u64 * 1024)
    } else if let Some(v) = lower.strip_suffix("kb") {
        (v, 1024u64)
    } else if let Some(v) = lower.strip_suffix('g') {
        (v, 1024u64 * 1024 * 1024)
    } else if let Some(v) = lower.strip_suffix('m') {
        (v, 1024u64 * 1024)
    } else if let Some(v) = lower.strip_suffix('k') {
        (v, 1024u64)
    } else if let Some(v) = lower.strip_suffix('b') {
        (v, 1u64)
    } else {
        (lower.as_str(), 1u64)
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("尺寸参数非法：{s}"))?;
    if !(n.is_finite() && n >= 0.0) {
        return Err(format!("尺寸参数非法：{s}"));
    }
    Ok((n * mult as f64) as u64)
}

fn parse_bps(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    if let Some(v) = lower.strip_suffix("mbps") {
        Ok((v
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("码率非法：{s}"))?
            * 1_000_000.0) as u64)
    } else if let Some(v) = lower.strip_suffix("kbps") {
        Ok((v
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("码率非法：{s}"))?
            * 1_000.0) as u64)
    } else if let Some(v) = lower.strip_suffix("bps") {
        Ok(v.trim()
            .parse::<u64>()
            .map_err(|_| format!("码率非法：{s}"))?)
    } else {
        Ok(s.parse::<u64>().map_err(|_| format!("码率非法：{s}"))?)
    }
}

fn parse_args() -> Result<Options, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut opts = Options::default();
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let mut next = || -> Result<String, String> {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("参数 {a} 缺少值"))
        };
        match a {
            "--mode" => {
                opts.mode = match next()?.as_str() {
                    "all" => Mode::All,
                    "seq" => Mode::Seq,
                    "prefetch" => Mode::Prefetch,
                    "seek" => Mode::Seek,
                    "paced" => Mode::Paced,
                    "weak" => Mode::Weak,
                    "hls" => Mode::Hls,
                    "matrix" => Mode::Matrix,
                    "mpv" => Mode::Mpv,
                    other => return Err(format!("未知 mode：{other}")),
                };
            }
            "--chunk" => opts.chunk = parse_size(&next()?)?,
            "--readahead" => opts.readahead = parse_size(&next()?)?,
            "--parallel" => {
                opts.parallel = next()?
                    .parse()
                    .map_err(|_| "parallel 需为整数".to_string())?
            }
            "--lru" => opts.lru = parse_size(&next()?)?,
            "--read-bytes" => opts.read_bytes = parse_size(&next()?)?,
            "--play-bps" => opts.play_bps = parse_bps(&next()?)?,
            "--seconds" => {
                opts.seconds = next()?
                    .parse()
                    .map_err(|_| "seconds 需为数字".to_string())?
            }
            "--limit-bps" => opts.limit_bps = parse_bps(&next()?)?,
            "--seek-bytes" => opts.seek_bytes = parse_size(&next()?)?,
            "--adaptive" => match next()?.as_str() {
                "on" | "1" | "true" => opts.adaptive = true,
                "off" | "0" | "false" => opts.adaptive = false,
                other => return Err(format!("--adaptive 需 on/off，得到 {other}")),
            },
            "--startup-first" => opts.startup_first = parse_size(&next()?)?,
            "--no-seek-priority" => opts.seek_priority = false,
            "--breaker-threshold" => {
                opts.breaker = next()?
                    .parse()
                    .map_err(|_| "breaker-threshold 需为整数（0=关闭）".to_string())?
            }
            "--max-restarts" => {
                opts.max_restarts = next()?
                    .parse()
                    .map_err(|_| "max-restarts 需为整数".to_string())?
            }
            "--out" => opts.out = Some(PathBuf::from(next()?)),
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other if other.starts_with("--") => return Err(format!("未知参数：{other}")),
            other => positional.push(other.to_string()),
        }
        i += 1;
    }
    let Some(link_file) = positional.first() else {
        return Err("缺少 DirectLink JSON 文件（先用 wp-cli link 生成）".into());
    };
    opts.link_file = PathBuf::from(link_file);
    if let Some(p) = positional.get(1) {
        opts.remote_path = p.clone();
    }
    Ok(opts)
}

fn print_help() {
    println!(
        r#"网盘流播放性能压测（wp-stream）
用法：stream_bench <direct-link.json> [远端路径] [选项]

选项：
  --mode <all|seq|prefetch|seek|paced|weak|hls|matrix|mpv>  默认 all
  --chunk <4M>        代理分块大小
  --readahead <64M>   预读窗口（0=关闭）
  --parallel <2>      并行预取连接数
  --lru <20G>         缓存上限
  --read-bytes <32M>  顺序读/矩阵测的读取字节上限
  --play-bps <60M>    paced 模拟播放码率（4K 常用 40–100Mbps）
  --seconds <12>      paced/weak 模拟播放时长
  --limit-bps <0>     >0 时启用本地限速中继（弱网模拟），如 20M/50M
  --seek-bytes <1M>   每次 seek 读取窗口
  --adaptive <on|off> 自适应首字节/分块（默认 on；task-18）
  --startup-first <1M> 首字节引导块大小（0=关闭）
  --no-seek-priority  关闭 seek 优先（默认开）
  --breaker-threshold <2>  权限类 4xx 连续失败熔断阈值（0=关闭，task-21）
  --max-restarts <3>  单块最多换链次数
  --out <path>        结果 JSON 写入文件
"#
    );
}

// ───────────────────────── 固定直链 Provider ─────────────────────────

struct FixedProvider {
    link: DirectLink,
    key: String,
}

#[async_trait]
impl LinkProvider for FixedProvider {
    fn cache_key(&self) -> String {
        self.key.clone()
    }
    async fn direct_link(&self, _remote_path: &str) -> wp_core::Result<DirectLink> {
        Ok(self.link.clone())
    }
}

// ───────────────────────── 弱网限速中继 ─────────────────────────

/// 全局限速器：多连接共享同一个速率预算（token bucket 简化版）。
struct Pacer {
    start: std::sync::Mutex<Option<Instant>>,
    sent: AtomicU64,
    limit_bps: u64,
}

impl Pacer {
    fn new(limit_bps: u64) -> Self {
        Self {
            start: std::sync::Mutex::new(None),
            sent: AtomicU64::new(0),
            limit_bps: limit_bps.max(1),
        }
    }
    async fn wait_turn(&self, bytes: u64) {
        let total = self.sent.fetch_add(bytes, Ordering::Relaxed) + bytes;
        let start = {
            let mut g = self.start.lock().unwrap();
            *g.get_or_insert_with(Instant::now)
        };
        let due = Duration::from_secs_f64(total as f64 * 8.0 / self.limit_bps as f64);
        let elapsed = start.elapsed();
        if due > elapsed {
            tokio::time::sleep(due - elapsed).await;
        }
    }
}

struct ThrottledRelay {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ThrottledRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// 把真实直链包装成一个限速的本地 HTTP 中继（幂等透传 Range/状态/关键响应头）。
async fn start_throttle_relay(
    target: DirectLink,
    limit_bps: u64,
) -> Result<ThrottledRelay, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| format!("限速中继绑定失败：{e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("限速中继端口失败：{e}"))?
        .port();
    let target = Arc::new(target);
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("中继 HTTP 客户端失败：{e}"))?;
    let pacer = Arc::new(Pacer::new(limit_bps));
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let target = target.clone();
            let client = client.clone();
            let pacer = pacer.clone();
            tokio::spawn(async move {
                let _ = relay_conn(stream, target, client, pacer).await;
            });
        }
    });
    Ok(ThrottledRelay {
        url: format!("http://127.0.0.1:{port}/relay"),
        task,
    })
}

async fn relay_conn(
    stream: TcpStream,
    target: Arc<DirectLink>,
    client: reqwest::Client,
    pacer: Arc<Pacer>,
) -> Result<(), String> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| e.to_string())?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let mut headers = std::collections::HashMap::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        let t = line.trim_end_matches(['\r', '\n']);
        if t.is_empty() {
            break;
        }
        if let Some((k, v)) = t.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut req = client.request(
        reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET),
        &target.url,
    );
    for (k, v) in &target.headers {
        let kl = k.to_ascii_lowercase();
        if matches!(
            kl.as_str(),
            "range" | "host" | "content-length" | "accept-encoding" | "connection"
        ) {
            continue;
        }
        req = req.header(k.as_str(), v.as_str());
    }
    if let Some(r) = headers.get("range") {
        req = req.header("Range", r.clone());
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let _ = reader
                .get_mut()
                .write_all(
                    format!(
                        "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await;
            return Err(format!("中继上游失败：{e}"));
        }
    };
    let status = resp.status().as_u16();
    let reason = resp.status().canonical_reason().unwrap_or("OK");
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, key) in [
        ("Content-Range", "content-range"),
        ("Content-Length", "content-length"),
        ("Accept-Ranges", "accept-ranges"),
        ("Content-Type", "content-type"),
    ] {
        if let Some(v) = resp.headers().get(key).and_then(|v| v.to_str().ok()) {
            head.push_str(&format!("{name}: {v}\r\n"));
        }
    }
    head.push_str("Connection: close\r\n\r\n");
    {
        let s = reader.get_mut();
        s.write_all(head.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
    }
    if method.eq_ignore_ascii_case("HEAD") {
        return Ok(());
    }
    let mut resp = resp;
    loop {
        match resp.chunk().await {
            Ok(Some(bytes)) => {
                let s = reader.get_mut();
                s.write_all(&bytes).await.map_err(|e| e.to_string())?;
                pacer.wait_turn(bytes.len() as u64).await;
            }
            Ok(None) => break,
            Err(e) => return Err(format!("中继读上游失败：{e}")),
        }
    }
    let _ = reader.get_mut().shutdown().await;
    Ok(())
}

// ───────────────────────── 基准测试器 ─────────────────────────

struct Bench {
    proxy: StreamProxy,
    client: reqwest::Client,
    cache_dir: PathBuf,
}

impl Bench {
    fn new(cfg: StreamConfig) -> Result<Self, String> {
        let cache_dir = cfg.cache_dir.clone();
        std::fs::create_dir_all(&cache_dir).map_err(|e| format!("建缓存目录失败：{e}"))?;
        let proxy = StreamProxy::new(cfg);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| format!("基准 HTTP 客户端失败：{e}"))?;
        Ok(Self {
            proxy,
            client,
            cache_dir,
        })
    }

    async fn open(
        &self,
        link: DirectLink,
        remote_path: &str,
    ) -> Result<wp_stream::StreamHandle, String> {
        let provider = Arc::new(FixedProvider {
            link,
            key: format!("bench:{}", self.cache_dir.display()),
        });
        match resolve_playback_with_provider(&self.proxy, provider, remote_path).await {
            Ok(PlaybackSource::Proxied { handle }) => Ok(handle),
            Ok(PlaybackSource::DirectHls { .. }) => Err("该直链是 HLS，请用 --mode hls".into()),
            Err(e) => Err(format!("打开播放源失败：{e}")),
        }
    }

    async fn finish(self) {
        self.proxy.shutdown().await;
        let _ = std::fs::remove_dir_all(&self.cache_dir);
    }
}

// ───────────────────────── 测量原语 ─────────────────────────

/// 通过代理读一段 Range，返回（字节数，首字节延迟，总延迟）。
async fn read_range(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
) -> Result<(Vec<u8>, f64, f64), String> {
    let t0 = Instant::now();
    let resp = client
        .get(url)
        .header("Range", format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|e| format!("Range 请求失败：{e}"))?;
    if resp.status().as_u16() != 206 {
        return Err(format!("期望 206，实际 {}", resp.status()));
    }
    let mut got: Vec<u8> = Vec::new();
    let mut ttfb: Option<f64> = None;
    let mut resp = resp;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("读 body 失败：{e}"))?
    {
        if ttfb.is_none() {
            ttfb = Some(t0.elapsed().as_secs_f64() * 1000.0);
        }
        got.extend_from_slice(&chunk);
    }
    Ok((
        got,
        ttfb.unwrap_or(0.0),
        t0.elapsed().as_secs_f64() * 1000.0,
    ))
}

/// 顺序读：从 0 开始连续读 `read_bytes`（不足则读到文件尾）。
///
/// 注意：本地代理对 GET 会先确保首块（默认 4MB）就绪才发响应头，因此
/// `headers_ms` ≈ 首块下载时间，是「起播」的关键成本；`body_ttfb_ms` 为响应头后首包间隔。
async fn seq_read(
    bench: &Bench,
    url: &str,
    total: u64,
    read_bytes: u64,
) -> Result<serde_json::Value, String> {
    let want = read_bytes.min(total);
    let t0 = Instant::now();
    let resp = match bench.client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            let st = bench.proxy.stats(&token_of(url)).unwrap_or_default();
            return Ok(serde_json::json!({
                "error": format!(
                    "顺序读请求失败：{e}（connect={}, timeout={}）",
                    e.is_connect(), e.is_timeout()
                ),
                "stats": stats_json(&st),
            }));
        }
    };
    if resp.status().as_u16() != 200 {
        let status = resp.status().as_u16();
        let snippet = resp.text().await.unwrap_or_default();
        let st = bench.proxy.stats(&token_of(url)).unwrap_or_default();
        return Ok(serde_json::json!({
            "status": status,
            "error": format!("顺序读期望 200，实际 {status}"),
            "body_snippet": snippet.chars().take(200).collect::<String>(),
            "stats": stats_json(&st),
        }));
    }
    let headers_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let mut resp = resp;
    let mut got = 0u64;
    let mut body_ttfb: Option<f64> = None;
    let mut last = Instant::now();
    let mut gap_max = 0f64;
    let mut gaps = 0u64;
    let mut read_error: Option<String> = None;
    while got < want {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let now = Instant::now();
                if body_ttfb.is_none() {
                    body_ttfb = Some(t0.elapsed().as_secs_f64() * 1000.0);
                } else {
                    let gap = now.duration_since(last);
                    if gap > GAP_THRESHOLD {
                        gaps += 1;
                    }
                    gap_max = gap_max.max(gap.as_secs_f64() * 1000.0);
                }
                last = now;
                got += chunk.len() as u64;
            }
            Ok(None) => break,
            Err(e) => {
                read_error = Some(e.to_string());
                break;
            }
        }
    }
    let secs = t0.elapsed().as_secs_f64().max(0.001);
    let stats = bench.proxy.stats(&token_of(url)).unwrap_or_default();
    Ok(serde_json::json!({
        "read_bytes": got,
        "want_bytes": want,
        "headers_ms": round1(headers_ms),
        "body_ttfb_ms": round1(body_ttfb.unwrap_or(0.0)),
        "total_ms": round1(secs * 1000.0),
        "throughput_mbps": round2(got as f64 * 8.0 / secs / 1_000_000.0),
        "gap_max_ms": round1(gap_max),
        "gaps_over_250ms": gaps,
        "prefetch_downloaded_bytes": stats.downloaded_bytes,
        "upstream_restarts": stats.upstream_restarts,
        "errors": stats.errors,
        "fatal": stats.fatal,
        "fatal_reason": stats.fatal_reason,
        "consecutive_failures": stats.consecutive_failures,
        "read_error": read_error,
    }))
}

/// 直连上游基线（绕过代理）：用于区分「真实网络慢」与「代理开销」。
async fn baseline_direct(
    client: &reqwest::Client,
    link: &DirectLink,
    read_bytes: u64,
) -> Result<serde_json::Value, String> {
    let want = read_bytes.min(link.size.max(1));
    let t0 = Instant::now();
    let mut req = client.get(&link.url);
    for (k, v) in &link.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return Ok(serde_json::json!({
                "error": format!("直连基线请求失败：{e}"),
                "headers_ms": round1(t0.elapsed().as_secs_f64() * 1000.0),
            }))
        }
    };
    if !resp.status().is_success() {
        return Ok(serde_json::json!({
            "status": resp.status().as_u16(),
            "error": format!("直连基线返回 HTTP {}", resp.status()),
            "headers_ms": round1(t0.elapsed().as_secs_f64() * 1000.0),
        }));
    }
    let headers_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let mut resp = resp;
    let mut got = 0u64;
    let mut body_ttfb: Option<f64> = None;
    let mut read_error: Option<String> = None;
    while got < want {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if body_ttfb.is_none() {
                    body_ttfb = Some(t0.elapsed().as_secs_f64() * 1000.0);
                }
                got += chunk.len() as u64;
            }
            Ok(None) => break,
            Err(e) => {
                read_error = Some(e.to_string());
                break;
            }
        }
    }
    let secs = t0.elapsed().as_secs_f64().max(0.001);
    Ok(serde_json::json!({
        "read_bytes": got,
        "want_bytes": want,
        "headers_ms": round1(headers_ms),
        "body_ttfb_ms": round1(body_ttfb.unwrap_or(0.0)),
        "total_ms": round1(secs * 1000.0),
        "throughput_mbps": round2(got as f64 * 8.0 / secs / 1_000_000.0),
        "read_error": read_error,
    }))
}

fn stats_json(st: &wp_stream::StreamStats) -> serde_json::Value {
    serde_json::json!({
        "downloaded_bytes": st.downloaded_bytes,
        "upstream_restarts": st.upstream_restarts,
        "errors": st.errors,
        "fatal": st.fatal,
        "fatal_reason": st.fatal_reason,
        "consecutive_failures": st.consecutive_failures,
    })
}

fn token_of(url: &str) -> String {
    url.rsplit('/').next().unwrap_or("").to_string()
}

/// 预读：打开后轮询统计，记录预读窗口填满时间。
async fn prefetch_fill(
    bench: &Bench,
    token: &str,
    total: u64,
    readahead: u64,
) -> Result<serde_json::Value, String> {
    let target = readahead.min(total);
    if target == 0 {
        return Ok(serde_json::json!({"enabled": false, "target_bytes": 0}));
    }
    let t0 = Instant::now();
    let mut reached = 0u64;
    let mut last_downloaded: Option<u64> = None;
    let mut last_t = Instant::now();
    let mut peak_mbps = 0f64;
    loop {
        if let Some(stats) = bench.proxy.stats(token) {
            reached = reached.max(stats.buffered_bytes);
            match last_downloaded {
                None => {
                    last_downloaded = Some(stats.downloaded_bytes);
                    last_t = Instant::now();
                }
                Some(prev) if stats.downloaded_bytes > prev => {
                    if last_t.elapsed() >= Duration::from_millis(100) {
                        let dt = last_t.elapsed().as_secs_f64().max(0.001);
                        let delta = stats.downloaded_bytes - prev;
                        peak_mbps = peak_mbps.max(delta as f64 * 8.0 / dt / 1_000_000.0);
                        last_downloaded = Some(stats.downloaded_bytes);
                        last_t = Instant::now();
                    }
                }
                _ => {}
            }
            if reached >= target {
                break;
            }
        }
        if t0.elapsed() > PREFETCH_TIMEOUT {
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    Ok(serde_json::json!({
        "enabled": true,
        "target_bytes": target,
        "reached_bytes": reached,
        "fill_ms": round1(t0.elapsed().as_secs_f64() * 1000.0),
        "peak_mbps": round2(peak_mbps),
        "filled": reached >= target,
    }))
}

/// seek 测试：在 readahead=0 的独立会话上测「冷未命中 → 重复命中」，
/// 再加一个默认配置会话验证回看命中。
async fn seek_test(
    link: DirectLink,
    cache_dir: &Path,
    total: u64,
    seek_bytes: u64,
    opts: &Options,
) -> Result<serde_json::Value, String> {
    let window = seek_bytes.max(1).min(total.max(1));
    let mid = total / 2;
    let back = total.saturating_sub(window);
    let positions: Vec<(&str, u64)> = vec![("前", 0), ("中", mid), ("后", back), ("回看-前", 0)];
    // 冷/热：readahead=0 保证「首次」是网络冷读，重复请求验证缓存命中
    let cold_cfg = StreamConfig {
        cache_dir: cache_dir.join("seek-cold"),
        chunk_bytes: opts.chunk,
        readahead_bytes: 0,
        max_cache_bytes: opts.lru,
        parallel_fetches: opts.parallel.max(1),
        adaptive_chunk: opts.adaptive,
        startup_first_bytes: opts.startup_first,
        seek_priority: opts.seek_priority,
        failure_breaker_threshold: opts.breaker,
        max_link_restarts: opts.max_restarts,
        ..StreamConfig::default()
    };
    let bench = Bench::new(cold_cfg)?;
    let handle = bench.open(link.clone(), &opts.remote_path).await?;
    let token = handle.token.clone();
    let mut rows = Vec::new();
    for (label, offset) in &positions {
        let before = bench
            .proxy
            .stats(&token)
            .map(|s| s.downloaded_bytes)
            .unwrap_or(0);
        let (_, ttfb, total_ms) = read_range(
            &bench.client,
            &handle.url,
            *offset,
            (*offset + window - 1).min(total - 1),
        )
        .await?;
        let after = bench
            .proxy
            .stats(&token)
            .map(|s| s.downloaded_bytes)
            .unwrap_or(0);
        let delta = after.saturating_sub(before);
        rows.push(serde_json::json!({
            "pos": label, "offset": offset, "phase": "cold",
            "ttfb_ms": round1(ttfb), "total_ms": round1(total_ms),
            "cache_hit": delta == 0, "downloaded_delta": delta,
        }));
        // 重复同一位置 → 应命中磁盘缓存
        let before2 = after;
        let (_, ttfb2, total2) = read_range(
            &bench.client,
            &handle.url,
            *offset,
            (*offset + window - 1).min(total - 1),
        )
        .await?;
        let after2 = bench
            .proxy
            .stats(&token)
            .map(|s| s.downloaded_bytes)
            .unwrap_or(0);
        rows.push(serde_json::json!({
            "pos": label, "offset": offset, "phase": "warm",
            "ttfb_ms": round1(ttfb2), "total_ms": round1(total2),
            "cache_hit": after2.saturating_sub(before2) == 0,
            "downloaded_delta": after2.saturating_sub(before2),
        }));
    }
    bench.finish().await;

    // 默认配置（带预读）下的回看：先读前段，等预读填满窗口，再回看前段应命中
    let warm_cfg = StreamConfig {
        cache_dir: cache_dir.join("seek-warm"),
        chunk_bytes: opts.chunk,
        readahead_bytes: opts.readahead,
        max_cache_bytes: opts.lru,
        parallel_fetches: opts.parallel.max(1),
        adaptive_chunk: opts.adaptive,
        startup_first_bytes: opts.startup_first,
        seek_priority: opts.seek_priority,
        failure_breaker_threshold: opts.breaker,
        max_link_restarts: opts.max_restarts,
        ..StreamConfig::default()
    };
    let bench2 = Bench::new(warm_cfg)?;
    let handle2 = bench2.open(link, &opts.remote_path).await?;
    let token2 = handle2.token.clone();
    let review = if opts.readahead > 0 {
        let _ = read_range(&bench2.client, &handle2.url, 0, window.saturating_sub(1)).await?;
        tokio::time::sleep(Duration::from_millis(800)).await;
        let before = bench2
            .proxy
            .stats(&token2)
            .map(|s| s.downloaded_bytes)
            .unwrap_or(0);
        let (_, ttfb, _) =
            read_range(&bench2.client, &handle2.url, 0, window.saturating_sub(1)).await?;
        let after = bench2
            .proxy
            .stats(&token2)
            .map(|s| s.downloaded_bytes)
            .unwrap_or(0);
        serde_json::json!({
            "readahead": opts.readahead,
            "review_ttfb_ms": round1(ttfb),
            "cache_hit": after.saturating_sub(before) == 0,
        })
    } else {
        serde_json::json!({"readahead": 0, "note": "预读关闭，回看命中依赖已读数据"})
    };
    bench2.finish().await;
    Ok(serde_json::json!({"rows": rows, "warm_review": review}))
}

/// 模拟播放（paced）：按 `play_bps` 消费，统计阻塞超过阈值的「卡顿」次数。
async fn paced_play(
    bench: &Bench,
    url: &str,
    total: u64,
    play_bps: u64,
    seconds: f64,
    play_chunk: u64,
) -> Result<serde_json::Value, String> {
    let t0 = Instant::now();
    let startup_needed = (play_chunk.saturating_mul(2)).min(total).max(1);
    let (startup_body, startup_ttfb, _) =
        read_range(&bench.client, url, 0, startup_needed - 1).await?;
    let startup_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let mut pos = startup_body.len() as u64;
    let play_start = Instant::now();
    let mut stalls = 0u64;
    let mut stall_ms_total = 0f64;
    let mut max_req_ms = 0f64;
    let mut requests = 0u64;
    while play_start.elapsed().as_secs_f64() < seconds && pos < total {
        // 按码率节流：播放到 pos 所需时间
        let due = Duration::from_secs_f64(pos as f64 * 8.0 / play_bps as f64);
        if due > play_start.elapsed() {
            tokio::time::sleep(due - play_start.elapsed()).await;
        }
        let end = (pos + play_chunk - 1).min(total - 1);
        let req_start = Instant::now();
        let ok = read_range(&bench.client, url, pos, end).await;
        let req_ms = req_start.elapsed().as_secs_f64() * 1000.0;
        let got = ok.map(|(b, _, _)| b.len() as u64).unwrap_or(0);
        max_req_ms = max_req_ms.max(req_ms);
        requests += 1;
        if req_start.elapsed() > STALL_THRESHOLD {
            stalls += 1;
            stall_ms_total += (req_start.elapsed() - STALL_THRESHOLD).as_secs_f64() * 1000.0;
        }
        if got == 0 {
            break;
        }
        pos += got;
    }
    let elapsed = play_start.elapsed().as_secs_f64();
    Ok(serde_json::json!({
        "play_bps": play_bps,
        "target_seconds": seconds,
        "actual_seconds": round1(elapsed),
        "startup_ttfb_ms": round1(startup_ttfb),
        "startup_total_ms": round1(startup_ms),
        "requests": requests,
        "stalls": stalls,
        "stall_ms_total": round1(stall_ms_total),
        "max_req_ms": round1(max_req_ms),
        "consumed_bytes": pos,
        "avg_mbps": round2(pos as f64 * 8.0 / elapsed.max(0.001) / 1_000_000.0),
    }))
}

// ───────────────────────── HLS ─────────────────────────

struct HlsSegment {
    duration: f64,
    url: String,
}

fn join_url(base: &str, line: &str) -> String {
    if line.starts_with("http://") || line.starts_with("https://") {
        return line.to_string();
    }
    let dir = base.rsplit_once('/').map(|(d, _)| d).unwrap_or(base);
    format!("{dir}/{line}")
}

async fn hls_get(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
) -> Result<(Vec<u8>, f64, f64), String> {
    let t0 = Instant::now();
    let mut req = client.get(url).timeout(Duration::from_secs(60));
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.map_err(|e| format!("HLS 请求失败：{e}"))?;
    let status = resp.status();
    let ttfb = t0.elapsed().as_secs_f64() * 1000.0;
    if !status.is_success() {
        return Err(format!("HLS {url} 返回 {status}"));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("HLS 读 body 失败：{e}"))?;
    let total_ms = t0.elapsed().as_secs_f64() * 1000.0;
    Ok((body.to_vec(), ttfb, total_ms))
}

async fn bench_hls(
    link: &DirectLink,
    play_bps: u64,
    seconds: f64,
) -> Result<serde_json::Value, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("HLS HTTP 客户端失败：{e}"))?;
    let headers = link.headers.clone();
    let t0 = Instant::now();
    let (body, m3u8_ttfb, m3u8_total) = hls_get(&client, &link.url, &headers).await?;
    let text = String::from_utf8_lossy(&body).to_string();
    if !text.contains("#EXTM3U") {
        return Err("响应不是 m3u8 播放列表".into());
    }
    let mut segments: Vec<HlsSegment> = Vec::new();
    let mut pending_dur: Option<f64> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("#EXTINF:") {
            pending_dur = v
                .split(',')
                .next()
                .and_then(|s| s.trim().parse::<f64>().ok());
        } else if !line.is_empty() && !line.starts_with('#') {
            if let Some(dur) = pending_dur.take() {
                segments.push(HlsSegment {
                    duration: dur,
                    url: join_url(&link.url, line),
                });
            }
        }
    }
    if segments.is_empty() {
        return Err("m3u8 里没有分片（可能是 master playlist）".into());
    }
    let duration: f64 = segments.iter().map(|s| s.duration).sum();

    // 首分片（起播关键路径）
    let (first_body, first_ttfb, first_ms) = hls_get(&client, &segments[0].url, &headers).await?;
    // 稳态带宽：再连拉 2 个分片
    let mut steady_bytes = 0u64;
    let steady_t0 = Instant::now();
    let mut steady_fetch = 0u32;
    for seg in segments.iter().skip(1).take(2) {
        if let Ok((b, _, _)) = hls_get(&client, &seg.url, &headers).await {
            steady_bytes += b.len() as u64;
            steady_fetch += 1;
        }
    }
    let steady_secs = steady_t0.elapsed().as_secs_f64().max(0.001);
    // seek：跳到 40% 位置对应分片
    let target = duration * 0.4;
    let mut acc = 0f64;
    let mut seek_idx = 0usize;
    for (i, s) in segments.iter().enumerate() {
        if acc + s.duration > target {
            seek_idx = i;
            break;
        }
        acc += s.duration;
    }
    let seek_t0 = Instant::now();
    let seek_body = hls_get(&client, &segments[seek_idx].url, &headers).await;
    let seek_ms = seek_t0.elapsed().as_secs_f64() * 1000.0;
    let seek_bytes = seek_body.as_ref().map(|(b, _, _)| b.len()).unwrap_or(0);

    // 估算「播放 seconds 秒」需要的流量与实测拉流速度对比
    let need_bytes = (play_bps as f64 / 8.0 * seconds) as u64;

    Ok(serde_json::json!({
        "m3u8": {
            "http_total_ms": round1(m3u8_total),
            "http_ttfb_ms": round1(m3u8_ttfb),
            "bytes": body.len(),
            "segments": segments.len(),
            "duration_secs": round2(duration),
            "target_duration_hint": segments.first().map(|s| s.duration),
        },
        "startup": {
            "first_segment_bytes": first_body.len(),
            "first_segment_ttfb_ms": round1(first_ttfb),
            "first_segment_ms": round1(first_ms),
            "estimated_startup_ms": round1(m3u8_total + first_ms),
            "first_segment_mbps": round2(first_body.len() as f64 * 8.0 * 1000.0 / first_ms.max(0.001) / 1_000_000.0),
        },
        "steady": {
            "segments_fetched": steady_fetch,
            "bytes": steady_bytes,
            "mbps": round2(steady_bytes as f64 * 8.0 / steady_secs / 1_000_000.0),
        },
        "seek_40pct": {
            "segment_index": seek_idx,
            "target_secs": round2(target),
            "ttfb_ms": round1(seek_ms),
            "bytes": seek_bytes,
            "ok": seek_body.is_ok(),
            "single_segment_seek": true,
        },
        "play_load": {
            "play_bps": play_bps,
            "seconds": seconds,
            "need_bytes": need_bytes,
            "need_startup_buffer_secs_if_64mb": round2(64.0 * 1024.0 * 1024.0 * 8.0 / play_bps as f64),
        },
        "total_probe_ms": round1(t0.elapsed().as_secs_f64() * 1000.0),
    }))
}

// ───────────────────────── matrix ─────────────────────────

fn matrix_configs() -> Vec<(u64, u64, usize)> {
    vec![
        (1 * 1024 * 1024, 32 * 1024 * 1024, 1),
        (4 * 1024 * 1024, 64 * 1024 * 1024, 2),
        (8 * 1024 * 1024, 64 * 1024 * 1024, 4),
        (4 * 1024 * 1024, 128 * 1024 * 1024, 2),
        (4 * 1024 * 1024, 0, 2),
    ]
}

async fn run_matrix(
    link: DirectLink,
    base_dir: &Path,
    opts: &Options,
) -> Result<serde_json::Value, String> {
    let total = link.size;
    let mut rows = Vec::new();
    for (idx, (chunk, readahead, parallel)) in matrix_configs().into_iter().enumerate() {
        let mk = |sub: &str, chunk: u64, readahead: u64, parallel: usize| StreamConfig {
            cache_dir: base_dir.join(format!("m{idx}")).join(sub),
            chunk_bytes: chunk,
            readahead_bytes: readahead,
            max_cache_bytes: opts.lru,
            parallel_fetches: parallel.max(1),
            adaptive_chunk: opts.adaptive,
            startup_first_bytes: opts.startup_first,
            seek_priority: opts.seek_priority,
            failure_breaker_threshold: opts.breaker,
            max_link_restarts: opts.max_restarts,
            ..StreamConfig::default()
        };

        // 会话 A：顺序读（全新会话，先 seq 再预读，避免把缓存命中误当网络吞吐）
        let bench = Bench::new(mk("seq", chunk, readahead, parallel))?;
        let t = Instant::now();
        let handle = bench.open(link.clone(), &opts.remote_path).await?;
        let open_ms = t.elapsed().as_secs_f64() * 1000.0;
        let seq = seq_read(&bench, &handle.url, total, opts.read_bytes).await?;
        let fill = prefetch_fill(&bench, &handle.token, total, readahead).await?;
        bench.finish().await;

        // 会话 B：起播（全新会话，Range 0-64KB；代理需先备好首块）
        let bench_b = Bench::new(mk("startup", chunk, readahead, parallel))?;
        let handle_b = bench_b.open(link.clone(), &opts.remote_path).await?;
        let (_, ttfb, _) =
            read_range(&bench_b.client, &handle_b.url, 0, 65535.min(total - 1)).await?;
        bench_b.finish().await;

        // 会话 C：冷 seek（全新会话；中段不在预读窗口内）→ 重复 warm
        let bench_c = Bench::new(mk("seek", chunk, readahead, parallel))?;
        let handle_c = bench_c.open(link.clone(), &opts.remote_path).await?;
        let mid = total / 2;
        let w = opts.seek_bytes.max(1).min(total.max(1));
        let before = bench_c
            .proxy
            .stats(&handle_c.token)
            .map(|s| s.downloaded_bytes)
            .unwrap_or(0);
        let (_, cold_ttfb, _) = read_range(
            &bench_c.client,
            &handle_c.url,
            mid,
            (mid + w - 1).min(total - 1),
        )
        .await?;
        let mid_delta = bench_c
            .proxy
            .stats(&handle_c.token)
            .map(|s| s.downloaded_bytes)
            .unwrap_or(0)
            .saturating_sub(before);
        let before2 = bench_c
            .proxy
            .stats(&handle_c.token)
            .map(|s| s.downloaded_bytes)
            .unwrap_or(0);
        let (_, warm_ttfb, _) = read_range(
            &bench_c.client,
            &handle_c.url,
            mid,
            (mid + w - 1).min(total - 1),
        )
        .await?;
        let warm_delta = bench_c
            .proxy
            .stats(&handle_c.token)
            .map(|s| s.downloaded_bytes)
            .unwrap_or(0)
            .saturating_sub(before2);
        bench_c.finish().await;

        rows.push(serde_json::json!({
            "chunk_bytes": chunk,
            "readahead_bytes": readahead,
            "parallel": parallel,
            "open_ms": round1(open_ms),
            "startup_first_range_ttfb_ms": round1(ttfb),
            "seq": seq,
            "prefetch": fill,
            "seek_mid_cold_ttfb_ms": round1(cold_ttfb),
            "seek_mid_cold_hit": mid_delta == 0,
            "seek_mid_warm_ttfb_ms": round1(warm_ttfb),
            "seek_mid_warm_hit": warm_delta == 0,
        }));
    }
    Ok(serde_json::json!({ "rows": rows }))
}

// ───────────────────────── main ─────────────────────────

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn host_of(url: &str) -> String {
    let Some((_, rest)) = url.split_once("://") else {
        return String::new();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    rest[..end].to_string()
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("ERROR: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let opts = parse_args()?;
    let text = std::fs::read_to_string(&opts.link_file)
        .map_err(|e| format!("读 {} 失败：{e}", opts.link_file.display()))?;
    let link: DirectLink =
        serde_json::from_str(&text).map_err(|e| format!("解析直链 JSON 失败：{e}"))?;
    if link.url.trim().is_empty() {
        return Err("直链为空".into());
    }
    let total = link.size;
    if total == 0 && !link.is_hls {
        return Err("直链 size=0，无法做代理压测（HLS 请用 --mode hls）".into());
    }

    let base_dir = std::env::temp_dir().join(format!(
        "wp-stream-bench-{}-{}",
        std::process::id(),
        wp_core::util::now_ts()
    ));
    std::fs::create_dir_all(&base_dir).map_err(|e| format!("建缓存目录失败：{e}"))?;

    let mut result = serde_json::json!({
        "env": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "wp_stream": env!("CARGO_PKG_VERSION"),
            "timestamp": wp_core::util::now_ts(),
            "note": "本机真实网络；弱网由本地限速中继模拟；测量含并发大负载机器影响",
        },
        "link": {
            "host": host_of(&link.url),
            "size": total,
            "is_hls": link.is_hls,
            "file_name": link.file_name,
            "header_names": link.headers.iter().map(|(k,_)| k.clone()).collect::<Vec<_>>(),
            "expire_at": link.expire_at,
            "path": opts.remote_path,
        },
        "config": {
            "chunk_bytes": opts.chunk,
            "readahead_bytes": opts.readahead,
            "parallel": opts.parallel,
            "lru_bytes": opts.lru,
            "play_bps": opts.play_bps,
            "adaptive_chunk": opts.adaptive,
            "startup_first_bytes": opts.startup_first,
            "seek_priority": opts.seek_priority,
            "failure_breaker_threshold": opts.breaker,
            "max_link_restarts": opts.max_restarts,
        },
    });

    // HLS 分支（HLS 直链不经过 Range 代理）
    if link.is_hls || opts.mode == Mode::Hls {
        if !link.is_hls {
            return Err("--mode hls 需要输入 is_hls=true 的直链 JSON".into());
        }
        let hls = bench_hls(&link, opts.play_bps, opts.seconds).await?;
        result["hls"] = hls;
        let _ = std::fs::remove_dir_all(&base_dir);
        return emit(result, opts.out.as_deref());
    }

    // matrix 模式
    if opts.mode == Mode::Matrix {
        let matrix = run_matrix(link, &base_dir, &opts).await?;
        result["matrix"] = matrix;
        let _ = std::fs::remove_dir_all(&base_dir);
        return emit(result, opts.out.as_deref());
    }

    let make_cfg = |sub: &str| StreamConfig {
        cache_dir: base_dir.join(sub),
        chunk_bytes: opts.chunk,
        readahead_bytes: opts.readahead,
        max_cache_bytes: opts.lru,
        parallel_fetches: opts.parallel.max(1),
        adaptive_chunk: opts.adaptive,
        startup_first_bytes: opts.startup_first,
        seek_priority: opts.seek_priority,
        failure_breaker_threshold: opts.breaker,
        max_link_restarts: opts.max_restarts,
        ..StreamConfig::default()
    };

    // mpv 端到端冒烟：代理 URL + 系统 mpv（headless）→ 起播耗时（区分代理慢 vs 渲染慢）
    if opts.mode == Mode::Mpv {
        let bench = Bench::new(make_cfg("mpv"))?;
        let t = Instant::now();
        let handle = bench.open(link.clone(), &opts.remote_path).await?;
        let open_ms = t.elapsed().as_secs_f64() * 1000.0;
        let url = handle.url.clone();
        let frames = 2u32;
        let (rc, log) = tokio::task::spawn_blocking(move || {
            let out = std::process::Command::new("mpv")
                .args([
                    "--no-config",
                    "--vo=null",
                    "--ao=null",
                    "--ytdl=no",
                    "--msg-level=all=info",
                    "--network-timeout=120",
                    &format!("--frames={frames}"),
                ])
                .arg(&url)
                .output();
            match out {
                Ok(o) => {
                    let mut log = String::from_utf8_lossy(&o.stderr).to_string();
                    log.push_str(&String::from_utf8_lossy(&o.stdout));
                    (o.status.code().unwrap_or(-1), log)
                }
                Err(e) => (-2, format!("spawn mpv 失败：{e}")),
            }
        })
        .await
        .map_err(|e| format!("mpv 任务失败：{e}"))?;
        let elapsed_ms = t.elapsed().as_secs_f64() * 1000.0;
        let stats = bench.proxy.stats(&handle.token).unwrap_or_default();
        let hits: Vec<String> = log
            .lines()
            .filter(|l| {
                l.contains("VO:")
                    || l.contains("AO:")
                    || l.contains("Failed")
                    || l.contains("error")
            })
            .take(8)
            .map(|s| s.chars().take(160).collect())
            .collect();
        result["mpv"] = serde_json::json!({
            "frames": frames,
            "exit_code": rc,
            "cold_start_ms": round1(elapsed_ms),
            "proxy_open_ms": round1(open_ms),
            "adaptive_chunk": opts.adaptive,
            "startup_first_bytes": opts.startup_first,
            "stats": {
                "bootstrap_bytes": stats.bootstrap_bytes,
                "bootstrap_mbps": round1(stats.bootstrap_mbps),
                "weak_network": stats.weak_network,
                "effective_readahead_bytes": stats.effective_readahead_bytes,
                "downloaded_bytes": stats.downloaded_bytes,
                "errors": stats.errors,
            },
            "log_hits": hits,
        });
        bench.proxy.close(&handle.token);
        bench.finish().await;
        let _ = std::fs::remove_dir_all(&base_dir);
        return emit(result, opts.out.as_deref());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("基线 HTTP 客户端失败：{e}"))?;

    // ── 0) 直连上游基线（绕过代理，区分「网络慢」与「代理开销」） ──
    if matches!(opts.mode, Mode::All | Mode::Seq) && opts.limit_bps == 0 {
        result["direct_upstream"] = baseline_direct(&client, &link, opts.read_bytes).await?;
    }

    // ── 弱网：把真实直链换成限速中继 ──
    let mut relay: Option<ThrottledRelay> = None;
    let mut effective_link = link.clone();
    if opts.limit_bps > 0 || opts.mode == Mode::Weak {
        let limit = if opts.limit_bps > 0 {
            opts.limit_bps
        } else {
            20_000_000
        };
        let r = start_throttle_relay(link.clone(), limit).await?;
        effective_link = DirectLink {
            url: r.url.clone(),
            headers: Vec::new(),
            expire_at: None,
            size: link.size,
            file_name: link.file_name.clone(),
            is_hls: false,
        };
        result["weak"] = serde_json::json!({
            "limit_bps": limit,
            "limit_mbps": round2(limit as f64 / 1_000_000.0),
            "relay": "local-token-bucket",
        });
        relay = Some(r);
    }

    // ── 1) 顺序读 + 预读（同一会话，接近真实「打开后连续观看」） ──
    if matches!(opts.mode, Mode::All | Mode::Seq | Mode::Prefetch) {
        let bench = Bench::new(make_cfg("seq"))?;
        let t = Instant::now();
        let handle = bench
            .open(effective_link.clone(), &opts.remote_path)
            .await?;
        result["open_ms"] = serde_json::json!(round1(t.elapsed().as_secs_f64() * 1000.0));
        result["proxy_url"] = serde_json::json!(handle.url.replace(&handle.token, "<token>"));
        let token = handle.token.clone();
        if matches!(opts.mode, Mode::All | Mode::Seq) {
            result["seq"] = seq_read(&bench, &handle.url, total, opts.read_bytes).await?;
        }
        if matches!(opts.mode, Mode::All | Mode::Prefetch) {
            result["prefetch"] = prefetch_fill(&bench, &token, total, opts.readahead).await?;
        }
        bench.proxy.close(&token);
        bench.finish().await;
    }

    // ── 2) 起播（Range 0-64KB，全新会话，测代理首块就绪代价） ──
    if matches!(opts.mode, Mode::All | Mode::Seq | Mode::Paced | Mode::Weak) {
        let bench = Bench::new(make_cfg("startup"))?;
        let handle = bench
            .open(effective_link.clone(), &opts.remote_path)
            .await?;
        match read_range(&bench.client, &handle.url, 0, 65535.min(total - 1)).await {
            Ok((_, ttfb, total_ms)) => {
                result["startup_first_range"] = serde_json::json!({
                    "range": format!("bytes=0-{}", 65535.min(total - 1)),
                    "ttfb_ms": round1(ttfb),
                    "total_ms": round1(total_ms),
                    "note": "自适应开启时先回 headers；关闭时 ttfb≈首块下载时间",
                })
            }
            Err(e) => {
                let st = bench.proxy.stats(&handle.token).unwrap_or_default();
                result["startup_first_range"] = serde_json::json!({
                    "error": e,
                    "stats": stats_json(&st),
                });
            }
        }
        bench.finish().await;
    }

    // ── 3) 模拟播放（全新会话，按 4K 码率消费，统计卡顿） ──
    if matches!(opts.mode, Mode::All | Mode::Paced | Mode::Weak) {
        let bench = Bench::new(make_cfg("paced"))?;
        let t = Instant::now();
        let handle = bench
            .open(effective_link.clone(), &opts.remote_path)
            .await?;
        let open_ms = t.elapsed().as_secs_f64() * 1000.0;
        let paced = paced_play(
            &bench,
            &handle.url,
            total,
            opts.play_bps,
            opts.seconds,
            opts.chunk,
        )
        .await?;
        let key = if opts.mode == Mode::Weak || opts.limit_bps > 0 {
            "paced_weak"
        } else {
            "paced"
        };
        result[key] = paced;
        result["paced_open_ms"] = serde_json::json!(round1(open_ms));
        bench.finish().await;
    }

    // ── 4) seek（内部使用全新会话：前/中/后 + 回看，冷未命中 vs 热命中） ──
    if matches!(opts.mode, Mode::All | Mode::Seek) {
        result["seek"] = seek_test(link.clone(), &base_dir, total, opts.seek_bytes, &opts).await?;
    }

    if let Some(r) = relay {
        drop(r);
    }
    let _ = std::fs::remove_dir_all(&base_dir);
    emit(result, opts.out.as_deref())
}

fn emit(result: serde_json::Value, out: Option<&Path>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?;
    if let Some(path) = out {
        std::fs::write(path, &text).map_err(|e| format!("写结果失败：{e}"))?;
    }
    println!("{text}");
    Ok(())
}
