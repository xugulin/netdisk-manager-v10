//! 播放链路决策：网盘路径 → 播放源（本地 Range 代理 / HLS 直连）。
//!
//! 把「取直链 → 判断是否 HLS → 走代理 or 直连」这段逻辑收敛到库层，
//! GUI / wp-media 只需调用 [`resolve_playback`]，避免各处散落 `if is_hls`。
//!
//! * 普通文件：`DirectLink.is_hls == false` → [`StreamProxy`] 本地代理（Range + 磁盘缓存 + 换链）；
//! * HLS（m3u8，如夸克大文件云端转码）：`is_hls == true` → **不启动代理**，
//!   直接把 m3u8 URL + 需要注入的 headers 交播放器直连（mpv `--http-header-fields`）；
//! * 取链错误（需要登录 / 无权限 / 夸克 23018 等）原样透传，文案保持适配器给出的可读信息。
//!
//! ```no_run
//! # async fn demo(proxy: &wp_stream::StreamProxy, drive: std::sync::Arc<dyn wp_drive::Drive>) -> wp_core::Result<()> {
//! use wp_stream::{resolve_playback, PlaybackSource};
//! match resolve_playback(proxy, drive, "/电影/示例.mp4").await? {
//!     PlaybackSource::Proxied { handle } => println!("本地代理：{}", handle.url),
//!     PlaybackSource::DirectHls { url, .. } => println!("HLS 直连：{url}"),
//! }
//! # Ok(()) }
//! ```

use std::sync::Arc;

use wp_core::model::DirectLink;
use wp_core::{Error, Result};
use wp_drive::Drive;

use crate::upstream::{DriveLinkProvider, LinkProvider};
use crate::{StreamHandle, StreamProxy};

/// 一个可播放的来源。
#[derive(Debug, Clone)]
pub enum PlaybackSource {
    /// 普通文件：已由 [`StreamProxy`] 包装成本地 HTTP 流（支持 Range/缓存/换链）。
    Proxied { handle: StreamHandle },
    /// HLS（m3u8）：播放器直连，需注入 `headers`（Cookie/Referer 等）。
    DirectHls {
        url: String,
        headers: Vec<(String, String)>,
        display: String,
        size: u64,
    },
}

impl PlaybackSource {
    /// 播放器应打开的 URL（本地代理地址或原始 m3u8）。
    pub fn url(&self) -> &str {
        match self {
            PlaybackSource::Proxied { handle } => &handle.url,
            PlaybackSource::DirectHls { url, .. } => url,
        }
    }
    /// 播放器应注入的 HTTP 头（代理流为空）。
    pub fn headers(&self) -> &[(String, String)] {
        match self {
            PlaybackSource::Proxied { handle } => &handle.headers,
            PlaybackSource::DirectHls { headers, .. } => headers,
        }
    }
    pub fn display(&self) -> &str {
        match self {
            PlaybackSource::Proxied { handle } => &handle.display,
            PlaybackSource::DirectHls { display, .. } => display,
        }
    }
    /// 文件大小（HLS 可能为 0/未知）。
    pub fn size(&self) -> u64 {
        match self {
            PlaybackSource::Proxied { handle } => handle.total_bytes,
            PlaybackSource::DirectHls { size, .. } => *size,
        }
    }
    /// 是否 HLS 直连（不做 Range 代理）。
    pub fn is_hls(&self) -> bool {
        matches!(self, PlaybackSource::DirectHls { .. })
    }
    /// 本地代理会话 token（HLS 直连为 None；停止播放时用它 [`StreamProxy::close`]）。
    pub fn token(&self) -> Option<&str> {
        match self {
            PlaybackSource::Proxied { handle } => Some(handle.token.as_str()),
            PlaybackSource::DirectHls { .. } => None,
        }
    }
    /// 日志/UI 用的一行描述。
    pub fn summary(&self) -> String {
        match self {
            PlaybackSource::Proxied { handle } => format!(
                "代理流 {}（{} 字节，token={}）",
                handle.display, handle.total_bytes, handle.token
            ),
            PlaybackSource::DirectHls { display, size, .. } => {
                format!("HLS 直连 {display}（{} 字节，m3u8）", size)
            }
        }
    }
}

