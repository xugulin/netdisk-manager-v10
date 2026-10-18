//! 下载：取直链 + 流式落盘（含 `__puus` 补全与 CDN 403 重试一次）。
//!
//! 夸克直链内嵌 `RequestDeniedByCallback` 防盗链回调，回调会校验
//! `${httpHeader.cookie}` 与 `__puus`。因此请求头必须带完整 Cookie + Referer，
//! 被拒后强制 `GET member` 刷新 `__puus` 再重试一次（并把新值持久化）。

use crate::{as_str, as_u64, data_of, QuarkDrive, UA_QQ};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use wp_core::model::{DirectLink, DownloadOptions, ProgressFn, RemoteEntry};
use wp_core::{Error, Result};
use wp_net::download::{download, DownloadRequest};

fn pick_download_item(data: &Value, fid: &str) -> Option<Value> {
    let list = if data.is_array() {
        data.as_array().cloned().unwrap_or_default()
    } else {
        data.get("list")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let has_url = |item: &&Value| {
        as_str(item.get("download_url"))
            .map(|u| !u.trim().is_empty())
            .unwrap_or(false)
    };
    list.iter()
        .find(|item| as_str(item.get("fid")).as_deref() == Some(fid) && has_url(item))
        .or_else(|| list.iter().find(has_url))
        .cloned()
}

pub(crate) fn is_cdn_denied(text: &str) -> bool {
    let text = text.to_lowercase();
    text.contains("requestdeniedbycallback")
        || text.contains("auth miss")
        || text.contains("auth not found")
        || text.contains("require login")
}

fn forward_progress(shared: Arc<Mutex<ProgressFn>>) -> ProgressFn {
    Box::new(move |done, total| {
        let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
        guard(done, total)
    })
}

impl QuarkDrive {
    pub(crate) fn download_headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            ("user-agent".to_string(), UA_QQ.to_string()),
            ("referer".to_string(), "https://pan.quark.cn/".to_string()),
            ("accept".to_string(), "*/*".to_string()),
        ];
        let cookie = self.api.cookie_header();
        if !cookie.is_empty() {
            headers.push(("cookie".to_string(), cookie));
        }
        headers
    }

    /// `POST file/download {"fids":[fid]}`，精确匹配 fid 的直链，否则取第一条有直链的。
    pub(crate) async fn fetch_download_info(&self, fid: &str) -> Result<Value> {
        let resp = self
            .api
            .post("file/download", json!({"fids": [fid]}))
            .await?;
        let data = data_of(&resp);
        pick_download_item(&data, fid).ok_or_else(|| {
            Error::other(format!(
                "未取到下载直链（fid={fid}）：{}",
                crate::compact_json(&resp)
            ))
        })
    }

    pub(crate) async fn direct_link_for(&self, path: &str) -> Result<DirectLink> {
        self.api.ensure_valid()?;
        let entry = self.resolve_retry(path, 2, 400).await?;
        if entry.is_dir {
            return Err(Error::other(format!("目录没有下载直链：{path}")));
        }
        // 直链本身不强制 __puus，但下载会；顺手补一次（失败不阻塞取链接）。
        let _ = self.api.refresh_puus(false).await;
        let info = self.fetch_download_info(&entry.id).await?;
        let url = as_str(info.get("download_url")).unwrap_or_default();
        if url.trim().is_empty() {
            return Err(Error::other(format!("未取到下载直链（fid={}）", entry.id)));
        }
        let size = as_u64(info.get("size")).unwrap_or(entry.size);
        let file_name = as_str(info.get("file_name"))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| entry.name.clone());
        Ok(DirectLink {
            url,
            headers: self.download_headers(),
            expire_at: None,
            size,
            file_name,
        })
    }

    pub(crate) async fn download_to_file(
        &self,
        remote: &str,
        local: &Path,
        opts: &DownloadOptions,
        progress: ProgressFn,
    ) -> Result<u64> {
        self.api.ensure_valid()?;
        let entry: RemoteEntry = self.resolve_retry(remote, 2, 400).await?;
        if entry.is_dir {
            return Err(Error::other(format!("不能下载目录：{remote}")));
        }
        let _ = self.api.refresh_puus(false).await;
        let info = self.fetch_download_info(&entry.id).await?;
        let url = as_str(info.get("download_url")).unwrap_or_default();
        if url.trim().is_empty() {
            return Err(Error::other(format!("未取到下载直链（fid={}）", entry.id)));
        }
        let size = as_u64(info.get("size")).unwrap_or(entry.size);
        let file_name = as_str(info.get("file_name"))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| entry.name.clone());

        let mut dest: PathBuf = opts.save_as.clone().unwrap_or_else(|| local.to_path_buf());
        if dest.is_dir() {
            dest = dest.join(&file_name);
        }
        // 目标已存在时先挪到备份：下载失败/取消可恢复原文件；也避开
        // Windows 上 std::fs::rename 不覆盖已存在目标的问题。
        let backup = if dest.exists() {
            let name = dest
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "download".into());
            let bak = dest.with_file_name(format!(".{name}.wp-bak"));
            let _ = std::fs::remove_file(&bak);
            std::fs::rename(&dest, &bak).map_err(Error::Io)?;
            Some(bak)
        } else {
            None
        };

        let progress = Arc::new(Mutex::new(progress));
        let build_request = |url: &str, headers: &[(String, String)]| {
            let mut req = DownloadRequest::new(url.to_string(), dest.clone())
                .headers(headers)
                .expected_size(size)
                .segments(opts.segments.max(1))
                .resume(opts.resume)
                .limit_bps(opts.limit_bps);
            req.segment_threshold = opts.segment_threshold;
            req
        };

        let outcome: Result<u64> = async {
            let headers = self.download_headers();
            let req = build_request(&url, &headers);
            match download(&self.download_http, req, forward_progress(progress.clone())).await {
                Ok(n) => {
                    self.warn_size_mismatch(&dest, size, n);
                    Ok(n)
                }
                Err(e) if is_cdn_denied(&e.to_string()) => {
                    wp_core::log_warn!(
                        "夸克",
                        "下载被 CDN 回调拒绝，强制刷新 __puus 后重试一次：{}",
                        wp_core::error::err_line(&e)
                    );
                    let _ = self.api.refresh_puus(true).await;
                    let headers = self.download_headers();
                    let req = build_request(&url, &headers);
                    match download(&self.download_http, req, forward_progress(progress.clone()))
                        .await
                    {
                        Ok(n) => {
                            self.warn_size_mismatch(&dest, size, n);
                            Ok(n)
                        }
                        Err(e2) if is_cdn_denied(&e2.to_string()) => Err(Error::auth(format!(
                            "下载被 CDN 拒绝（403 RequestDeniedByCallback / auth miss）：\
                             Cookie 缺少或已失效的 __puus，刷新后仍失败：{}",
                            wp_core::error::err_line(&e2)
                        ))),
                        Err(e2) => Err(e2),
                    }
                }
                Err(e) => Err(e),
            }
        }
        .await;

        if let Some(bak) = backup {
            if outcome.is_ok() {
                let _ = std::fs::remove_file(&bak);
            } else {
                if dest.exists() {
                    let _ = std::fs::remove_file(&dest);
                }
                if let Err(e) = std::fs::rename(&bak, &dest) {
                    wp_core::log_warn!(
                        "夸克",
                        "下载失败后恢复原文件失败：{}（备份保留在 {}）",
                        wp_core::error::err_line(&Error::Io(e)),
                        bak.display()
                    );
                }
            }
        }
        outcome
    }

    fn warn_size_mismatch(&self, dest: &Path, expected: u64, got: u64) {
        if expected > 0 && got != expected {
            wp_core::log_warn!(
                "夸克",
                "下载完成但字节数不一致：期望 {expected}，实际 {got}（{}）",
                dest.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdn_denial_detection() {
        assert!(is_cdn_denied(
            "下载失败：HTTP 403 RequestDeniedByCallback / require login [auth miss]"
        ));
        assert!(is_cdn_denied("403 auth miss"));
        assert!(!is_cdn_denied("HTTP 500 internal error"));
        assert!(!is_cdn_denied("timeout"));
    }

    #[test]
    fn download_item_exact_and_fallback() {
        let data = json!({
            "list": [
                {"fid":"other","download_url":"https://a/1"},
                {"fid":"f1","download_url":"https://a/2"},
                {"fid":"f2","download_url":""}
            ]
        });
        let item = pick_download_item(&data, "f1").unwrap();
        assert_eq!(item["download_url"], "https://a/2");

        // 精确 fid 没有直链时，退回到第一条有直链的。
        let item = pick_download_item(&data, "f2").unwrap();
        assert_eq!(item["fid"], "other");

        let flat = json!([{"fid":"x","download_url":"u"}]);
        assert_eq!(pick_download_item(&flat, "y").unwrap()["fid"], "x");

        assert!(pick_download_item(&json!({"list":[]}), "x").is_none());
        assert!(
            pick_download_item(&json!({"list":[{"fid":"x","download_url":""}]}), "x").is_none()
        );
    }
}
