//! 下载器：单连接断点续传 + 大文件多段并发，进度回调可取消。
//!
//! 约定：
//! * 半成品写到 `<目标>.part`（多段时为 `<目标>.part.N`），完成后原子改名为目标；
//! * 进度回调 `(已完成字节, 总字节)`，返回 `false` = 用户取消 → 返回
//!   [`wp_core::Error::Canceled`]，**保留** `<目标>.part` 供下次续传；
//! * 服务器不支持 Range 时自动退回整文件下载；
//! * 下载请求默认 `Accept-Encoding: identity`，避免 gzip 解压导致字节数错位。

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use wp_core::model::ProgressFn;
use wp_core::{Error, Result};

/// 一次下载请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub url: String,
    /// 额外请求头（UA / Referer / Cookie 等，由适配器决定）
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// 本地目标路径
    pub dest: PathBuf,
    /// 预期大小（0 = 未知，从 Content-Length 推断）
    #[serde(default)]
    pub expected_size: u64,
    /// 多段数（1 = 单连接）
    #[serde(default = "default_segments")]
    pub segments: usize,
    /// 超过该大小才多段
    #[serde(default = "default_threshold")]
    pub segment_threshold: u64,
    /// 是否允许续传（读已有 .part）
    #[serde(default = "default_true")]
    pub resume: bool,
    /// 限速（字节/秒，0 = 不限）
    #[serde(default)]
    pub limit_bps: u64,
}

fn default_segments() -> usize {
    4
}
fn default_threshold() -> u64 {
    32 * 1024 * 1024
}
fn default_true() -> bool {
    true
}

impl DownloadRequest {
    pub fn new(url: impl Into<String>, dest: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            headers: Vec::new(),
            dest: dest.into(),
            expected_size: 0,
            segments: 1,
            segment_threshold: default_threshold(),
            resume: true,
            limit_bps: 0,
        }
    }

    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }

    pub fn headers(mut self, hs: &[(String, String)]) -> Self {
        self.headers.extend(hs.iter().cloned());
        self
    }

    pub fn expected_size(mut self, size: u64) -> Self {
        self.expected_size = size;
        self
    }

    pub fn segments(mut self, n: usize) -> Self {
        self.segments = n.max(1);
        self
    }

    pub fn resume(mut self, yes: bool) -> Self {
        self.resume = yes;
        self
    }

    pub fn limit_bps(mut self, bps: u64) -> Self {
        self.limit_bps = bps;
        self
    }
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

fn seg_part_path(dest: &Path, i: usize) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(format!(".part.{i}"));
    PathBuf::from(s)
}

fn fsize(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn ensure_parent(p: &Path) {
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
}

/// 下载入口。
pub async fn download(
    client: &reqwest::Client,
    req: DownloadRequest,
    mut progress: ProgressFn,
) -> Result<u64> {
    ensure_parent(&req.dest);
    let part = part_path(&req.dest);

    // 已完成？
    if let (true, n) = (req.resume, fsize(&part)) {
        if req.expected_size > 0 && n == req.expected_size {
            tokio::fs::rename(&part, &req.dest).await.map_err(io_err)?;
            let _ = progress(n, n);
            return Ok(n);
        }
    }

    // 是否走多段：要求已知大小、够大、当前没有可续传的半成品
    let have_partial = req.resume && fsize(&part) > 0;
    if req.segments > 1
        && req.expected_size > req.segment_threshold
        && !have_partial
        && req.expected_size > 0
    {
        match download_segmented(client, &req, &mut progress).await {
            Ok(n) => return Ok(n),
            Err(e) if e.is_canceled() => return Err(e),
            Err(e) => {
                wp_core::log_debug!("下载", "多段下载失败（{e}），退回单连接");
                // 清理分片后重试单连接
                for i in 0..req.segments {
                    let _ = std::fs::remove_file(seg_part_path(&req.dest, i));
                }
            }
        }
    }

    download_single(client, &req, &mut progress).await
}

fn io_err(e: std::io::Error) -> Error {
    Error::Io(e)
}

async fn download_single(
    client: &reqwest::Client,
    req: &DownloadRequest,
    progress: &mut ProgressFn,
) -> Result<u64> {
    let part = part_path(&req.dest);
    let mut existing = if req.resume { fsize(&part) } else { 0 };
    if existing > 0 && req.expected_size > 0 && existing > req.expected_size {
        let _ = std::fs::remove_file(&part);
        existing = 0;
    }
    if req.expected_size > 0 && existing == req.expected_size {
        tokio::fs::rename(&part, &req.dest).await.map_err(io_err)?;
        let _ = progress(existing, req.expected_size);
        return Ok(existing);
    }

    let mut rb = client
        .get(&req.url)
        .header(reqwest::header::ACCEPT_ENCODING, "identity");
    for (k, v) in &req.headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    if existing > 0 {
        rb = rb.header(reqwest::header::RANGE, format!("bytes={existing}-"));
    }
    let resp = rb.send().await.map_err(crate::client::net_err)?;
    let status = resp.status().as_u16();
    if status == 416 {
        // 已经下载完（或服务端认为范围越界）→ 校验大小
        if req.expected_size > 0 && existing == req.expected_size {
            tokio::fs::rename(&part, &req.dest).await.map_err(io_err)?;
            return Ok(existing);
        }
        let _ = std::fs::remove_file(&part);
        return Err(Error::network("断点续传范围越界，已清理半成品，请重试"));
    }
    if !(200..300).contains(&status) {
        let body = resp.text().await.unwrap_or_default();
        return Err(crate::client::http_status_error(status, &body));
    }
    let is_partial = status == 206;
    if !is_partial {
        existing = 0; // 服务器忽略了 Range，从头写
    }
    let content_len = resp.content_length().unwrap_or(0);
    let total = if req.expected_size > 0 {
        req.expected_size
    } else if is_partial {
        existing + content_len
    } else {
        content_len
    };

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(existing > 0)
        .truncate(existing == 0)
        .open(&part)
        .await
        .map_err(io_err)?;
    let mut done = existing;
    let started = Instant::now();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(crate::client::net_err)?;
        file.write_all(&chunk).await.map_err(io_err)?;
        done += chunk.len() as u64;
        let keep = progress(done, total);
        if !keep {
            let _ = file.flush().await;
            return Err(Error::Canceled("用户取消/暂停".into()));
        }
        throttle(done, started, req.limit_bps).await;
    }
    file.flush().await.map_err(io_err)?;
    drop(file);
    tokio::fs::rename(&part, &req.dest).await.map_err(io_err)?;
    let _ = progress(done, total.max(done));
    Ok(done)
}

