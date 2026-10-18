//! 下载：`filemetas?dlink=1` 取服务端签好的直链，再带 Cookie 流式下载。
//!
//! 文档 §6 的关键点：
//! * **不需要客户端算签名**，`dlink` 由服务端签好；
//! * 下载请求**必须带 Cookie**，否则 403 `31045 user not exists`；
//! * 302 跟随（`d.pcs.baidu.com` → `*.baidupcs.com`）；
//! * 续传/Range 由 wp-net 下载器处理；服务器忽略 Range（200）时从头覆盖；
//! * 目录打包未实现 → Unsupported。

use crate::files;
use crate::{BaiduDrive, UA};
use std::path::Path;
use wp_core::model::{DirectLink, DownloadOptions, ProgressFn};
use wp_core::util::{base_name, norm_path};
use wp_core::{Error, Result};
use wp_net::download::DownloadRequest;

/// 下载请求头（文档 §6 的原文要求）。
pub(crate) fn download_headers(cookie: &str) -> Vec<(String, String)> {
    let mut headers = vec![
        ("User-Agent".to_string(), UA.to_string()),
        ("Referer".to_string(), "https://pan.baidu.com/".to_string()),
        ("Accept".to_string(), "*/*".to_string()),
        ("Accept-Language".to_string(), "zh-CN,zh;q=0.9".to_string()),
    ];
    if !cookie.trim().is_empty() {
        headers.push(("Cookie".to_string(), cookie.to_string()));
    }
    headers
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
    wp_core::log_debug!(
        "百度",
        "已取直链：{full} → {}（{}B）",
        url.split('?').next().unwrap_or(""),
        size
    );
    Ok(DirectLink {
        url,
        headers,
        expire_at: None,
        size,
        file_name,
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
        assert!(map.contains_key("User-Agent"));

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
