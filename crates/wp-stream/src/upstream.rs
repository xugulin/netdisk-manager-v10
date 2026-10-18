//! 上游访问层：`LinkProvider`（按路径取直链）与 reqwest 单段 Range 拉取。
//!
//! 设计要点：
//! * 代理不关心具体网盘：只依赖 [`LinkProvider`]（`direct_link(path) -> url + headers`）；
//! * 直链里的 `headers`（百度是 Cookie/Referer、夸克是 UA 等）**原样注入**上游请求；
//! * `Accept-Encoding: identity`：媒体文件不该被压缩，同时保证 `Content-Length`/`Content-Range`
//!   与实际字节一致；
//! * 上游返回 `206` 正常；返回 `200`（忽略 Range）时做「整文件回退」——丢弃前 `start` 字节
//!   再取所需段，并置 `upstream_ignored_range = true` 供调用方记录/告警；
//! * 非 2xx 状态**不在这里抛错**，而是原样带回给代理层分类（401/403/410 → 换链等）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::header::{
    HeaderName, HeaderValue, ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_TYPE, RANGE,
};
use wp_core::model::DirectLink;
use wp_core::Result;
use wp_drive::Drive;

/// 直链提供者：代理与测试都可实现。
#[async_trait]
pub trait LinkProvider: Send + Sync {
    /// 缓存 key 用的实例稳定标识（如 `baidu:baidu`）。
    fn cache_key(&self) -> String;
    /// 取远端路径的直链。
    async fn direct_link(&self, remote_path: &str) -> Result<DirectLink>;
}

/// 真实网盘适配器 → `LinkProvider` 的适配层。
pub struct DriveLinkProvider {
    drive: Arc<dyn Drive>,
}

impl DriveLinkProvider {
    pub fn new(drive: Arc<dyn Drive>) -> Self {
        Self { drive }
    }
}

#[async_trait]
impl LinkProvider for DriveLinkProvider {
    fn cache_key(&self) -> String {
        format!("{}:{}", self.drive.kind().slug(), self.drive.id())
    }
    async fn direct_link(&self, remote_path: &str) -> Result<DirectLink> {
        self.drive.direct_link(remote_path).await
    }
}

/// 上游 HTTP 客户端配置。
#[derive(Debug, Clone)]
pub struct UpstreamConfig {
    /// 建立连接超时。
    pub connect_timeout: Duration,
    /// 单次读取/等待响应超时（超过视为可重试的网络错误）。
    pub read_timeout: Duration,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(30),
        }
    }
}

/// 传输层错误（区别于「上游返回了非 2xx」）。
#[derive(Debug, Clone)]
pub enum TransportError {
    /// 超时（可重试，通常需要换链）。
    Timeout(String),
    /// 连接/读写出错（可重试）。
    Network(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Timeout(m) => write!(f, "上游超时：{m}"),
            TransportError::Network(m) => write!(f, "上游网络错误：{m}"),
        }
    }
}

/// 一次上游请求的原始响应（非 2xx 也返回，供上层分类）。
#[derive(Debug, Clone, Default)]
pub struct RawResponse {
    pub status: u16,
    /// 响应头 `Content-Length`（HEAD 时是文件大小；206 时是本段长度）。
    pub content_length: Option<u64>,
    /// 响应头 `Content-Range`。
    pub content_range: Option<String>,
    /// 响应头 `Accept-Ranges`。
    pub accept_ranges: Option<String>,
    /// 响应头 `Content-Type`。
    pub content_type: Option<String>,
    /// 2xx 时读到的数据（206=请求段；200=已裁掉前 `start` 字节的请求段）。
    pub body: Vec<u8>,
    /// 上游是否忽略了 Range 直接回了 200（整文件回退路径）。
    pub upstream_ignored_range: bool,
    /// 请求耗时。
    pub elapsed_ms: u64,
    /// 非 2xx 时的响应体摘要（≤512 字节，诊断用）。
    pub error_snippet: String,
}

