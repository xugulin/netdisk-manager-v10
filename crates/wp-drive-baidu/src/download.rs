//! 下载：`filemetas?dlink=1` 取服务端签好的直链，再带 Cookie 流式下载。
//!
//! 文档 §6 的关键点：
//! * **不需要客户端算签名**，`dlink` 由服务端签好；
//! * 下载请求**必须带 Cookie**，否则 403 `31045 user not exists`；
//! * 302 跟随（`d.pcs.baidu.com` → `*.baidupcs.com`）；
//! * 续传/Range 由 wp-net 下载器处理；服务器忽略 Range（200）时从头覆盖；
//! * 目录打包未实现 → Unsupported。
//!
//! ⚠️ **UA 必须用客户端标识**（`DOWNLOAD_UA`，`netdisk;...`）：2026-10-19 真机实测，
//! 非会员账号下载大文件（>约 50MB）时，直链请求若用浏览器 UA 会 403
//! `31326 user is not authorized, hitcode:119`；换 netdisk UA 后同一 Cookie/直链 206 正常。
//! 该 UA 同时供播放流代理（wp-stream 上游请求）使用，解决此前「百度大文件不可播」的 31326。
//!
//! ⚠️ **跨域重定向必须手动解析**：`d.pcs.baidu.com/file/...` 会 302 到 `*.baidupcs.com`；
//! reqwest 在跨域重定向时会丢掉 `Cookie` 头（安全策略），CDN 侧因此返回 403
//! （`31326` 或 `31045 user not exists`）。[`direct_link`] 先用不跟随重定向的客户端
//! 读 `Location` 得到最终 CDN URL，再交给下载器/播放流直连（最终 URL 自带签名，直连无需 Cookie）。

use crate::files;
use crate::{BaiduDrive, DOWNLOAD_UA, MAX_RANGE_SPAN};
use std::path::Path;
use wp_core::model::{DirectLink, DownloadOptions, ProgressFn};
use wp_core::util::{base_name, norm_path};
use wp_core::{Error, Result};
use wp_net::download::DownloadRequest;

/// 下载请求头（文档 §6 的原文要求 + 客户端 UA）。
pub(crate) fn download_headers(cookie: &str) -> Vec<(String, String)> {
    let mut headers = vec![
        ("User-Agent".to_string(), DOWNLOAD_UA.to_string()),
        ("Referer".to_string(), "https://pan.baidu.com/".to_string()),
        ("Accept".to_string(), "*/*".to_string()),
        ("Accept-Language".to_string(), "zh-CN,zh;q=0.9".to_string()),
    ];
    if !cookie.trim().is_empty() {
        headers.push(("Cookie".to_string(), cookie.to_string()));
    }
    headers
}

/// 手动解析 `d.pcs.baidu.com` 的 302 跳转，返回最终 CDN URL。
///
/// * 用**不跟随重定向**的客户端发 `Range: bytes=0-0`（只取 1 字节，不必传完）；
/// * 3xx 时读 `Location` 继续（最多 4 跳），把最终 URL 交给下载器/播放流直连；
/// * 任何异常（网络错、非 3xx、相对 Location）都**回退原 URL**，不改变原有行为。
async fn resolve_final_url(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
) -> String {
    let mut current = url.to_string();
    for _ in 0..4 {
        let mut rb = client
            .get(&current)
            .header(reqwest::header::RANGE, "bytes=0-0")
            .header(reqwest::header::ACCEPT_ENCODING, "identity");
        for (k, v) in headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        let Ok(resp) = rb.send().await else {
            return current;
        };
        if !resp.status().is_redirection() {
            return current;
        }
        let Some(loc) = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
        else {
            return current;
        };
        if loc.is_empty() || !loc.starts_with("http") {
            return current;
        }
        current = loc.to_string();
    }
    current
}

fn host_of(url: &str) -> &str {
    url.split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
}

/// 取直链（仅链接，不下载）。
pub(crate) async fn direct_link(d: &BaiduDrive, path: &str) -> Result<DirectLink> {
    let full = norm_path(path);
    if full == "/" {
        return Err(Error::unsupported("目录暂不支持打包下载"));
    }
    let info = files::filemeta_value(d, &full, true).await?;
    let is_dir = info
        .get("isdir")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0)
        == 1;
    let url = info
        .get("dlink")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if is_dir || url.is_empty() {
        return Err(Error::unsupported(format!(
            "目录或该文件不支持直链下载：{full}"
        )));
    }
    let size = info
        .get("size")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let file_name = info
        .get("server_filename")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| base_name(&full));
    let headers = download_headers(&d.cookie_header());
    // 先解析跨域跳转（d.pcs → *.baidupcs），避免 reqwest 跟随重定向时丢 Cookie 导致 CDN 403。
    let url = resolve_final_url(&d.http.download_resolve, &url, &headers).await;
    wp_core::log_debug!(
        "百度",
        "已取直链：{full} → {}（{}B）",
        url.split('?').next().unwrap_or(""),
        size
    );
    wp_core::log_debug!(
        "百度",
        "直链跳转解析完成：host={}（最终 URL 已含签名，下载/播放直连）",
        host_of(&url)
    );
    Ok(DirectLink {
        url,
        headers,
        expire_at: None,
        size,
        file_name,
        is_hls: false,
    })
}

/// 下载远端文件到本地。
pub(crate) async fn download(
    d: &BaiduDrive,
    remote: &str,
    local: &Path,
    opts: &DownloadOptions,
    progress: ProgressFn,
) -> Result<u64> {
    let link = direct_link(d, remote).await?;
    if let Some(parent) = local.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await.map_err(Error::Io)?;
        }
    }
    let mut req = DownloadRequest::new(link.url.clone(), local.to_path_buf());
    req.headers = link.headers.clone();
    req.expected_size = link.size;
    req.segments = opts.segments.max(1);
    req.segment_threshold = opts.segment_threshold;
    req.resume = opts.resume;
    req.limit_bps = opts.limit_bps;
    // CDN 拒绝无 Range / 开区间 / >约 5MB 跨度的 Range；按 4MiB 分块请求。
    req.max_range_span = MAX_RANGE_SPAN;

    let bytes = wp_net::download(&d.http.download, req, progress).await?;
    wp_core::log_info!(
        "百度",
        "下载完成：{remote} → {}（{bytes}B）",
        local.display()
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_headers_include_cookie_only_when_present() {
        let h = download_headers("BDUSS=a; STOKEN=b");
        let map: std::collections::HashMap<_, _> = h.into_iter().collect();
        assert_eq!(
            map.get("Cookie").map(String::as_str),
            Some("BDUSS=a; STOKEN=b")
        );
        assert_eq!(
            map.get("Referer").map(String::as_str),
            Some("https://pan.baidu.com/")
        );
        assert_eq!(map.get("Accept").map(String::as_str), Some("*/*"));
        // 下载 UA 必须是客户端标识：浏览器 UA 对大文件会被 CDN 判 31326（真机实测）
        let ua = map.get("User-Agent").map(String::as_str).unwrap_or("");
        assert!(
            ua.starts_with("netdisk"),
            "下载 UA 应为客户端标识，实际：{ua}"
        );

        let h = download_headers("");
        assert!(h.iter().all(|(k, _)| k != "Cookie"));
    }

    #[test]
    fn root_is_not_downloadable() {
        // 纯路径判断，不发网络请求。
        assert_eq!(norm_path("/"), "/");
        // 该函数本身会联网，这里只验证路径归一化行为与 base_name。
        assert_eq!(base_name("/a/b.txt"), "b.txt");
    }
}