async fn throttle(done: u64, started: Instant, limit_bps: u64) {
    if limit_bps == 0 {
        return;
    }
    let target = Duration::from_secs_f64(done as f64 / limit_bps as f64);
    let elapsed = started.elapsed();
    if target > elapsed {
        tokio::time::sleep(target - elapsed).await;
    }
}

/// 多段并发下载：每段独立 Range 写入分片文件，最后顺序合并。
async fn download_segmented(
    client: &reqwest::Client,
    req: &DownloadRequest,
    progress: &mut ProgressFn,
) -> Result<u64> {
    let total = req.expected_size;
    let n = req
        .segments
        .clamp(2, 16)
        .min((total / (256 * 1024)).max(2) as usize);
    let seg_size = total / n as u64;
    let counter = Arc::new(AtomicU64::new(0));
    let cancel = Arc::new(AtomicBool::new(false));
    let mut set = tokio::task::JoinSet::new();

    for i in 0..n {
        let start = i as u64 * seg_size;
        let end = if i + 1 == n {
            total
        } else {
            (i as u64 + 1) * seg_size
        };
        let client = client.clone();
        let url = req.url.clone();
        let headers = req.headers.clone();
        let dest = seg_part_path(&req.dest, i);
        let counter = counter.clone();
        let cancel = cancel.clone();
        set.spawn(async move {
            let mut rb = client
                .get(&url)
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .header(reqwest::header::RANGE, format!("bytes={start}-{}", end - 1));
            for (k, v) in &headers {
                rb = rb.header(k.as_str(), v.as_str());
            }
            let resp = rb.send().await.map_err(crate::client::net_err)?;
            let status = resp.status().as_u16();
            if !(200..300).contains(&status) {
                return Err(Error::network(format!("分片 {i} 下载失败：HTTP {status}")));
            }
            if status == 200 {
                return Err(Error::network("服务器不支持分片 Range"));
            }
            let mut file = tokio::fs::File::create(&dest).await.map_err(Error::Io)?;
            let mut got = 0u64;
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Canceled("用户取消/暂停".into()));
                }
                let chunk = chunk.map_err(crate::client::net_err)?;
                file.write_all(&chunk).await.map_err(Error::Io)?;
                got += chunk.len() as u64;
                counter.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            file.flush().await.map_err(Error::Io)?;
            let expect = end - start;
            if got != expect {
                return Err(Error::network(format!(
                    "分片 {i} 字节数不符：期望 {expect}，实际 {got}"
                )));
            }
            Ok(())
        });
    }

    let mut last_report = 0u64;
    loop {
        tokio::select! {
            joined = set.join_next() => {
                match joined {
                    None => break,
                    Some(Err(e)) => {
                        cancel.store(true, Ordering::Relaxed);
                        set.abort_all();
                        return Err(Error::other(format!("分片任务崩溃：{e}")));
                    }
                    Some(Ok(Err(e))) => {
                        cancel.store(true, Ordering::Relaxed);
                        set.abort_all();
                        return Err(e);
                    }
                    Some(Ok(Ok(()))) => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                let done = counter.load(Ordering::Relaxed).min(total);
                if done != last_report {
                    last_report = done;
                    if !progress(done, total) {
                        cancel.store(true, Ordering::Relaxed);
                        set.abort_all();
                        return Err(Error::Canceled("用户取消/暂停".into()));
                    }
                }
            }
        }
    }

    // 合并分片
    let dest = req.dest.clone();
    let part = part_path(&dest);
    tokio::task::spawn_blocking(move || -> Result<u64> {
        use std::io::Write;
        let mut out = std::fs::File::create(&part).map_err(Error::Io)?;
        let mut merged = 0u64;
        for i in 0..n {
            let seg = seg_part_path(&dest, i);
            let mut f = std::fs::File::open(&seg).map_err(Error::Io)?;
            merged += std::io::copy(&mut f, &mut out).map_err(Error::Io)?;
            let _ = std::fs::remove_file(&seg);
        }
        out.flush().map_err(Error::Io)?;
        drop(out);
        if merged != total {
            let _ = std::fs::remove_file(&part);
            return Err(Error::network(format!(
                "合并后大小不符：期望 {total}，实际 {merged}"
            )));
        }
        std::fs::rename(&part, &dest).map_err(Error::Io)?;
        Ok(merged)
    })
    .await
    .map_err(|e| Error::other(format!("合并分片任务失败：{e}")))??;

    let _ = progress(total, total);
    Ok(total)
}