impl RawResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
    /// 是否是「链接失效类」状态（换链后重试）。
    pub fn is_auth_expired(&self) -> bool {
        matches!(self.status, 401 | 403 | 410)
    }
    /// 是否是「暂时性」状态（限流/服务端错误，可换链后重试）。
    pub fn is_transient(&self) -> bool {
        matches!(self.status, 408 | 425 | 429 | 500 | 502 | 503 | 504)
    }
}

/// reqwest 封装。
#[derive(Clone)]
pub struct UpstreamClient {
    client: reqwest::Client,
    config: UpstreamConfig,
}

impl UpstreamClient {
    pub fn new(config: UpstreamConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("wp-stream/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(config.connect_timeout)
            // 不做全局 timeout：大块下载总时长可能远超单次读取超时，
            // 我们用 tokio::time::timeout 包每次 send/read。
            .build()
            .map_err(|e| wp_core::Error::network(format!("构建上游 HTTP 客户端失败：{e}")))?;
        Ok(Self { client, config })
    }

    pub fn config(&self) -> &UpstreamConfig {
        &self.config
    }

    fn apply_headers(
        &self,
        mut req: reqwest::RequestBuilder,
        link: &DirectLink,
    ) -> reqwest::RequestBuilder {
        for (k, v) in &link.headers {
            let Ok(name) = HeaderName::from_bytes(k.as_bytes()) else {
                continue;
            };
            // 这些头由代理自己控制，直链里即便带了也不照抄
            if matches!(
                name.as_str(),
                "range" | "content-length" | "accept-encoding" | "connection" | "host"
            ) {
                continue;
            }
            let Ok(value) = HeaderValue::from_str(v) else {
                continue;
            };
            req = req.header(name, value);
        }
        req.header(ACCEPT_ENCODING, "identity")
    }

    /// HEAD 探测/取文件大小。
    pub async fn head(
        &self,
        link: &DirectLink,
    ) -> std::result::Result<RawResponse, TransportError> {
        let started = Instant::now();
        let req = self.client.head(&link.url);
        let req = self.apply_headers(req, link);
        let resp = timeout(
            self.config.read_timeout,
            self.config.connect_timeout,
            req.send(),
        )
        .await?;
        let status = resp.status().as_u16();
        let raw = RawResponse {
            status,
            content_length: header_u64(&resp, CONTENT_LENGTH),
            content_range: header_str(&resp, CONTENT_RANGE),
            accept_ranges: header_str(&resp, ACCEPT_RANGES),
            content_type: header_str(&resp, CONTENT_TYPE),
            elapsed_ms: started.elapsed().as_millis() as u64,
            ..Default::default()
        };
        Ok(raw)
    }

    /// GET 单段 Range（`start..=end`）。上游忽略 Range 时自动整文件回退裁剪。
    pub async fn get_range(
        &self,
        link: &DirectLink,
        start: u64,
        end: u64,
    ) -> std::result::Result<RawResponse, TransportError> {
        let started = Instant::now();
        let need = (end - start + 1) as usize;
        let req = self.client.get(&link.url);
        let req = self.apply_headers(req, link);
        let req = req.header(RANGE, format!("bytes={start}-{end}"));
        let resp = timeout(
            self.config.read_timeout,
            self.config.connect_timeout,
            req.send(),
        )
        .await?;
        let status = resp.status().as_u16();
        let mut raw = RawResponse {
            status,
            content_length: header_u64(&resp, CONTENT_LENGTH),
            content_range: header_str(&resp, CONTENT_RANGE),
            accept_ranges: header_str(&resp, ACCEPT_RANGES),
            content_type: header_str(&resp, CONTENT_TYPE),
            ..Default::default()
        };
        match status {
            206 => {
                raw.body = self.read_exact_body(resp, need).await?;
                raw.elapsed_ms = started.elapsed().as_millis() as u64;
                Ok(raw)
            }
            200 => {
                // 上游忽略 Range：整文件回退（先丢弃 start 字节，再截取 need 字节）。
                raw.upstream_ignored_range = true;
                raw.body = self.read_skip_take(resp, start, need).await?;
                raw.elapsed_ms = started.elapsed().as_millis() as u64;
                Ok(raw)
            }
            _ => {
                raw.error_snippet = self.read_snippet(resp).await;
                raw.elapsed_ms = started.elapsed().as_millis() as u64;
                Ok(raw)
            }
        }
    }

    /// 探测专用 GET：发 Range 但**不读 body**（只取状态/响应头），避免为探测下载巨量数据。
    ///
    /// 200 时置 `upstream_ignored_range=true`（说明上游忽略 Range）；非 2xx 时读 ≤512B 摘要。
    pub async fn probe_get(
        &self,
        link: &DirectLink,
        start: u64,
        end: u64,
    ) -> std::result::Result<RawResponse, TransportError> {
        let started = Instant::now();
        let req = self.client.get(&link.url);
        let req = self.apply_headers(req, link);
        let req = req.header(RANGE, format!("bytes={start}-{end}"));
        let resp = timeout(
            self.config.read_timeout,
            self.config.connect_timeout,
            req.send(),
        )
        .await?;
        let status = resp.status().as_u16();
        let mut raw = RawResponse {
            status,
            content_length: header_u64(&resp, CONTENT_LENGTH),
            content_range: header_str(&resp, CONTENT_RANGE),
            accept_ranges: header_str(&resp, ACCEPT_RANGES),
            content_type: header_str(&resp, CONTENT_TYPE),
            ..Default::default()
        };
        if status == 200 {
            raw.upstream_ignored_range = true;
        } else if !raw.is_success() {
            raw.error_snippet = self.read_snippet(resp).await;
        }
        // 2xx（206）不读 body：响应 drop 时连接关闭/归还，探测足够。
        raw.elapsed_ms = started.elapsed().as_millis() as u64;
        Ok(raw)
    }

    /// 读 `need` 字节；不足视为网络错误（可重试）。
    async fn read_exact_body(
        &self,
        mut resp: reqwest::Response,
        need: usize,
    ) -> std::result::Result<Vec<u8>, TransportError> {
        let mut out = Vec::with_capacity(need.min(8 * 1024 * 1024));
        loop {
            let chunk = timeout(
                self.config.read_timeout,
                self.config.connect_timeout,
                resp.chunk(),
            )
            .await?;
            match chunk {
                Some(bytes) => {
                    out.extend_from_slice(&bytes);
                    if out.len() >= need {
                        out.truncate(need);
                        return Ok(out);
                    }
                }
                None => break,
            }
        }
        if out.len() < need {
            return Err(TransportError::Network(format!(
                "上游 206 返回数据不足：期望 {need}，实际 {}",
                out.len()
            )));
        }
        Ok(out)
    }

    /// 从 200 全量响应里跳过 `skip` 字节，取 `take` 字节。
    async fn read_skip_take(
        &self,
        mut resp: reqwest::Response,
        skip: u64,
        take: usize,
    ) -> std::result::Result<Vec<u8>, TransportError> {
        let mut out = Vec::with_capacity(take.min(8 * 1024 * 1024));
        let mut skipped = 0u64;
        loop {
            let chunk = timeout(
                self.config.read_timeout,
                self.config.connect_timeout,
                resp.chunk(),
            )
            .await?;
            match chunk {
                Some(bytes) => {
                    let mut data: &[u8] = &bytes;
                    if skipped < skip {
                        let remain = (skip - skipped) as usize;
                        if data.len() <= remain {
                            skipped += data.len() as u64;
                            continue;
                        }
                        data = &data[remain..];
                        skipped = skip;
                    }
                    out.extend_from_slice(data);
                    if out.len() >= take {
                        out.truncate(take);
                        return Ok(out);
                    }
                }
                None => break,
            }
        }
        Err(TransportError::Network(format!(
            "上游忽略 Range 回退失败：需要跳过 {skip} 取 {take}，只读到 {}",
            out.len()
        )))
    }

    /// 读非 2xx 响应体摘要（诊断用，失败不报错）。
    async fn read_snippet(&self, mut resp: reqwest::Response) -> String {
        let mut out: Vec<u8> = Vec::new();
        for _ in 0..4 {
            match timeout(
                self.config.read_timeout,
                self.config.connect_timeout,
                resp.chunk(),
            )
            .await
            {
                Ok(Some(bytes)) => {
                    out.extend_from_slice(&bytes);
                    if out.len() >= 512 {
                        break;
                    }
                }
                _ => break,
            }
        }
        out.truncate(512);
        String::from_utf8_lossy(&out).to_string()
    }

    /// 解析 `Content-Range: bytes a-b/total` 的总大小。
    pub fn parse_content_range_total(value: &str) -> Option<u64> {
        let value = value.trim();
        let rest = value.strip_prefix("bytes")?.trim_start();
        let (_, total) = rest.split_once('/')?;
        total.trim().parse::<u64>().ok()
    }

    /// 解析 `Content-Range: bytes a-b/total` 的起始偏移。
    pub fn parse_content_range_start(value: &str) -> Option<u64> {
        let value = value.trim();
        let rest = value.strip_prefix("bytes")?.trim_start();
        let (range_part, _) = rest.split_once('/')?;
        let (a, _) = range_part.split_once('-')?;
        a.trim().parse::<u64>().ok()
    }
}

/// 请求超时小工具：`connect_timeout` 只作用于建连（reqwest 内部），这里再兜一层总超时。
fn timeout<F, T>(
    read: Duration,
    connect: Duration,
    fut: F,
) -> impl std::future::Future<Output = std::result::Result<T, TransportError>>
where
    F: std::future::Future<Output = std::result::Result<T, reqwest::Error>>,
{
    let total = read.max(connect) + connect;
    async move {
        match tokio::time::timeout(total, fut).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(classify_reqwest_error(e)),
            Err(_) => Err(TransportError::Timeout(format!(
                "请求超过 {} 秒",
                total.as_secs()
            ))),
        }
    }
}

