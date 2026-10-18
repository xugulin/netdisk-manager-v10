//! M0-E1：`probe_range` —— 对一条真实 `DirectLink` 做 HEAD / 首段 Range / 中段 Range 探测，
//! 输出结构化结论（状态码、Content-Range、Accept-Ranges、耗时、错误），供 CLI `probe-range`
//! 与真机验收记录使用。
//!
//! 探测**只读**：HEAD + 两次小范围 GET（各 ≤1KB 请求头，不读 body），不会下载整个文件。

use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use wp_core::Result;
use wp_drive::Drive;

use crate::upstream::{
    DriveLinkProvider, LinkProvider, RawResponse, UpstreamClient, UpstreamConfig,
};

/// 单步探测结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProbeStep {
    /// HTTP 状态码（None = 请求未发出/传输层失败）。
    pub status: Option<u16>,
    /// `Content-Range` 响应头。
    pub content_range: Option<String>,
    /// `Accept-Ranges` 响应头。
    pub accept_ranges: Option<String>,
    /// `Content-Length` 响应头。
    pub content_length: Option<u64>,
    /// `Content-Type` 响应头。
    pub content_type: Option<String>,
    /// 耗时（毫秒）。
    pub elapsed_ms: u64,
    /// 该步是否达到预期（HEAD 只要 2xx；Range 要求 206）。
    pub ok: bool,
    /// 传输层错误 / 非 2xx 响应摘要。
    pub error: Option<String>,
}

impl ProbeStep {
    fn from_raw(raw: &RawResponse, require_206: bool) -> Self {
        let ok = if require_206 {
            raw.status == 206
        } else {
            raw.is_success()
        };
        let error = if ok {
            None
        } else {
            Some(if raw.error_snippet.is_empty() {
                format!("HTTP {}", raw.status)
            } else {
                format!("HTTP {}：{}", raw.status, raw.error_snippet.trim())
            })
        };
        Self {
            status: Some(raw.status),
            content_range: raw.content_range.clone(),
            accept_ranges: raw.accept_ranges.clone(),
            content_length: raw.content_length,
            content_type: raw.content_type.clone(),
            elapsed_ms: raw.elapsed_ms,
            ok,
            error,
        }
    }

    fn transport_error(e: impl std::fmt::Display) -> Self {
        Self {
            status: None,
            elapsed_ms: 0,
            error: Some(e.to_string()),
            ..Default::default()
        }
    }
}

/// 三盘 Range 能力探测报告（CLI 直接序列化为 JSON）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RangeProbe {
    /// 网盘实例标识（如 `baidu:baidu`）。
    pub drive: String,
    /// 远端路径。
    pub path: String,
    /// 直链的 host（不含 query，便于记录/对比）。
    pub url_host: String,
    /// 文件总大小（来自 DirectLink.size / HEAD Content-Length / Content-Range）。
    pub size: Option<u64>,
    /// 取直链耗时（毫秒）。
    pub direct_link_ms: u64,
    /// 直链过期时间戳（秒；None = 适配器未给）。
    pub expire_at: Option<i64>,
    /// 直链注入的请求头名（不打印敏感值）。
    pub header_names: Vec<String>,
    /// 直链是否为 HLS（m3u8）流；`true` 时 `probe_range` 直接返回错误（无单文件 Range 语义）。
    pub is_hls: bool,
    /// HEAD 结果。
    pub head: Option<ProbeStep>,
    /// 首段 `bytes=0-1023` 结果。
    pub first: Option<ProbeStep>,
    /// 中段（`size/2` 起 1KB）结果。
    pub mid: Option<ProbeStep>,
    /// 是否支持单段 Range（首段返回 206 即视为支持）。
    pub range_supported: bool,
    /// 上游是否忽略 Range（返回 200 全量）。
    pub upstream_ignores_range: bool,
    /// 整体错误（取直链失败等；分步错误在各 step.error 里）。
    pub error: Option<String>,
}

