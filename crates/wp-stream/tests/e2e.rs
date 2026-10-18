//! wp-stream 端到端测试：本地 mock 上游（tokio TcpListener 实现 Range 服务）+ mock LinkProvider。
//!
//! 覆盖：
//! * HTTP 语义：GET/HEAD、单段 Range、开区间、suffix、多段 400、416、未知 token 404、keep-alive；
//! * 缓存：同范围第二次请求不再打上游、跨块 Range、后台预读；
//! * 换链：上游 403 → 重新 direct_link → 续传成功；换链次数耗尽 → 502；expire_at 到期主动换链；
//! * 上游忽略 Range（200）→ 整文件回退裁剪；
//! * 探测：probe_with_provider 对 206 / 200 两种上游的结论。
//!
//! 说明：不联网、不碰真实网盘；缓存目录全部在系统临时目录并在测试结束删除。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use wp_core::model::DirectLink;
use wp_stream::range;
use wp_stream::{probe_with_provider, LinkProvider, StreamConfig, StreamProxy};

// ───────────────────────── 测试临时目录 ─────────────────────────

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("wp-stream-e2e-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).expect("创建测试临时目录");
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ───────────────────────── mock 上游 ─────────────────────────

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    #[allow(dead_code)]
    path: String,
    headers: HashMap<String, String>,
}

#[derive(Default)]
struct MockState {
    /// 固定返回的状态码（非空时优先于正常处理），用于制造 403/500。
    forced_status: Mutex<Option<u16>>,
    /// 记录收到的请求。
    requests: Mutex<Vec<RecordedRequest>>,
}

impl MockState {
    fn hits(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    fn set_forced_status(&self, status: Option<u16>) {
        *self.forced_status.lock().unwrap() = status;
    }
    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

struct MockUpstream {
    url: String,
    state: Arc<MockState>,
    task: tokio::task::JoinHandle<()>,
}

impl MockUpstream {
    async fn start(body: Vec<u8>, supports_range: bool) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(MockState::default());
        let body = Arc::new(body);
        let st = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let st = st.clone();
                let body = body.clone();
                tokio::spawn(async move {
                    serve_mock_conn(stream, st, body, supports_range).await;
                });
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}/file"),
            state,
            task,
        }
    }
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_mock_conn(
    mut stream: TcpStream,
    state: Arc<MockState>,
    body: Arc<Vec<u8>>,
    supports_range: bool,
) {
    let Some(req) = read_mock_request(&mut stream).await else {
        return;
    };
    state.requests.lock().unwrap().push(req.clone());
    let forced = *state.forced_status.lock().unwrap();
    if let Some(code) = forced {
        let reason = match code {
            403 => "Forbidden",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "Error",
        };
        let resp =
            format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.shutdown().await;
        return;
    }

    let mut extra_headers = String::new();
    let mut body_bytes: Vec<u8> = Vec::new();
    let mut status = 200u16;
    let mut reason = "OK";
    let mut content_range: Option<String> = None;
    let is_head = req.method == "HEAD";
    if is_head {
        if supports_range {
            extra_headers.push_str("Accept-Ranges: bytes\r\n");
        }
    } else {
        let mut served = false;
        if let Some(value) = req.headers.get("range").cloned() {
            if supports_range {
                if let Ok(spec) = range::parse_range_header(&value) {
                    if let Ok((start, end)) = range::resolve_range(spec, body.len() as u64) {
                        status = 206;
                        reason = "Partial Content";
                        content_range = Some(range::content_range(start, end, body.len() as u64));
                        body_bytes = body[start as usize..=end as usize].to_vec();
                        extra_headers.push_str("Accept-Ranges: bytes\r\n");
                        served = true;
                    }
                }
            }
        }
        if !served {
            body_bytes = body.as_ref().clone();
            if supports_range {
                extra_headers.push_str("Accept-Ranges: bytes\r\n");
            }
        }
    }
    let content_length = if is_head {
        body.len()
    } else {
        body_bytes.len()
    };
    let mut resp = format!("HTTP/1.1 {status} {reason}\r\n{extra_headers}");
    resp.push_str("Content-Type: video/mp4\r\n");
    if let Some(cr) = content_range {
        resp.push_str(&format!("Content-Range: {cr}\r\n"));
    }
    resp.push_str(&format!(
        "Content-Length: {content_length}\r\nConnection: close\r\n\r\n"
    ));
    let _ = stream.write_all(resp.as_bytes()).await;
    if !body_bytes.is_empty() {
        let _ = stream.write_all(&body_bytes).await;
    }
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

/// 从 mock 上游连接读取一个完整请求（到 `\r\n\r\n` 为止）。
async fn read_mock_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buf = Vec::new();
    loop {
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 64 * 1024 {
            return None;
        }
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let mut lines = head.split("\r\n");
            let req_line = lines.next()?;
            let mut parts = req_line.split_whitespace();
            let method = parts.next()?.to_string();
            let path = parts.next().unwrap_or("/").to_string();
            let mut headers = HashMap::new();
            for line in lines {
                if let Some((k, v)) = line.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                }
            }
            return Some(RecordedRequest {
                method,
                path,
                headers,
            });
        }
    }
}

