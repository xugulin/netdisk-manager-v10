//! 摘要、编码与签名工具。
//!
//! 三家网盘用到的算法：
//! * 百度：MD5（秒传/分片）、`bdstoken` 即 token 字符串（非签名）；
//! * 夸克：MD5（秒传）、SHA1（部分接口签名）；
//! * 光鸭：SHA1（埋点签名 `sha1(data_list + t + appid + 密钥)`）、OSS V1 签名
//!   `HMAC-SHA1` + Base64。

use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::Sha1;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// MD5 十六进制小写。
pub fn md5_hex(data: &[u8]) -> String {
    use md5::Digest;
    let mut h = md5::Md5::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// MD5 十六进制大写（OSS Content-MD5 之外的部分接口用大写）。
pub fn md5_hex_upper(data: &[u8]) -> String {
    md5_hex(data).to_uppercase()
}

/// SHA1 十六进制小写。
pub fn sha1_hex(data: &[u8]) -> String {
    use sha1::Digest;
    let mut h = Sha1::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// SHA256 十六进制小写。
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// HMAC-SHA1 原始字节。
pub fn hmac_sha1(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// HMAC-SHA1 → Base64（阿里云 OSS V1 签名用）。
pub fn hmac_sha1_base64(key: &[u8], data: &[u8]) -> String {
    B64.encode(hmac_sha1(key, data))
}

/// Base64 编码。
pub fn base64_encode(data: &[u8]) -> String {
    B64.encode(data)
}

/// Base64 解码（容忍换行/空白）。
pub fn base64_decode(text: &str) -> wp_core::Result<Vec<u8>> {
    let cleaned: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    B64.decode(cleaned)
        .map_err(|e| wp_core::Error::other(format!("Base64 解码失败：{e}")))
}

/// URL query 值编码（RFC 3986：空格 → `%20`，非 ASCII 按 UTF-8 百分号编码）。
///
/// 注意：不用 `form_urlencoded`（它把空格编码成 `+`），因为网盘签名/OSS
/// CanonicalResource 等场景需要 `%20`，`+` 会被当成字面加号。
pub fn url_encode(text: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
    /// 保留 RFC 3986 unreserved 字符（字母数字 + `-._~`），其余百分号编码。
    const QUERY: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    utf8_percent_encode(text, QUERY).to_string()
}

/// 对参数列表排序后拼接成 `k=v&k2=v2`（部分签名要求按 key 字典序）。
pub fn sorted_query(pairs: &[(String, String)]) -> String {
    let mut v: Vec<(String, String)> = pairs.to_vec();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v.iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// 生成随机十六进制串（设备 id / state 等）。
pub fn random_hex(len: usize) -> String {
    wp_core::util::random_hex(len)
}

/// 生成 RFC1123 时间（OSS 签名 `Date` 头）。
pub fn rfc1123_now() -> String {
    chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_known() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn sha1_known() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn sha256_known() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hmac_sha1_known() {
        // RFC 2202 test case 1: key=0x0b*20, data="Hi There"
        let key = [0x0bu8; 20];
        let mac = hmac_sha1(&key, b"Hi There");
        assert_eq!(
            mac.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "b617318655057264e28bc0b6fb378c8ef146be00"
        );
    }

    #[test]
    fn base64_roundtrip() {
        let s = "中文 abc";
        assert_eq!(
            base64_decode(&base64_encode(s.as_bytes())).unwrap(),
            s.as_bytes()
        );
    }

    #[test]
    fn url_encode_cn() {
        assert_eq!(url_encode("中文 1"), "%E4%B8%AD%E6%96%87%201");
    }
}