/// 解析一条网盘路径的播放源。
///
/// 只取一次 `Drive::direct_link`：`is_hls` 为真直接返回 [`PlaybackSource::DirectHls`]，
/// 否则把同一条直链交给 [`StreamProxy::open_with_link`] 建代理会话（不再二次取链）。
pub async fn resolve_playback(
    proxy: &StreamProxy,
    drive: Arc<dyn Drive>,
    remote_path: &str,
) -> Result<PlaybackSource> {
    let provider: Arc<dyn LinkProvider> = Arc::new(DriveLinkProvider::new(drive));
    resolve_playback_with_provider(proxy, provider, remote_path).await
}

/// 同 [`resolve_playback`]，但直链来源是可注入的 [`LinkProvider`]（单测/自定义取链）。
pub async fn resolve_playback_with_provider(
    proxy: &StreamProxy,
    provider: Arc<dyn LinkProvider>,
    remote_path: &str,
) -> Result<PlaybackSource> {
    let link = provider.direct_link(remote_path).await?;
    resolve_from_link(proxy, provider, remote_path, link).await
}

/// 已经拿到直链时直接决策（GUI 预览里先取过链的场景），避免再取一次。
pub async fn resolve_from_link(
    proxy: &StreamProxy,
    provider: Arc<dyn LinkProvider>,
    remote_path: &str,
    link: DirectLink,
) -> Result<PlaybackSource> {
    if link.is_hls {
        if link.url.trim().is_empty() {
            return Err(Error::other(format!(
                "HLS 直链为空（{remote_path}）：适配器返回 is_hls=true 但没有 m3u8 URL"
            )));
        }
        let display = display_of(&link.file_name, remote_path);
        wp_core::log_info!(
            "wp-stream",
            "播放源：HLS 直连 {display}（{} 字节）",
            link.size
        );
        return Ok(PlaybackSource::DirectHls {
            url: link.url,
            headers: link.headers,
            display,
            size: link.size,
        });
    }
    let handle = proxy
        .open_link_with_provider(provider, remote_path, link)
        .await?;
    wp_core::log_info!(
        "wp-stream",
        "播放源：本地代理 {}（{} 字节，token={}）",
        handle.display,
        handle.total_bytes,
        handle.token
    );
    Ok(PlaybackSource::Proxied { handle })
}