#[cfg(test)]
mod tests {
    //! 用一个极简的本地 HTTP 服务验证：单连接 / 断点续传 / 多段合并 / 取消。
    use super::*;
    use std::io::Read as _;
    use std::io::Write;
    use std::net::TcpListener;

    fn body() -> Vec<u8> {
        (0..100_000u32).map(|i| (i % 251) as u8).collect()
    }

    /// 启动一次性测试服务：支持 Range，返回完整/部分内容。
    fn spawn_server(data: Vec<u8>, support_range: bool) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let mut start = 0usize;
                let mut end = data.len();
                let mut status = "200 OK";
                if support_range {
                    if let Some(line) = req
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                    {
                        if let Some(v) = line.split('=').nth(1) {
                            let mut it = v.trim().split('-');
                            start = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
                            end = it
                                .next()
                                .filter(|s| !s.trim().is_empty())
                                .and_then(|s| s.trim().parse().ok())
                                .unwrap_or(data.len() - 1)
                                + 1;
                            status = "206 Partial Content";
                        }
                    }
                } else {
                    start = 0;
                    end = data.len();
                }
                let piece = &data[start.min(data.len())..end.min(data.len())];
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                    piece.len(),
                    start,
                    end.saturating_sub(1),
                    data.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(piece);
                let _ = s.flush();
            }
        });
        port
    }

    fn tmpfile(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wpnet-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[tokio::test]
    async fn single_download() {
        let data = body();
        let port = spawn_server(data.clone(), true);
        let client = crate::client::build_client_default();
        let dest = tmpfile("single.bin");
        let _ = std::fs::remove_file(&dest);
        let n = download(
            &client,
            DownloadRequest::new(format!("http://127.0.0.1:{port}/f"), &dest)
                .expected_size(data.len() as u64),
            Box::new(|_, _| true),
        )
        .await
        .unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), data);
    }

    #[tokio::test]
    async fn segmented_download() {
        let data = body();
        let port = spawn_server(data.clone(), true);
        let client = crate::client::build_client_default();
        let dest = tmpfile("seg.bin");
        let _ = std::fs::remove_file(&dest);
        let n = download(
            &client,
            DownloadRequest::new(format!("http://127.0.0.1:{port}/f"), &dest)
                .expected_size(data.len() as u64)
                .segments(4),
            Box::new(|_, _| true),
        )
        .await
        .unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert!(!part_path(&dest).exists());
    }

    #[tokio::test]
    async fn resume_from_part() {
        let data = body();
        let port = spawn_server(data.clone(), true);
        let client = crate::client::build_client_default();
        let dest = tmpfile("resume.bin");
        let _ = std::fs::remove_file(&dest);
        // 预置一半
        let part = part_path(&dest);
        std::fs::write(&part, &data[..40_000]).unwrap();
        let n = download(
            &client,
            DownloadRequest::new(format!("http://127.0.0.1:{port}/f"), &dest)
                .expected_size(data.len() as u64),
            Box::new(|_, _| true),
        )
        .await
        .unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), data);
    }

    #[tokio::test]
    async fn cancel_keeps_part() {
        let data = body();
        let port = spawn_server(data.clone(), true);
        let client = crate::client::build_client_default();
        let dest = tmpfile("cancel.bin");
        let _ = std::fs::remove_file(&dest);
        let r = download(
            &client,
            DownloadRequest::new(format!("http://127.0.0.1:{port}/f"), &dest),
            Box::new(|done, _| done < 10),
        )
        .await;
        assert!(matches!(r, Err(Error::Canceled(_))));
        assert!(part_path(&dest).exists());
    }
}
