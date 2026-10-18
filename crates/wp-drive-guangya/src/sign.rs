//! 签名：阿里云 OSS V1（STS）签名 + 埋点签名。
//!
//! 两部分都来自 Python 源码，**拼接顺序不能改**：
//!
//! OSS V1：
//! ```text
//! StringToSign = HTTP-Verb + "\n"
//!              + Content-MD5 + "\n"
//!              + Content-Type + "\n"
//!              + Date + "\n"
//!              + CanonicalizedOSSHeaders
//!              + CanonicalizedResource
//! ```
//! 其中 `Date` 行填 `x-oss-date` 的值（项目手工签名器的约定），
//! `CanonicalizedOSSHeaders` 只含 `x-oss-*` 头、键小写、字典序、值 trim、每行 `\n` 结尾。
//!
//! 埋点：`sha1(data_list + t + appid + 密钥)` 的小写 hex，无分隔符。

use std::collections::BTreeMap;
use std::path::Path;

/// OSS 子资源（决定 CanonicalizedResource 的 query 部分）。
#[derive(Debug, Clone, Copy)]
pub enum OssSubResource<'a> {
    /// 普通 PUT / GET
    None,
    /// `POST ?uploads`（InitiateMultipartUpload）
    Uploads,
    /// `PUT ?partNumber=N&uploadId=ID`（UploadPart）
    Part { number: u32, upload_id: &'a str },
    /// `POST ?uploadId=ID`（Complete/AbortMultipartUpload）
    UploadId { upload_id: &'a str },
}

/// OSS V1 签名输入。
pub struct OssSignRequest<'a> {
    pub method: &'a str,
    pub content_md5: &'a str,
    pub content_type: &'a str,
    /// 填 `x-oss-date` 的值（本 crate 不发 `Date` 头）。
    pub date: &'a str,
    /// 实际发送且以 `x-oss-` 开头的头（键大小写不敏感）。
    pub oss_headers: &'a BTreeMap<String, String>,
    pub bucket: &'a str,
    pub object_path: &'a str,
    pub sub_resource: OssSubResource<'a>,
    pub access_key_id: &'a str,
    pub access_key_secret: &'a str,
}

/// CanonicalizedResource：`/{bucket}/{object}` 加上排序后的子资源。
pub fn canonical_resource(bucket: &str, object_path: &str, sub: OssSubResource<'_>) -> String {
    let base = format!("/{}/{}", bucket, object_path.trim_start_matches('/'));
    match sub {
        OssSubResource::None => base,
        OssSubResource::Uploads => format!("{base}?uploads"),
        OssSubResource::Part { number, upload_id } => {
            format!("{base}?partNumber={number}&uploadId={upload_id}")
        }
        OssSubResource::UploadId { upload_id } => format!("{base}?uploadId={upload_id}"),
    }
}

/// CanonicalizedOSSHeaders：所有 `x-oss-*` 头，键小写、字典序、值 trim、`k:v\n`。
pub fn canonical_oss_headers(headers: &BTreeMap<String, String>) -> String {
    let mut pairs: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| k.to_ascii_lowercase().starts_with("x-oss-"))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs.iter().map(|(k, v)| format!("{k}:{v}\n")).collect()
}

/// 完整 StringToSign（暴露出来便于单测/日志排查，生产不要打印 SK）。
pub fn string_to_sign(req: &OssSignRequest<'_>) -> String {
    let canonical_headers = canonical_oss_headers(req.oss_headers);
    let resource = canonical_resource(req.bucket, req.object_path, req.sub_resource);
    format!(
        "{}\n{}\n{}\n{}\n{}{}",
        req.method, req.content_md5, req.content_type, req.date, canonical_headers, resource
    )
}

/// 返回 `Authorization` 头的值：`OSS <AccessKeyId>:<base64(HMAC-SHA1(SK, StringToSign))>`。
pub fn oss_v1_authorization(req: &OssSignRequest<'_>) -> String {
    let sts = string_to_sign(req);
    let sig = wp_net::crypto::hmac_sha1_base64(req.access_key_secret.as_bytes(), sts.as_bytes());
    format!("OSS {}:{}", req.access_key_id, sig)
}

/// 埋点签名：`sha1(data_list + t + appid + secret)` 小写 hex，字符串直接拼接。
#[allow(dead_code)]
pub fn telemetry_sign(data_list: &str, t: i64, appid: i64, secret: &str) -> String {
    let raw = format!("{data_list}{t}{appid}{secret}");
    wp_net::crypto::sha1_hex(raw.as_bytes())
}

/// 埋点 `application/x-www-form-urlencoded` 表单（顺序与 Python 源码一致）。
#[allow(dead_code)]
pub fn telemetry_form(data_list: &str, t: i64, appid: i64, secret: &str) -> Vec<(String, String)> {
    vec![
        ("data_list".into(), data_list.into()),
        ("gzip".into(), "1".into()),
        ("appid".into(), appid.to_string()),
        ("t".into(), t.to_string()),
        ("sign".into(), telemetry_sign(data_list, t, appid, secret)),
    ]
}

