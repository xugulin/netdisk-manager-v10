//! 缩略图：用条目里的绝对 CDN URL 取原始图片字节。
//!
//! 光鸭的登录态是 Bearer access token（不是 Cookie，Python 源也没有 Cookie
//! 读取逻辑），所以这里复用 [`GuangyaDrive::bearer`] 的令牌；API 客户端本身
//! 开启了 cookie_store，服务端若下发 Cookie 也会自动带上。请求带 Web 端
//! UA / Referer，20s 超时，响应上限 5 MB。
//!
//! 列表字段 `thumbnail` 的提取（含字面量 `\u0026` 还原）在 `files.rs`；
//! 这里再做一次还原，保证调用方手工构造的条目也能下载。

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use std::time::Duration;
use url::Url;
use wp_core::model::RemoteEntry;
use wp_core::{Error, Result};

use crate::{snippet, GuangyaDrive, API_UA};

/// 单张缩略图的请求超时。
const THUMBNAIL_TIMEOUT: Duration = Duration::from_secs(20);
/// 单张缩略图最大字节数（5 MB）。
pub(crate) const MAX_THUMBNAIL_BYTES: u64 = 5 * 1024 * 1024;
/// Web 端 Referer（与业务 API 保持一致）。
const REFERER_URL: &str = "https://www.guangyapan.com/";

/// 下载缩略图原始字节。
pub(crate) async fn fetch(drive: &GuangyaDrive, entry: &RemoteEntry) -> Result<Vec<u8>> {
    let url = thumbnail_url(entry)?;
    // 未登录/令牌过期会在这里返回可读的 Auth 错误。
    let token = drive.bearer().await?;
    let headers = request_headers(&token)?;

    let resp = drive
        .api
        .get(&url)
        .headers(headers)
        .timeout(THUMBNAIL_TIMEOUT)
        .send()
        .await
        .map_err(wp_net::client::net_err)?;

    let status = resp.status().as_u16();
    if status != 200 {
        let body = resp.text().await.unwrap_or_default();
        let message = format!("取缩略图失败：HTTP {status}：{}", snippet(body.trim(), 200));
        return Err(match status {
            401 | 403 => Error::auth(message),
            404 => Error::not_found(message),
            _ => Error::server(status as i64, message),
        });
    }

    if let Some(len) = resp.content_length() {
        check_size(len)?;
    }

    let mut resp = resp;
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(wp_net::client::net_err)? {
        check_size(out.len() as u64 + chunk.len() as u64)?;
        out.extend_from_slice(&chunk);
    }
    if out.is_empty() {
        return Err(Error::server(0, "缩略图响应为空（0 字节）"));
    }
    Ok(out)
}

/// 取并校验条目的缩略图 URL；缺失/空白 → “该文件无缩略图”。
fn thumbnail_url(entry: &RemoteEntry) -> Result<String> {
    let raw = entry
        .thumbnail
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::unsupported("该文件无缩略图"))?;
    // 防御性兜底：正常条目已在 files.rs 还原过。
    let url = raw.replace("\\u0026", "&");
    let parsed =
        Url::parse(&url).map_err(|e| Error::other(format!("缩略图 URL 非法：{e}（{url}）")))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(Error::other(format!(
                "缩略图 URL 必须是 http/https 绝对地址（当前协议：{other}）"
            )))
        }
    }
    if parsed.host_str().unwrap_or("").is_empty() {
        return Err(Error::other(format!("缩略图 URL 缺少主机名：{url}")));
    }
    Ok(url)
}

/// 缩略图请求头：Web UA + Referer + 当前会话的 Bearer 令牌。
fn request_headers(token: &str) -> Result<HeaderMap> {
    let mut h = crate::headers(&[
        (
            "accept",
            "image/avif,image/webp,image/apng,image/*,*/*;q=0.8",
        ),
        ("referer", REFERER_URL),
        ("user-agent", API_UA),
    ])?;
    h.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| Error::other(format!("Authorization 头非法：{e}")))?,
    );
    Ok(h)
}

/// 超过 5 MB 直接拒绝（响应头与流式累计共用）。
fn check_size(len: u64) -> Result<()> {
    if len > MAX_THUMBNAIL_BYTES {
        return Err(Error::other(format!(
            "缩略图过大：{len} 字节，超过上限 {} MB",
            MAX_THUMBNAIL_BYTES / 1024 / 1024
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{ACCEPT, REFERER, USER_AGENT};

    #[test]
    fn missing_thumbnail_is_readable() {
        let entry = RemoteEntry::file("a.jpg", "/a.jpg", 1);
        let err = thumbnail_url(&entry).unwrap_err();
        assert!(
            err.to_string().contains("该文件无缩略图"),
            "错误信息应可读：{err}"
        );

        let entry = entry.with_thumbnail(Some("  \t ".into()));
        assert!(thumbnail_url(&entry)
            .unwrap_err()
            .to_string()
            .contains("该文件无缩略图"));
    }

    #[test]
    fn url_must_be_absolute_http() {
        let entry = RemoteEntry::file("a.jpg", "/a.jpg", 1)
            .with_thumbnail(Some("https://cdn.example.com/t/a.jpg?x=1".into()));
        assert_eq!(
            thumbnail_url(&entry).unwrap(),
            "https://cdn.example.com/t/a.jpg?x=1"
        );

        // 字面量 \u0026 即使绕过列表映射也要还原。
        let entry =
            entry.with_thumbnail(Some("https://cdn.example.com/t/a.jpg?x=1\\u0026y=2".into()));
        assert_eq!(
            thumbnail_url(&entry).unwrap(),
            "https://cdn.example.com/t/a.jpg?x=1&y=2"
        );

        let entry = entry.with_thumbnail(Some("/relative/a.jpg".into()));
        assert!(thumbnail_url(&entry).is_err());
        let entry = entry.with_thumbnail(Some("ftp://cdn.example.com/a.jpg".into()));
        let err = thumbnail_url(&entry).unwrap_err().to_string();
        assert!(err.contains("http/https"), "{err}");
    }

    #[test]
    fn headers_carry_ua_referer_and_bearer() {
        let h = request_headers("token-abc").unwrap();
        assert!(h
            .get(USER_AGENT)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Mozilla/5.0"));
        assert_eq!(h.get(REFERER).unwrap().to_str().unwrap(), REFERER_URL);
        assert_eq!(
            h.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer token-abc"
        );
        assert!(h.get(ACCEPT).is_some());
    }

    #[test]
    fn size_limit_is_5mb() {
        assert!(check_size(0).is_ok());
        assert!(check_size(MAX_THUMBNAIL_BYTES).is_ok());
        let err = check_size(MAX_THUMBNAIL_BYTES + 1).unwrap_err().to_string();
        assert!(err.contains("超过上限 5 MB"), "{err}");
    }
}