/// 对指定网盘文件做 Range 能力探测。
pub async fn probe_range(drive: Arc<dyn Drive>, remote_path: &str) -> Result<RangeProbe> {
    let provider: Arc<dyn LinkProvider> = Arc::new(DriveLinkProvider::new(drive));
    probe_with_provider(provider, remote_path).await
}

/// 同 [`probe_range`]，但直接接受 [`LinkProvider`]（单测/离线探测用）。
pub async fn probe_with_provider(
    provider: Arc<dyn LinkProvider>,
    remote_path: &str,
) -> Result<RangeProbe> {
    let client = UpstreamClient::new(UpstreamConfig::default())?;
    let mut report = RangeProbe {
        drive: provider.cache_key(),
        path: remote_path.to_string(),
        ..Default::default()
    };

    // 1) 取直链
    let t0 = Instant::now();
    let link = match provider.direct_link(remote_path).await {
        Ok(link) => {
            report.direct_link_ms = t0.elapsed().as_millis() as u64;
            link
        }
        Err(e) => {
            report.direct_link_ms = t0.elapsed().as_millis() as u64;
            report.error = Some(format!("取直链失败：{e}"));
            return Ok(report);
        }
    };
    report.url_host = host_of(&link.url);
    report.expire_at = link.expire_at;
    report.is_hls = link.is_hls;
    report.header_names = link.headers.iter().map(|(k, _)| k.clone()).collect();
    if link.size > 0 {
        report.size = Some(link.size);
    }
    if link.is_hls {
        // HLS/m3u8 没有「单文件 Range」概念（播放列表 + 分片），探测无意义。
        return Err(wp_core::Error::unsupported(
            "HLS 流不支持 Range 探测（is_hls=true，应把 m3u8 URL+headers 直接交给播放器）",
        ));
    }
    if link.url.is_empty() {
        report.error = Some("直链为空".into());
        return Ok(report);
    }

    // 2) HEAD
    match client.head(&link).await {
        Ok(raw) => {
            if report.size.is_none() {
                report.size = raw.content_length;
            }
            report.head = Some(ProbeStep::from_raw(&raw, false));
        }
        Err(e) => report.head = Some(ProbeStep::transport_error(e)),
    }

    // 3) 首段 Range：bytes=0-1023
    match client.probe_get(&link, 0, 1023).await {
        Ok(raw) => {
            report.range_supported = raw.status == 206;
            report.upstream_ignores_range = raw.upstream_ignored_range;
            if report.size.is_none() {
                report.size = raw
                    .content_range
                    .as_deref()
                    .and_then(UpstreamClient::parse_content_range_total);
            }
            report.first = Some(ProbeStep::from_raw(&raw, true));
        }
        Err(e) => report.first = Some(ProbeStep::transport_error(e)),
    }

    // 4) 中段 Range（文件够大才有意义）
    if let Some(size) = report.size {
        if size >= 4096 {
            let start = size / 2;
            let end = start + 1023;
            match client.probe_get(&link, start, end).await {
                Ok(raw) => {
                    let mid_ok = raw.status == 206
                        && raw
                            .content_range
                            .as_deref()
                            .and_then(UpstreamClient::parse_content_range_start)
                            .map(|s| s == start)
                            .unwrap_or(false);
                    let mut step = ProbeStep::from_raw(&raw, true);
                    if !mid_ok {
                        step.ok = false;
                        if step.error.is_none() {
                            step.error = Some(format!(
                                "中段 Range 响应异常：status={} content-range={:?}",
                                raw.status, raw.content_range
                            ));
                        }
                    }
                    report.mid = Some(step);
                }
                Err(e) => report.mid = Some(ProbeStep::transport_error(e)),
            }
        }
    }

    // 汇总一条整体错误，方便 CLI/验收脚本一眼看出结论
    if !report.range_supported {
        if let Some(first) = &report.first {
            report.error = Some(format!(
                "首段 Range 未返回 206（status={:?}，accept-ranges={:?}，content-range={:?}）",
                first.status, first.accept_ranges, first.content_range
            ));
        }
    }
    Ok(report)
}

fn host_of(url: &str) -> String {
    let Some((_, rest)) = url.split_once("://") else {
        return String::new();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    rest[..end].to_string()
}
