//! 缩略图：从条目字段提取带签名的绝对 URL，再用本适配器的会话 Cookie 拉取字节。
//!
//! 夸克 `file/sort` / `file/list` / `file/info` 返回的条目里，缩略图字段名是
//! `thumbnail`（Python 源还兼容 `preview_url` / `thumb_url`）。URL 是绝对地址，
//! 可能落在管理域 `drive-pc.quark.cn`，也可能落在分享域 `drive.quark.cn`；
//! 因此不拼接域名，直接按 URL 请求。URL 文本里偶尔残留字面量 `\u0026`，
//! 需要还原成 `&` 才能正确解析签名参数。

use crate::{as_str, QuarkDrive};
use serde_json::Value;
use wp_core::model::RemoteEntry;
use wp_core::{Error, Result};

/// 缩略图下载超时（秒）。
pub(crate) const THUMBNAIL_TIMEOUT_SECS: u64 = 20;

/// 缩略图响应上限（5 MiB）：正常缩略图远小于该值，超限直接判失败。
pub(crate) const MAX_THUMBNAIL_BYTES: usize = 5 * 1024 * 1024;

/// 从 `file/sort` / `file/list` / `file/info` 的响应条目里提取缩略图 URL。
pub(crate) fn thumbnail_from_raw(raw: &Value) -> Option<String> {
    ["thumbnail", "preview_url", "thumb_url"]
        .iter()
        .find_map(|key| as_str(raw.get(*key)).and_then(|s| normalize_thumbnail_url(&s)))
}

/// URL 规范化：去首尾空白、字面量 `\u0026` → `&`；空串/纯空白视为无缩略图。
pub(crate) fn normalize_thumbnail_url(raw: &str) -> Option<String> {
    let url = raw.trim().replace("\\u0026", "&");
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

/// 条目上可用的缩略图 URL；缺失或为空时返回可读错误。
pub(crate) fn thumbnail_url(entry: &RemoteEntry) -> Result<String> {
    entry
        .thumbnail
        .as_deref()
        .and_then(normalize_thumbnail_url)
        .ok_or_else(|| Error::other("该文件无缩略图"))
}

impl QuarkDrive {
    /// 用适配器自己的 HTTP 客户端 GET 绝对 URL；带当前会话 Cookie 与 Referer。
    pub(crate) async fn fetch_thumbnail_impl(&self, entry: &RemoteEntry) -> Result<Vec<u8>> {
        let url = thumbnail_url(entry)?;
        let parsed = url::Url::parse(&url)
            .map_err(|e| Error::other(format!("缩略图地址无效：{url}（{e}）")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(Error::other(format!("缩略图地址不是 HTTP(S)：{url}")));
        }

        let mut req = self.thumb_http.get(parsed);
        // 复用下载请求的头构造：浏览器 UA + Referer + 当前会话完整 Cookie。
        for (key, value) in self.download_headers() {
            req = req.header(key, value);
        }
        let mut resp = req.send().await.map_err(wp_net::client::net_err)?;

        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            return Err(Error::other(format!(
                "缩略图请求失败：HTTP {} {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("Unknown")
            )));
        }
        if let Some(len) = resp.content_length() {
            if len > MAX_THUMBNAIL_BYTES as u64 {
                return Err(Error::other(format!(
                    "缩略图过大：{len} 字节（上限 5 MiB）"
                )));
            }
        }

        let mut out = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(wp_net::client::net_err)? {
            if out.len() + chunk.len() > MAX_THUMBNAIL_BYTES {
                return Err(Error::other("缩略图过大：超过 5 MiB 上限"));
            }
            out.extend_from_slice(&chunk);
        }
        if out.is_empty() {
            return Err(Error::other("缩略图内容为空"));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_url_unescapes_literal_u0026_and_blank() {
        assert_eq!(
            normalize_thumbnail_url(" https://cdn.example/a.jpg?x=1\\u0026y=2 ").as_deref(),
            Some("https://cdn.example/a.jpg?x=1&y=2")
        );
        assert_eq!(normalize_thumbnail_url("").as_deref(), None);
        assert_eq!(normalize_thumbnail_url("   ").as_deref(), None);
        assert_eq!(
            normalize_thumbnail_url("https://cdn.example/a.jpg").as_deref(),
            Some("https://cdn.example/a.jpg")
        );
    }

    #[test]
    fn extract_thumbnail_from_entry_value() {
        let raw = json!({"fid":"f1","thumbnail":"https://cdn.example/a.jpg?x=1\\u0026y=2"});
        assert_eq!(
            thumbnail_from_raw(&raw).as_deref(),
            Some("https://cdn.example/a.jpg?x=1&y=2")
        );
        // 空串 / 纯空白 / null / 缺失都当作没有缩略图。
        assert_eq!(thumbnail_from_raw(&json!({"thumbnail": ""})), None);
        assert_eq!(thumbnail_from_raw(&json!({"thumbnail": "   "})), None);
        assert_eq!(thumbnail_from_raw(&json!({"thumbnail": null})), None);
        assert_eq!(thumbnail_from_raw(&json!({"fid": "f1"})), None);
        // 兼容 Python 源的兜底字段。
        assert_eq!(
            thumbnail_from_raw(&json!({"preview_url": "https://cdn.example/p.jpg"})).as_deref(),
            Some("https://cdn.example/p.jpg")
        );
    }

    #[test]
    fn missing_thumbnail_is_readable_error() {
        let e = RemoteEntry::file("a.jpg", "/a.jpg", 1);
        assert_eq!(thumbnail_url(&e).unwrap_err().to_string(), "该文件无缩略图");

        let mut blank = RemoteEntry::file("b.jpg", "/b.jpg", 1);
        blank.thumbnail = Some("   ".into());
        assert_eq!(
            thumbnail_url(&blank).unwrap_err().to_string(),
            "该文件无缩略图"
        );

        let mut ok = RemoteEntry::file("c.jpg", "/c.jpg", 1);
        ok.thumbnail = Some("https://cdn.example/c.jpg?a=1\\u0026b=2".into());
        assert_eq!(
            thumbnail_url(&ok).unwrap(),
            "https://cdn.example/c.jpg?a=1&b=2"
        );
    }
}