fn classify_reqwest_error(e: reqwest::Error) -> TransportError {
    if e.is_timeout() {
        TransportError::Timeout(e.to_string())
    } else {
        TransportError::Network(e.to_string())
    }
}

fn header_str(resp: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn header_u64(resp: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<u64> {
    header_str(resp, name).and_then(|s| s.trim().parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range;

    #[test]
    fn content_range_parsing() {
        assert_eq!(
            UpstreamClient::parse_content_range_total("bytes 0-1023/2048"),
            Some(2048)
        );
        assert_eq!(
            UpstreamClient::parse_content_range_total("bytes */2048"),
            Some(2048)
        );
        assert_eq!(UpstreamClient::parse_content_range_total("garbage"), None);
        assert_eq!(
            UpstreamClient::parse_content_range_start("bytes 100-199/2048"),
            Some(100)
        );
        assert_eq!(
            UpstreamClient::parse_content_range_start("bytes 100-199/*"),
            Some(100)
        );
    }

    #[test]
    fn status_classification() {
        let mut r = RawResponse {
            status: 403,
            ..Default::default()
        };
        assert!(r.is_auth_expired());
        assert!(!r.is_transient());
        r.status = 503;
        assert!(!r.is_auth_expired());
        assert!(r.is_transient());
        r.status = 206;
        assert!(r.is_success());
        assert!(!r.is_auth_expired() && !r.is_transient());
    }

    #[test]
    fn range_len_math() {
        assert_eq!(
            range::resolve_range(range::RangeSpec::FromTo { start: 10, end: 19 }, 100),
            Ok((10, 19))
        );
    }
}