/// 按扩展名推断 MIME（与 Python `mimetypes.guess_type` 的常见值对齐）。
///
/// 只用于 OSS 的 `Content-Type`，未知类型回退 `application/octet-stream`。
pub fn content_type_for(file_name: &str) -> &'static str {
    let ext = Path::new(file_name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "flv" => "video/x-flv",
        "ts" => "video/mp2t",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" => "audio/ogg",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "heic" => "image/heic",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "text/javascript",
        "json" => "application/json",
        "toml" => "application/toml",
        "xml" => "application/xml",
        "csv" => "text/csv",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "rar" => "application/vnd.rar",
        "7z" => "application/x-7z-compressed",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "apk" => "application/vnd.android.package-archive",
        "torrent" => "application/x-bittorrent",
        "iso" => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

/// OSS 返回的 ETag 去引号（业务 `gcid` 需要）。
pub fn strip_etag_quotes(etag: &str) -> String {
    etag.trim().trim_matches('"').to_string()
}

/// 用固定输入验证签名算法的已知向量（由同算法独立计算，见测试注释）。
#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn headers() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("x-oss-date".into(), "Mon, 18 Oct 2026 12:00:00 GMT".into());
        m.insert("x-oss-security-token".into(), "STS.test-token".into());
        m
    }

    #[test]
    fn oss_v1_signature_matches_known_vector() {
        let req = OssSignRequest {
            method: "PUT",
            content_md5: "",
            content_type: "application/octet-stream",
            date: "Mon, 18 Oct 2026 12:00:00 GMT",
            oss_headers: &headers(),
            bucket: "gy-test-bucket",
            object_path: "uploads/2026/10/file.bin",
            sub_resource: OssSubResource::None,
            access_key_id: "AKIDEXAMPLE",
            access_key_secret: "SECRETKEYEXAMPLE",
        };
        let expected_sts = concat!(
            "PUT\n",
            "\n",
            "application/octet-stream\n",
            "Mon, 18 Oct 2026 12:00:00 GMT\n",
            "x-oss-date:Mon, 18 Oct 2026 12:00:00 GMT\n",
            "x-oss-security-token:STS.test-token\n",
            "/gy-test-bucket/uploads/2026/10/file.bin"
        );
        assert_eq!(string_to_sign(&req), expected_sts);
        assert_eq!(
            oss_v1_authorization(&req),
            "OSS AKIDEXAMPLE:s9G6no3B6xwPDwFXiLXO4HqPgrc="
        );
    }

    #[test]
    fn canonical_headers_sorted_and_trimmed() {
        let mut m = BTreeMap::new();
        m.insert("X-OSS-Security-Token".into(), "  tok  ".into());
        m.insert("x-oss-date".into(), " date ".into());
        m.insert("content-type".into(), "ignored".into());
        let canon = canonical_oss_headers(&m);
        assert_eq!(canon, "x-oss-date:date\nx-oss-security-token:tok\n");
    }

    #[test]
    fn multipart_canonical_resources() {
        assert_eq!(
            canonical_resource("b", "a/b/c", OssSubResource::Uploads),
            "/b/a/b/c?uploads"
        );
        assert_eq!(
            canonical_resource(
                "b",
                "/a/b/c",
                OssSubResource::Part {
                    number: 2,
                    upload_id: "abcDEF123"
                }
            ),
            "/b/a/b/c?partNumber=2&uploadId=abcDEF123"
        );
        assert_eq!(
            canonical_resource("b", "a", OssSubResource::UploadId { upload_id: "u1" }),
            "/b/a?uploadId=u1"
        );
    }

    #[test]
    fn telemetry_signature_known_vector() {
        // sha1("H4sI...AAAD//wMA" + "1700000000000" + "1" + "")
        let data_list = "H4sIAAAAAAAA/6tWKkktLlGyUkpKLElNLQYAAAD//wMA";
        assert_eq!(
            telemetry_sign(data_list, 1_700_000_000_000, 1, ""),
            "11af03d49f116f11a87c868f4f3073f6540a494c"
        );
        let form = telemetry_form(data_list, 1_700_000_000_000, 1, "");
        assert_eq!(form[1], ("gzip".to_string(), "1".to_string()));
        assert_eq!(form[3].0, "t");
        assert_eq!(form[4].1, "11af03d49f116f11a87c868f4f3073f6540a494c");
    }

    #[test]
    fn content_types_and_etag() {
        assert_eq!(content_type_for("a.MP4"), "video/mp4");
        assert_eq!(content_type_for("b.toml"), "application/toml");
        assert_eq!(content_type_for("noext"), "application/octet-stream");
        assert_eq!(strip_etag_quotes("\"ABC123\""), "ABC123");
        assert_eq!(strip_etag_quotes("ABC123"), "ABC123");
    }

    #[test]
    fn hmac_sha1_helper_vector() {
        // RFC 2202 test case 2：key="Jefe", data="what do ya want for nothing?"
        assert_eq!(
            wp_net::crypto::hmac_sha1_base64(b"Jefe", b"what do ya want for nothing?"),
            "7/zfauXrL6LSdBbV8YTfnCWafHk="
        );
        // base64 解码再编码，确认 crate 的编码器是标准字母表
        assert_eq!(
            base64::engine::general_purpose::STANDARD.encode(b"abc"),
            "YWJj"
        );
    }
}