fn display_of(file_name: &str, remote_path: &str) -> String {
    if file_name.trim().is_empty() {
        wp_core::util::base_name(remote_path)
    } else {
        file_name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use async_trait::async_trait;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use wp_core::model::{
        AccountInfo, DirectLink, DownloadOptions, DriveKind, OpReport, ProgressFn, RemoteEntry,
        UploadOptions,
    };
    use wp_drive::{Drive, DriveCaps};

    use crate::range;
    use crate::StreamConfig;

    // ───────────── mock Drive（只实现驱动播放决策所需的几个方法） ─────────────

    enum LinkBehavior {
        /// 正常返回一条直链。
        Fixed(DirectLink),
        /// 返回错误（模拟适配器透传的业务错误）。
        Fail(wp_core::Error),
    }

    struct MockDrive {
        behavior: Mutex<LinkBehavior>,
        calls: AtomicUsize,
    }

    impl MockDrive {
        fn new(link: DirectLink) -> Arc<Self> {
            Arc::new(Self {
                behavior: Mutex::new(LinkBehavior::Fixed(link)),
                calls: AtomicUsize::new(0),
            })
        }
        fn failing(err: wp_core::Error) -> Arc<Self> {
            Arc::new(Self {
                behavior: Mutex::new(LinkBehavior::Fail(err)),
                calls: AtomicUsize::new(0),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Drive for MockDrive {
        fn kind(&self) -> DriveKind {
            DriveKind::Quark
        }
        fn id(&self) -> &str {
            "mock"
        }
        fn name(&self) -> &str {
            "Mock 网盘"
        }
        fn caps(&self) -> DriveCaps {
            DriveCaps {
                direct_link: true,
                ..DriveCaps::default()
            }
        }
        async fn account(&self) -> Result<AccountInfo> {
            Err(Error::unsupported("mock"))
        }
        async fn logout(&self) -> Result<()> {
            Err(Error::unsupported("mock"))
        }
        async fn list(&self, _path: &str) -> Result<Vec<RemoteEntry>> {
            Err(Error::unsupported("mock"))
        }
        async fn stat(&self, _path: &str) -> Result<RemoteEntry> {
            Err(Error::unsupported("mock"))
        }
        async fn delete(&self, _paths: &[String]) -> Result<OpReport> {
            Err(Error::unsupported("mock"))
        }
        async fn rename(&self, _path: &str, _new_name: &str) -> Result<RemoteEntry> {
            Err(Error::unsupported("mock"))
        }
        async fn move_entries(&self, _paths: &[String], _dest: &str) -> Result<OpReport> {
            Err(Error::unsupported("mock"))
        }
        async fn copy_entries(&self, _paths: &[String], _dest: &str) -> Result<OpReport> {
            Err(Error::unsupported("mock"))
        }
        async fn search(&self, _keyword: &str, _dir: &str) -> Result<Vec<RemoteEntry>> {
            Err(Error::unsupported("mock"))
        }
        async fn direct_link(&self, _path: &str) -> Result<DirectLink> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &*self.behavior.lock().unwrap() {
                LinkBehavior::Fixed(link) => Ok(link.clone()),
                LinkBehavior::Fail(e) => Err(match e {
                    Error::Server { code, msg } => Error::server(*code, msg.clone()),
                    Error::Auth(m) => Error::auth(m.clone()),
                    Error::NotFound(m) => Error::not_found(m.clone()),
                    other => Error::other(other.to_string()),
                }),
            }
        }
        async fn download(
            &self,
            _remote: &str,
            _local: &Path,
            _opts: &DownloadOptions,
            _progress: ProgressFn,
        ) -> Result<u64> {
            Err(Error::unsupported("mock"))
        }
        async fn upload(
            &self,
            _local: &Path,
            _remote_dir: &str,
            _opts: &UploadOptions,
            _progress: ProgressFn,
        ) -> Result<RemoteEntry> {
            Err(Error::unsupported("mock"))
        }
    }

    /// 极简 Range 上游：任意 `bytes=a-b` 返回 206 + body 切片（Connection: close）。
    struct MockUpstream {
        url: String,
        hits: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for MockUpstream {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn start_upstream(body: Vec<u8>) -> MockUpstream {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let body = Arc::new(body);
        let h = hits.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let body = body.clone();
                let h = h.clone();
                tokio::spawn(async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    serve_upstream(stream, body).await;
                });
            }
        });
        MockUpstream {
            url: format!("http://127.0.0.1:{port}/file.mp4"),
            hits,
            task,
        }
    }

    async fn serve_upstream(mut stream: TcpStream, body: Arc<Vec<u8>>) {
        let mut buf = Vec::new();
        loop {
            let mut chunk = [0u8; 1024];
            let Ok(n) = stream.read(&mut chunk).await else {
                return;
            };
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
            if find_subslice(&buf, b"\r\n\r\n").is_some() || buf.len() > 64 * 1024 {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf).to_string();
        let mut headers = HashMap::new();
        for line in head.split("\r\n").skip(1) {
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        let mut status = 200u16;
        let mut reason = "OK";
        let mut extra = String::new();
        let mut payload = body.as_ref().clone();
        if let Some(r) = headers.get("range") {
            if let Ok(spec) = range::parse_range_header(r) {
                if let Ok((s, e)) = range::resolve_range(spec, body.len() as u64) {
                    status = 206;
                    reason = "Partial Content";
                    extra.push_str(&format!(
                        "Content-Range: {}\r\nAccept-Ranges: bytes\r\n",
                        range::content_range(s, e, body.len() as u64)
                    ));
                    payload = body[s as usize..=e as usize].to_vec();
                }
            }
        }
        if status == 200 {
            extra.push_str("Accept-Ranges: bytes\r\n");
        }
        let resp = format!(
            "HTTP/1.1 {status} {reason}\r\n{extra}Content-Type: video/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.write_all(&payload).await;
        let _ = stream.shutdown().await;
    }

    fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    fn test_config(dir: &Path) -> StreamConfig {
        StreamConfig {
            cache_dir: dir.to_path_buf(),
            readahead_bytes: 0,
            max_cache_bytes: 8 * 1024 * 1024,
            chunk_bytes: 8 * 1024,
            parallel_fetches: 1,
            max_link_restarts: 2,
            request_timeout_secs: 5,
            connect_timeout_secs: 3,
            adaptive_chunk: false,
            startup_first_bytes: 0,
            seek_priority: false,
            seek_pause_ms: 400,
            adaptive_weak_mbps: 20.0,
            weak_readahead_bytes: 32 * 1024 * 1024,
            failure_breaker_threshold: 2,
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "wp-stream-playback-{tag}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn hls_link() -> DirectLink {
        DirectLink {
            url: "https://hls.example.com/video/index.m3u8?sign=abc".into(),
            headers: vec![
                ("Cookie".into(), "quark_cookie=xyz".into()),
                ("Referer".into(), "https://pan.quark.cn/".into()),
            ],
            expire_at: Some(wp_core::util::now_ts() + 3600),
            size: 0,
            file_name: "大视频.mp4".into(),
            is_hls: true,
        }
    }

    fn http_link(url: String, size: u64) -> DirectLink {
        DirectLink {
            url,
            headers: vec![("X-Mock".into(), "1".into())],
            expire_at: None,
            size,
            file_name: "正片.mp4".into(),
            is_hls: false,
        }
    }

    // ───────────── 测试 ─────────────

    #[tokio::test]
    async fn hls_direct_link_returns_direct_hls_without_proxy_session() {
        let dir = temp_dir("hls");
        let proxy = StreamProxy::new(test_config(&dir));
        let drive = MockDrive::new(hls_link());
        let source = resolve_playback(&proxy, drive.clone(), "/夸克/大视频.mp4")
            .await
            .unwrap();
        match &source {
            PlaybackSource::DirectHls {
                url,
                headers,
                display,
                size,
            } => {
                assert_eq!(url, "https://hls.example.com/video/index.m3u8?sign=abc");
                assert_eq!(headers.len(), 2);
                assert_eq!(headers[0].0, "Cookie");
                assert_eq!(display, "大视频.mp4");
                assert_eq!(*size, 0);
            }
            other => panic!("HLS 直链应返回 DirectHls，实际：{other:?}"),
        }
        assert!(source.is_hls());
        assert!(source.token().is_none());
        assert_eq!(drive.calls(), 1, "只应取一次直链");
        assert_eq!(proxy.session_count(), 0, "HLS 直连不应注册代理会话");
        assert!(source.summary().contains("HLS"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn http_direct_link_returns_proxied_stream_and_plays() {
        let dir = temp_dir("http");
        let body: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        let upstream = start_upstream(body.clone()).await;
        let proxy = StreamProxy::new(test_config(&dir));
        let drive = MockDrive::new(http_link(upstream.url.clone(), body.len() as u64));
        let source = resolve_playback(&proxy, drive.clone(), "/电影/正片.mp4")
            .await
            .unwrap();
        let PlaybackSource::Proxied { handle } = &source else {
            panic!("普通直链应返回 Proxied，实际：{source:?}");
        };
        assert!(!source.is_hls());
        assert!(!handle.direct);
        assert_eq!(handle.total_bytes, body.len() as u64);
        assert_eq!(handle.display, "正片.mp4");
        let token = source.token().expect("代理流应有 token");
        assert!(proxy.stats(token).is_some(), "代理会话应有统计");

        // 通过本地代理读取一段，验证真的能播
        let client = reqwest::Client::new();
        let resp = client
            .get(&handle.url)
            .header("Range", "bytes=1000-1999")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 206);
        let got = resp.bytes().await.unwrap();
        assert_eq!(got.as_ref(), &body[1000..=1999]);
        assert_eq!(
            drive.calls(),
            1,
            "只应取一次直链（open_with_link 不重复取链）"
        );
        assert!(
            upstream.hits.load(Ordering::SeqCst) >= 1,
            "代理应向上游发起 Range 取块请求"
        );

        proxy.close(token);
        proxy.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn direct_link_error_is_transparent() {
        let dir = temp_dir("err");
        let proxy = StreamProxy::new(test_config(&dir));
        let drive = MockDrive::failing(Error::server(23018, "download file size limit"));
        let err = resolve_playback(&proxy, drive, "/夸克/大视频.mp4")
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("23018"), "错误码应透传：{text}");
        assert!(text.contains("size limit"), "错误文案应可读：{text}");
        assert_eq!(proxy.session_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hls_with_empty_url_returns_readable_error() {
        let dir = temp_dir("hls-empty");
        let proxy = StreamProxy::new(test_config(&dir));
        let mut link = hls_link();
        link.url = String::new();
        let drive = MockDrive::new(link);
        let err = resolve_playback(&proxy, drive, "/夸克/坏链接.mp4")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HLS"), "错误应说明 HLS：{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn display_falls_back_to_remote_base_name() {
        let dir = temp_dir("display");
        let proxy = StreamProxy::new(test_config(&dir));
        let mut link = hls_link();
        link.file_name = String::new();
        let drive = MockDrive::new(link);
        let source = resolve_playback(&proxy, drive, "/目录/我的电影.mkv")
            .await
            .unwrap();
        assert_eq!(source.display(), "我的电影.mkv");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resolve_with_provider_handles_both_branches() {
        struct FixedProvider(DirectLink);
        #[async_trait]
        impl LinkProvider for FixedProvider {
            fn cache_key(&self) -> String {
                "mock:test".into()
            }
            async fn direct_link(&self, _p: &str) -> Result<DirectLink> {
                Ok(self.0.clone())
            }
        }

        let dir = temp_dir("provider");
        let proxy = StreamProxy::new(test_config(&dir));

        // HLS 分支（provider 入口）
        let hls = resolve_playback_with_provider(
            &proxy,
            Arc::new(FixedProvider(hls_link())),
            "/夸克/大视频.mp4",
        )
        .await
        .unwrap();
        assert!(hls.is_hls());
        assert_eq!(proxy.session_count(), 0);

        // 普通分支（provider 入口）
        let body: Vec<u8> = (0..20_000u32).map(|i| (i % 199) as u8).collect();
        let upstream = start_upstream(body.clone()).await;
        let http = resolve_playback_with_provider(
            &proxy,
            Arc::new(FixedProvider(http_link(
                upstream.url.clone(),
                body.len() as u64,
            ))),
            "/电影/正片.mp4",
        )
        .await
        .unwrap();
        match http {
            PlaybackSource::Proxied { handle } => {
                assert!(proxy.stats(&handle.token).is_some());
                proxy.close(&handle.token);
            }
            other => panic!("应为代理流：{other:?}"),
        }
        proxy.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn playback_source_helpers() {
        let handle = StreamHandle {
            url: "http://127.0.0.1:1/s/tok".into(),
            token: "tok".into(),
            display: "a.mp4".into(),
            total_bytes: 10,
            direct: false,
            headers: Vec::new(),
        };
        let p = PlaybackSource::Proxied { handle };
        assert_eq!(p.url(), "http://127.0.0.1:1/s/tok");
        assert_eq!(p.token(), Some("tok"));
        assert_eq!(p.size(), 10);
        assert!(!p.is_hls());

        let h = PlaybackSource::DirectHls {
            url: "https://x/v.m3u8".into(),
            headers: vec![("Cookie".into(), "c".into())],
            display: "v".into(),
            size: 5,
        };
        assert!(h.is_hls());
        assert_eq!(h.headers().len(), 1);
        assert!(h.token().is_none());
        assert!(h.summary().contains("HLS"));
    }
}