// ───────────────────────── mock LinkProvider ─────────────────────────

struct MockProvider {
    /// 依次返回的 url（队列尾重复使用）。
    urls: Mutex<Vec<String>>,
    size: u64,
    expire_at: Mutex<Option<i64>>,
    calls: AtomicUsize,
}

impl MockProvider {
    fn new(url: String, size: u64) -> Arc<Self> {
        Arc::new(Self {
            urls: Mutex::new(vec![url]),
            size,
            expire_at: Mutex::new(None),
            calls: AtomicUsize::new(0),
        })
    }
    fn with_urls(urls: Vec<String>, size: u64) -> Arc<Self> {
        Arc::new(Self {
            urls: Mutex::new(urls),
            size,
            expire_at: Mutex::new(None),
            calls: AtomicUsize::new(0),
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn set_expire_at(&self, ts: Option<i64>) {
        *self.expire_at.lock().unwrap() = ts;
    }
}

#[async_trait::async_trait]
impl LinkProvider for MockProvider {
    fn cache_key(&self) -> String {
        "mock:test".to_string()
    }
    async fn direct_link(&self, _remote_path: &str) -> wp_core::Result<DirectLink> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let url = {
            let urls = self.urls.lock().unwrap();
            urls.get(n)
                .or_else(|| urls.last())
                .cloned()
                .unwrap_or_default()
        };
        Ok(DirectLink {
            url,
            headers: vec![
                ("X-Mock-Auth".to_string(), "secret".to_string()),
                ("Referer".to_string(), "https://mock/".to_string()),
            ],
            expire_at: *self.expire_at.lock().unwrap(),
            size: self.size,
            file_name: "sample.mp4".to_string(),
            is_hls: false,
        })
    }
}

// ───────────────────────── 工具 ─────────────────────────

fn body_of(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn test_config(cache_dir: &Path, chunk: u64, readahead: u64) -> StreamConfig {
    StreamConfig {
        cache_dir: cache_dir.to_path_buf(),
        readahead_bytes: readahead,
        max_cache_bytes: 256 * 1024 * 1024,
        chunk_bytes: chunk,
        parallel_fetches: 2,
        max_link_restarts: 3,
        request_timeout_secs: 5,
        connect_timeout_secs: 3,
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap()
}

async fn cleanup(proxy: &StreamProxy) {
    proxy.shutdown().await;
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

// ───────────────────────── HTTP 语义测试 ─────────────────────────

#[tokio::test]
async fn full_get_returns_200_and_whole_body() {
    let tmp = TempDir::new("full");
    let data = body_of(300_000);
    let mock = MockUpstream::start(data.clone(), true).await;
    let provider = MockProvider::new(mock.url.clone(), data.len() as u64);
    let proxy = StreamProxy::new(test_config(tmp.path(), 64 * 1024, 0));
    let handle = proxy.open_with_provider(provider, "/v.mp4").await.unwrap();
    assert_eq!(handle.total_bytes, data.len() as u64);

    let resp = client().get(&handle.url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["accept-ranges"], "bytes");
    assert_eq!(
        resp.headers()["content-length"],
        data.len().to_string().as_str()
    );
    let got = resp.bytes().await.unwrap();
    assert_eq!(got.as_ref(), data.as_slice());
    cleanup(&proxy).await;
}

#[tokio::test]
async fn range_get_returns_206_with_slice() {
    let tmp = TempDir::new("range");
    let data = body_of(100_000);
    let mock = MockUpstream::start(data.clone(), true).await;
    let provider = MockProvider::new(mock.url.clone(), data.len() as u64);
    let proxy = StreamProxy::new(test_config(tmp.path(), 64 * 1024, 0));
    let handle = proxy.open_with_provider(provider, "/v.mp4").await.unwrap();

    let resp = client()
        .get(&handle.url)
        .header("Range", "bytes=1000-1999")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    assert_eq!(
        resp.headers()["content-range"],
        format!("bytes 1000-1999/{}", data.len()).as_str()
    );
    let got = resp.bytes().await.unwrap();
    assert_eq!(got.as_ref(), &data[1000..=1999]);
    cleanup(&proxy).await;
}

#[tokio::test]
async fn open_ended_and_suffix_ranges() {
    let tmp = TempDir::new("ranges2");
    let data = body_of(10_000);
    let mock = MockUpstream::start(data.clone(), true).await;
    let provider = MockProvider::new(mock.url.clone(), data.len() as u64);
    let proxy = StreamProxy::new(test_config(tmp.path(), 4096, 0));
    let handle = proxy.open_with_provider(provider, "/v.mp4").await.unwrap();

    let resp = client()
        .get(&handle.url)
        .header("Range", "bytes=9990-")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    let got = resp.bytes().await.unwrap();
    assert_eq!(got.as_ref(), &data[9990..]);

    let resp = client()
        .get(&handle.url)
        .header("Range", "bytes=-100")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    assert_eq!(
        resp.headers()["content-range"],
        format!(
            "bytes {}-{}/{}",
            data.len() - 100,
            data.len() - 1,
            data.len()
        )
        .as_str()
    );
    let got = resp.bytes().await.unwrap();
    assert_eq!(got.as_ref(), &data[data.len() - 100..]);
    cleanup(&proxy).await;
}

#[tokio::test]
async fn head_returns_metadata_without_body() {
    let tmp = TempDir::new("head");
    let data = body_of(50_000);
    let mock = MockUpstream::start(data.clone(), true).await;
    let provider = MockProvider::new(mock.url.clone(), data.len() as u64);
    let proxy = StreamProxy::new(test_config(tmp.path(), 16 * 1024, 0));
    let handle = proxy.open_with_provider(provider, "/v.mp4").await.unwrap();

    let resp = client().head(&handle.url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["accept-ranges"], "bytes");
    assert_eq!(
        resp.headers()["content-length"],
        data.len().to_string().as_str()
    );
    let got = resp.bytes().await.unwrap();
    assert!(got.is_empty());
    cleanup(&proxy).await;
}

#[tokio::test]
async fn multi_range_is_rejected_with_400() {
    let tmp = TempDir::new("multi");
    let data = body_of(10_000);
    let mock = MockUpstream::start(data.clone(), true).await;
    let provider = MockProvider::new(mock.url.clone(), data.len() as u64);
    let proxy = StreamProxy::new(test_config(tmp.path(), 4096, 0));
    let handle = proxy.open_with_provider(provider, "/v.mp4").await.unwrap();

    let resp = client()
        .get(&handle.url)
        .header("Range", "bytes=0-1,5-6")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    cleanup(&proxy).await;
}

#[tokio::test]
async fn unsatisfiable_range_returns_416() {
    let tmp = TempDir::new("416");
    let data = body_of(10_000);
    let mock = MockUpstream::start(data.clone(), true).await;
    let provider = MockProvider::new(mock.url.clone(), data.len() as u64);
    let proxy = StreamProxy::new(test_config(tmp.path(), 4096, 0));
    let handle = proxy.open_with_provider(provider, "/v.mp4").await.unwrap();

    let resp = client()
        .get(&handle.url)
        .header("Range", "bytes=999999-")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 416);
    assert_eq!(
        resp.headers()["content-range"],
        format!("bytes */{}", data.len()).as_str()
    );
    cleanup(&proxy).await;
}

#[tokio::test]
async fn unknown_token_returns_404_and_other_paths_404() {
    let tmp = TempDir::new("404");
    let data = body_of(1000);
    let mock = MockUpstream::start(data.clone(), true).await;
    let provider = MockProvider::new(mock.url.clone(), data.len() as u64);
    let proxy = StreamProxy::new(test_config(tmp.path(), 4096, 0));
    let handle = proxy.open_with_provider(provider, "/v.mp4").await.unwrap();
    let port = proxy.port().unwrap();

    let resp = client()
        .get(format!("http://127.0.0.1:{port}/s/deadbeef"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    let resp = client()
        .get(format!("http://127.0.0.1:{port}/not-a-stream"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    let resp = client().head(&handle.url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    cleanup(&proxy).await;
}

// ───────────────────────── 缓存测试 ─────────────────────────

#[tokio::test]
async fn cache_hit_avoids_second_upstream_fetch() {
    let tmp = TempDir::new("cachehit");
    let data = body_of(200_000);
    let mock = MockUpstream::start(data.clone(), true).await;
    let provider = MockProvider::new(mock.url.clone(), data.len() as u64);
    let proxy = StreamProxy::new(test_config(tmp.path(), 64 * 1024, 0));
    let handle = proxy.open_with_provider(provider, "/v.mp4").await.unwrap();

    let resp = client()
        .get(&handle.url)
        .header("Range", "bytes=0-999")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    let _ = resp.bytes().await.unwrap();
    let hits_after_first = mock.state.hits();
    assert_eq!(hits_after_first, 1, "首块应只请求上游一次");

    let resp = client()
        .get(&handle.url)
        .header("Range", "bytes=0-999")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    assert_eq!(
        mock.state.hits(),
        hits_after_first,
        "缓存命中不应再请求上游"
    );

    let stats = proxy.stats(&handle.token).unwrap();
    assert!(stats.downloaded_bytes >= 64 * 1024);
    assert!(stats.buffered_bytes >= 64 * 1024);
    cleanup(&proxy).await;
}

#[tokio::test]
async fn cross_chunk_range_is_stitched_correctly() {
    let tmp = TempDir::new("cross");
    let data = body_of(200_000);
    let mock = MockUpstream::start(data.clone(), true).await;
    let provider = MockProvider::new(mock.url.clone(), data.len() as u64);
    let proxy = StreamProxy::new(test_config(tmp.path(), 64 * 1024, 0));
    let handle = proxy.open_with_provider(provider, "/v.mp4").await.unwrap();

    let resp = client()
        .get(&handle.url)
        .header("Range", "bytes=60000-70000")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    let got = resp.bytes().await.unwrap();
    assert_eq!(got.as_ref(), &data[60000..=70000]);
    assert_eq!(mock.state.hits(), 2, "跨两块应触发两次上游请求");
    cleanup(&proxy).await;
}

#[tokio::test]
async fn upstream_headers_are_injected() {
    let tmp = TempDir::new("inject");
    let data = body_of(50_000);
    let mock = MockUpstream::start(data.clone(), true).await;
    let provider = MockProvider::new(mock.url.clone(), data.len() as u64);
    let proxy = StreamProxy::new(test_config(tmp.path(), 32 * 1024, 0));
    let handle = proxy.open_with_provider(provider, "/v.mp4").await.unwrap();

    let resp = client()
        .get(&handle.url)
        .header("Range", "bytes=0-99")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    let requests = mock.state.requests();
    assert!(!requests.is_empty());
    let auth = requests[0].headers.get("x-mock-auth");
    assert_eq!(
        auth.map(|s| s.as_str()),
        Some("secret"),
        "直链 headers 必须注入上游请求"
    );
    let referer = requests[0].headers.get("referer");
    assert_eq!(referer.map(|s| s.as_str()), Some("https://mock/"));
    cleanup(&proxy).await;
}

#[tokio::test]
async fn prefetch_fills_readahead_window() {
    let tmp = TempDir::new("prefetch");
    let data = body_of(400_000);
    let mock = MockUpstream::start(data.clone(), true).await;
    let provider = MockProvider::new(mock.url.clone(), data.len() as u64);
    let proxy = StreamProxy::new(test_config(tmp.path(), 64 * 1024, 128 * 1024));
    let handle = proxy.open_with_provider(provider, "/v.mp4").await.unwrap();
    let mut buffered = 0u64;
    for _ in 0..100 {
        if let Some(stats) = proxy.stats(&handle.token) {
            buffered = stats.buffered_bytes;
            if buffered >= 128 * 1024 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        buffered >= 128 * 1024,
        "后台预读应缓存预读窗口：buffered={buffered}"
    );
    cleanup(&proxy).await;
}

// ───────────────────────── 换链（T14）测试 ─────────────────────────

#[tokio::test]
async fn link_refresh_after_403_resumes_from_offset() {
    let tmp = TempDir::new("refresh");
    let data = body_of(300_000);
    let good = MockUpstream::start(data.clone(), true).await;
    let broken = MockUpstream::start(data.clone(), true).await;
    broken.state.set_forced_status(Some(403));
    // 第一次取链返回坏链（403），之后都是好链
    let provider = MockProvider::with_urls(
        vec![broken.url.clone(), good.url.clone()],
        data.len() as u64,
    );
    let proxy = StreamProxy::new(test_config(tmp.path(), 64 * 1024, 0));
    let handle = proxy
        .open_with_provider(provider.clone(), "/v.mp4")
        .await
        .unwrap();

    let resp = client()
        .get(&handle.url)
        .header("Range", "bytes=1000-1999")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206, "换链后应成功");
    let got = resp.bytes().await.unwrap();
    assert_eq!(got.as_ref(), &data[1000..=1999]);
    assert!(provider.calls() >= 2, "403 后应重新取直链");
    let stats = proxy.stats(&handle.token).unwrap();
    assert!(stats.upstream_restarts >= 1, "应记录换链次数：{stats:?}");
    assert_eq!(stats.errors, 0);
    cleanup(&proxy).await;
}

#[tokio::test]
async fn link_refresh_exhausted_returns_502() {
    let tmp = TempDir::new("exhaust");
    let data = body_of(100_000);
    let broken = MockUpstream::start(data.clone(), true).await;
    broken.state.set_forced_status(Some(403));
    let provider = MockProvider::new(broken.url.clone(), data.len() as u64);
    let mut cfg = test_config(tmp.path(), 64 * 1024, 0);
    cfg.max_link_restarts = 2;
    let proxy = StreamProxy::new(cfg);
    let handle = proxy
        .open_with_provider(provider.clone(), "/v.mp4")
        .await
        .unwrap();

    let resp = client()
        .get(&handle.url)
        .header("Range", "bytes=0-99")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502, "换链耗尽应明确报错");
    let stats = proxy.stats(&handle.token).unwrap();
    assert!(stats.errors >= 1);
    assert!(stats.upstream_restarts >= 2);
    cleanup(&proxy).await;
}

#[tokio::test]
async fn expire_at_in_past_triggers_proactive_refresh() {
    let tmp = TempDir::new("expire");
    let data = body_of(80_000);
    let mock = MockUpstream::start(data.clone(), true).await;
    let provider = MockProvider::new(mock.url.clone(), data.len() as u64);
    provider.set_expire_at(Some(wp_core::util::now_ts() - 10)); // 一打开就已过期
    let proxy = StreamProxy::new(test_config(tmp.path(), 32 * 1024, 0));
    let handle = proxy
        .open_with_provider(provider.clone(), "/v.mp4")
        .await
        .unwrap();

    let resp = client()
        .get(&handle.url)
        .header("Range", "bytes=0-99")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    assert!(provider.calls() >= 2, "expire_at 已过期应主动换链");
    let stats = proxy.stats(&handle.token).unwrap();
    assert!(stats.upstream_restarts >= 1);
    cleanup(&proxy).await;
}

#[tokio::test]
async fn upstream_ignoring_range_falls_back_to_full_download() {
    let tmp = TempDir::new("norange");
    let data = body_of(120_000);
    // supports_range=false → mock 永远回 200 全量，忽略 Range
    let mock = MockUpstream::start(data.clone(), false).await;
    let provider = MockProvider::new(mock.url.clone(), data.len() as u64);
    let proxy = StreamProxy::new(test_config(tmp.path(), 40 * 1024, 0));
    let handle = proxy.open_with_provider(provider, "/v.mp4").await.unwrap();

    let resp = client()
        .get(&handle.url)
        .header("Range", "bytes=5000-5999")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    assert_eq!(
        resp.headers()["content-range"],
        format!("bytes 5000-5999/{}", data.len()).as_str()
    );
    let got = resp.bytes().await.unwrap();
    assert_eq!(got.as_ref(), &data[5000..=5999]);
    cleanup(&proxy).await;
}

// ───────────────────────── keep-alive ─────────────────────────

#[tokio::test]
async fn keep_alive_serves_multiple_range_requests_on_one_connection() {
    let tmp = TempDir::new("keepalive");
    let data = body_of(50_000);
    let mock = MockUpstream::start(data.clone(), true).await;
    let provider = MockProvider::new(mock.url.clone(), data.len() as u64);
    let proxy = StreamProxy::new(test_config(tmp.path(), 32 * 1024, 0));
    let handle = proxy.open_with_provider(provider, "/v.mp4").await.unwrap();
    let port = proxy.port().unwrap();
    let path = format!("/s/{}", handle.token);

    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    for (start, end) in [(0usize, 9usize), (100, 199)] {
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes={start}-{end}\r\nConnection: keep-alive\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).await.unwrap();
        let (status, headers, body) = read_http_response(&mut stream).await;
        assert_eq!(status, 206);
        assert_eq!(
            headers.get("content-range").map(|s| s.as_str()),
            Some(format!("bytes {start}-{end}/{}", data.len()).as_str())
        );
        assert_eq!(
            body.as_slice(),
            &data[start..=end],
            "keep-alive 第二个请求应正确"
        );
    }
    cleanup(&proxy).await;
}

// ───────────────────────── probe_range ─────────────────────────

#[tokio::test]
async fn probe_reports_range_capabilities() {
    let data = body_of(1_000_000);
    let mock = MockUpstream::start(data.clone(), true).await;
    let provider = MockProvider::new(mock.url.clone(), data.len() as u64);
    let report = probe_with_provider(provider, "/v.mp4").await.unwrap();
    assert_eq!(report.drive, "mock:test");
    assert_eq!(report.size, Some(data.len() as u64));
    assert!(report.range_supported, "支持 Range 应报告 true：{report:?}");
    assert!(!report.upstream_ignores_range);
    let head = report.head.as_ref().unwrap();
    assert_eq!(head.status, Some(200));
    assert_eq!(head.accept_ranges.as_deref(), Some("bytes"));
    let first = report.first.as_ref().unwrap();
    assert_eq!(first.status, Some(206));
    let mid = report.mid.as_ref().unwrap();
    assert_eq!(mid.status, Some(206));
    assert!(mid.ok);
    assert!(report.error.is_none(), "不应有整体错误：{report:?}");
}

#[tokio::test]
async fn probe_detects_upstream_ignoring_range() {
    let data = body_of(200_000);
    let mock = MockUpstream::start(data.clone(), false).await;
    let provider = MockProvider::new(mock.url.clone(), data.len() as u64);
    let report = probe_with_provider(provider, "/v.mp4").await.unwrap();
    assert!(
        !report.range_supported,
        "忽略 Range 应报告不支持：{report:?}"
    );
    assert!(report.upstream_ignores_range);
    let first = report.first.as_ref().unwrap();
    assert_eq!(first.status, Some(200));
    assert!(!first.ok);
    assert!(report.error.is_some());
}

#[tokio::test]
async fn probe_direct_link_failure_is_reported() {
    struct FailingProvider;
    #[async_trait::async_trait]
    impl LinkProvider for FailingProvider {
        fn cache_key(&self) -> String {
            "failing:test".into()
        }
        async fn direct_link(&self, _p: &str) -> wp_core::Result<DirectLink> {
            Err(wp_core::Error::server(23018, "download file size limit"))
        }
    }
    let report = probe_with_provider(Arc::new(FailingProvider), "/big.mp4")
        .await
        .unwrap();
    assert!(!report.range_supported);
    let err = report.error.unwrap();
    assert!(
        err.contains("23018") || err.contains("size limit"),
        "错误应带原始信息：{err}"
    );
}

// ───────────────────────── HLS 直连（is_hls 契约） ─────────────────────────

struct HlsProvider;

#[async_trait::async_trait]
impl LinkProvider for HlsProvider {
    fn cache_key(&self) -> String {
        "mock:hls".into()
    }
    async fn direct_link(&self, _p: &str) -> wp_core::Result<DirectLink> {
        Ok(DirectLink {
            url: "https://cdn.example.com/v.m3u8?token=abc".into(),
            headers: vec![("Cookie".into(), "c=1".into())],
            expire_at: None,
            size: 0,
            file_name: "大文件.mp4".into(),
            is_hls: true,
        })
    }
}

#[tokio::test]
async fn hls_link_returns_direct_handle_without_proxy_session() {
    let tmp = TempDir::new("hls");
    let proxy = StreamProxy::new(test_config(tmp.path(), 64 * 1024, 0));
    let handle = proxy
        .open_with_provider(Arc::new(HlsProvider), "/大文件.mp4")
        .await
        .unwrap();
    assert!(handle.direct, "HLS 应标记为直连");
    assert_eq!(handle.url, "https://cdn.example.com/v.m3u8?token=abc");
    assert_eq!(handle.display, "大文件.mp4");
    assert_eq!(
        handle.headers,
        vec![("Cookie".to_string(), "c=1".to_string())]
    );
    assert!(handle.token.is_empty(), "直连句柄没有本地 token");
    assert!(
        proxy.stats(&handle.token).is_none(),
        "直连句柄没有代理会话，stats 应为 None"
    );
    assert_eq!(proxy.session_count(), 0, "HLS 不应注册代理会话");
    cleanup(&proxy).await;
}

#[tokio::test]
async fn probe_hls_returns_clear_error() {
    let err = probe_with_provider(Arc::new(HlsProvider), "/大文件.mp4")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("HLS"),
        "HLS 探测应返回明确错误：{err}"
    );
}

// ───────────────────────── 原始 HTTP 响应读取（keep-alive 测试用） ─────────────────────────

async fn read_http_response(stream: &mut TcpStream) -> (u16, HashMap<String, String>, Vec<u8>) {
    let mut buf = Vec::new();
    let header_end;
    loop {
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "连接被提前关闭");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        assert!(buf.len() < 64 * 1024, "响应头过大");
    }
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < len {
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0);
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    (status, headers, body)
}
