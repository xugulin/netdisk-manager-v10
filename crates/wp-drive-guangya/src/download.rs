//! 下载：取签名直链 + `wp-net` 流式下载（断点续传/多段/限速/取消）。
//!
//! Python GUI 的下载是 `httpx.stream` 单连接、无 Range、无校验；这里改用
//! `wp_net::download`，能力更强但请求语义保持一致：**直链请求不带 Bearer/设备头**。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use wp_core::model::{DirectLink, DownloadOptions, ProgressFn};
use wp_core::util::{now_ts, sanitize_file_name};
use wp_core::{Error, Result};

use crate::{as_i64, data_or_self, files, GuangyaDrive};

/// 取下载直链（不真正下载）。
pub(crate) async fn direct_link(drive: &GuangyaDrive, path: &str) -> Result<DirectLink> {
    let entry = files::resolve_entry(drive, path).await?;
    if entry.is_dir {
        return Err(Error::unsupported("目录不支持下载直链"));
    }
    let value = drive
        .api_post(
            "/userres/v1/get_res_download_url",
            json!({ "fileId": entry.id }),
            &[],
        )
        .await?;
    parse_direct_link(data_or_self(&value), &entry.name, entry.size)
}

/// 从 `get_res_download_url` 的 data 解析直链（纯逻辑，便于单测）。
pub(crate) fn parse_direct_link(
    data: &Value,
    fallback_name: &str,
    fallback_size: u64,
) -> Result<DirectLink> {
    let url = ["signedURL", "signedUrl", "url", "downloadUrl"]
        .iter()
        .find_map(|k| data.get(*k).and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string();
    if url.is_empty() {
        return Err(Error::server(
            0,
            format!(
                "取下载地址响应缺少 signedURL：{}",
                crate::snippet(&data.to_string(), 300)
            ),
        ));
    }
    let duration = data.get("urlDuration").and_then(as_i64).filter(|n| *n > 0);
    let expire_at = duration.map(|d| {
        // urlDuration 单位未能从源码确认（协议 §6.1）：大到像 Unix 时间戳就按时间戳解释。
        if d > 1_000_000_000 {
            d
        } else {
            now_ts() + d
        }
    });
    let size = data
        .get("fileSize")
        .and_then(as_i64)
        .filter(|n| *n >= 0)
        .map(|n| n as u64)
        .unwrap_or(fallback_size);
    let file_name = data
        .get("fileName")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(fallback_name)
        .to_string();
    Ok(DirectLink {
        url,
        headers: Vec::new(),
        expire_at,
        size,
        file_name,
        is_hls: false,
    })
}

/// 下载到本地。
pub(crate) async fn download(
    drive: &GuangyaDrive,
    remote: &str,
    local: &Path,
    opts: &DownloadOptions,
    progress: ProgressFn,
) -> Result<u64> {
    let entry = files::resolve_entry(drive, remote).await?;
    if entry.is_dir {
        return Err(Error::unsupported("不能下载目录，请选择文件"));
    }
    let link = direct_link(drive, remote).await?;
    let dest = choose_local_path(local, opts, &entry.name)?;

    let mut req = wp_net::download::DownloadRequest::new(link.url, dest.clone())
        .expected_size(if link.size > 0 { link.size } else { entry.size })
        .segments(opts.segments.max(1))
        .resume(opts.resume)
        .limit_bps(opts.limit_bps);
    req.segment_threshold = opts.segment_threshold;
    let expected = req.expected_size;

    let done = wp_net::download::download(&drive.xfer, req, progress).await?;
    if expected > 0 && done != expected {
        return Err(Error::network(format!(
            "下载大小不一致：期望 {expected} 字节，实际 {done} 字节（{}）",
            dest.display()
        )));
    }
    Ok(done)
}

/// 选择本地落点：显式文件路径优先；给目录时拼远端文件名；未给路径用 `save_as`。
pub(crate) fn choose_local_path(
    local: &Path,
    opts: &DownloadOptions,
    remote_name: &str,
) -> Result<PathBuf> {
    let safe_name = sanitize_file_name(remote_name);
    let name = if safe_name.trim().is_empty() {
        "download.bin".to_string()
    } else {
        safe_name
    };
    if !local.as_os_str().is_empty() {
        if local.is_dir() {
            return Ok(local.join(name));
        }
        return Ok(local.to_path_buf());
    }
    if let Some(save_as) = &opts.save_as {
        if save_as.as_os_str().is_empty() {
            return Err(Error::other("未指定下载保存路径"));
        }
        if save_as.is_dir() {
            return Ok(save_as.join(name));
        }
        return Ok(save_as.clone());
    }
    Err(Error::other("未指定下载保存路径"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_link_parsing() {
        let data = json!({
            "signedURL":"https://cdn.example.com/a?sign=1&x=2",
            "urlDuration": 3600,
            "speedupSignature":"x",
            "requestId":"r"
        });
        let link = parse_direct_link(&data, "a.mkv", 1024).unwrap();
        assert_eq!(link.url, "https://cdn.example.com/a?sign=1&x=2");
        assert_eq!(link.size, 1024);
        assert_eq!(link.file_name, "a.mkv");
        assert!(link.expire_at.unwrap() > now_ts());
        assert!(link.headers.is_empty());

        let ts =
            parse_direct_link(&json!({"signedUrl":"u","urlDuration":1700000000}), "b", 1).unwrap();
        assert_eq!(ts.expire_at, Some(1_700_000_000));

        assert!(parse_direct_link(&json!({"url":""}), "c", 0).is_err());
    }

    #[test]
    fn local_path_selection() {
        let dir = std::env::temp_dir().join(format!(
            "wp-gy-dl-{}-{}",
            std::process::id(),
            wp_core::util::random_hex(4)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let opts = DownloadOptions::default();

        let p = choose_local_path(&dir, &opts, "a/b:c?.mkv").unwrap();
        assert_eq!(p.parent().unwrap(), dir);
        assert_eq!(p.file_name().unwrap().to_string_lossy(), "a_b_c_.mkv");

        let file = dir.join("out.bin");
        assert_eq!(choose_local_path(&file, &opts, "x").unwrap(), file);
        assert!(choose_local_path(Path::new(""), &opts, "x").is_err());

        let opts2 = DownloadOptions {
            save_as: Some(dir.clone()),
            ..Default::default()
        };
        assert_eq!(
            choose_local_path(Path::new(""), &opts2, "y.txt").unwrap(),
            dir.join("y.txt")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
