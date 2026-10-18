//! HTTP `Range` 头解析与归一化。
//!
//! 代理只服务**单段** Range：
//! * `bytes=a-b`（闭区间）
//! * `bytes=a-`（到文件尾）
//! * `bytes=-N`（最后 N 字节，HTTP 规范允许的 suffix range）
//!
//! 多段 Range（`bytes=0-1,5-6`）明确**不支持**：返回 [`RangeError::MultiUnsupported`]，
//! 代理层翻译成 `400 Bad Request`（并在响应体说明）。播放器（mpv/ffmpeg、VLC）实际只用单段。
//!
//! 解析与解析后求值分两步：
//! 1. [`parse_range_header`] 只做语法解析，不知道文件大小；
//! 2. [`resolve_range`] 结合文件总大小求闭区间 `(start, end)`，越界时返回
//!    [`RangeError::Unsatisfiable`]（代理层 → `416`）。

use std::fmt;

/// Range 解析/求值错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeError {
    /// 语法错误（单位不是 bytes、数字非法、起止顺序颠倒等）→ 400。
    Invalid,
    /// 多段 Range：明确不支持 → 400。
    MultiUnsupported,
    /// 语法合法但超出文件范围 → 416。
    Unsatisfiable,
}

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RangeError::Invalid => write!(f, "Range 头语法错误"),
            RangeError::MultiUnsupported => {
                write!(f, "不支持多段 Range（仅支持单段 bytes=a-b / a- / -N）")
            }
            RangeError::Unsatisfiable => write!(f, "Range 超出文件大小"),
        }
    }
}

/// 语法解析后的单段 Range。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSpec {
    /// `bytes=a-b`（两侧都写；解析时不校验 a<=b，求值时校验）
    FromTo { start: u64, end: u64 },
    /// `bytes=a-`（到文件尾）
    From(u64),
    /// `bytes=-N`（最后 N 字节）
    Suffix(u64),
}

/// 解析 `Range` 头（不含文件大小信息）。
pub fn parse_range_header(value: &str) -> Result<RangeSpec, RangeError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(RangeError::Invalid);
    }
    // 单位：`bytes=`（HTTP 规范里 unit 大小写不敏感）
    let Some((unit, rest)) = value.split_once('=') else {
        return Err(RangeError::Invalid);
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return Err(RangeError::Invalid);
    }
    let rest = rest.trim();
    if rest.contains(',') {
        return Err(RangeError::MultiUnsupported);
    }
    let Some((a, b)) = rest.split_once('-') else {
        return Err(RangeError::Invalid);
    };
    let a = a.trim();
    let b = b.trim();
    let num =
        |s: &str| -> Result<u64, RangeError> { s.parse::<u64>().map_err(|_| RangeError::Invalid) };
    match (a.is_empty(), b.is_empty()) {
        // `bytes=-N`
        (true, false) => {
            let n = num(b)?;
            if n == 0 {
                return Err(RangeError::Invalid);
            }
            Ok(RangeSpec::Suffix(n))
        }
        // `bytes=a-`
        (false, true) => Ok(RangeSpec::From(num(a)?)),
        // `bytes=a-b`
        (false, false) => Ok(RangeSpec::FromTo {
            start: num(a)?,
            end: num(b)?,
        }),
        // `bytes=-`
        (true, true) => Err(RangeError::Invalid),
    }
}

/// 结合文件总大小求闭区间 `(start, end)`（单位字节，含端点）。
///
/// * `total == 0`（空文件）时任何 Range 都不可满足；
/// * `bytes=a-b` 的 `end` 超过文件尾会被收敛到 `total-1`（RFC 9110 允许）；
/// * `bytes=a-` 且 `a >= total` 不可满足；
/// * suffix 大于文件大小时返回整个文件。
pub fn resolve_range(spec: RangeSpec, total: u64) -> Result<(u64, u64), RangeError> {
    if total == 0 {
        return Err(RangeError::Unsatisfiable);
    }
    match spec {
        RangeSpec::FromTo { start, end } => {
            if start >= total || start > end {
                return Err(RangeError::Unsatisfiable);
            }
            Ok((start, end.min(total - 1)))
        }
        RangeSpec::From(start) => {
            if start >= total {
                return Err(RangeError::Unsatisfiable);
            }
            Ok((start, total - 1))
        }
        RangeSpec::Suffix(n) => {
            if n == 0 {
                return Err(RangeError::Invalid);
            }
            let start = total.saturating_sub(n);
            Ok((start, total - 1))
        }
    }
}

/// 生成 `Content-Range` 头值：`bytes {start}-{end}/{total}`。
pub fn content_range(start: u64, end: u64, total: u64) -> String {
    format!("bytes {start}-{end}/{total}")
}

/// 生成不可满足时的 `Content-Range` 头值：`bytes */{total}`。
pub fn content_range_star(total: u64) -> String {
    format!("bytes */{total}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_from_to() {
        assert_eq!(
            parse_range_header("bytes=0-1023").unwrap(),
            RangeSpec::FromTo {
                start: 0,
                end: 1023
            }
        );
        assert_eq!(
            parse_range_header("  BYTES=100-199 ").unwrap(),
            RangeSpec::FromTo {
                start: 100,
                end: 199
            }
        );
        assert_eq!(
            parse_range_header("bytes=5 - 9").unwrap(),
            RangeSpec::FromTo { start: 5, end: 9 }
        );
    }

    #[test]
    fn parse_open_ended() {
        assert_eq!(
            parse_range_header("bytes=2048-").unwrap(),
            RangeSpec::From(2048)
        );
    }

    #[test]
    fn parse_suffix() {
        assert_eq!(
            parse_range_header("bytes=-4096").unwrap(),
            RangeSpec::Suffix(4096)
        );
        assert_eq!(parse_range_header("bytes=-0"), Err(RangeError::Invalid));
    }

    #[test]
    fn parse_multi_unsupported() {
        assert_eq!(
            parse_range_header("bytes=0-1,5-6"),
            Err(RangeError::MultiUnsupported)
        );
    }

    #[test]
    fn parse_invalid_forms() {
        for bad in [
            "",
            "bytes",
            "items=0-1",
            "bytes=",
            "bytes=-",
            "bytes=a-b",
            "bytes=1-2-3",
            "bytes=--1",
            "bytes=1--2",
        ] {
            assert_eq!(
                parse_range_header(bad),
                Err(RangeError::Invalid),
                "bad={bad:?}"
            );
        }
    }

    #[test]
    fn resolve_clamps_end_to_file_size() {
        assert_eq!(
            resolve_range(
                RangeSpec::FromTo {
                    start: 0,
                    end: 10_000
                },
                1024
            ),
            Ok((0, 1023))
        );
        assert_eq!(
            resolve_range(RangeSpec::FromTo { start: 5, end: 6 }, 1024),
            Ok((5, 6))
        );
    }

    #[test]
    fn resolve_from_and_suffix() {
        assert_eq!(resolve_range(RangeSpec::From(1000), 1024), Ok((1000, 1023)));
        assert_eq!(
            resolve_range(RangeSpec::From(1024), 1024),
            Err(RangeError::Unsatisfiable)
        );
        assert_eq!(resolve_range(RangeSpec::Suffix(100), 1024), Ok((924, 1023)));
        // suffix 比文件大 → 整个文件
        assert_eq!(
            resolve_range(RangeSpec::Suffix(10_000), 1024),
            Ok((0, 1023))
        );
        // 空文件
        assert_eq!(
            resolve_range(RangeSpec::From(0), 0),
            Err(RangeError::Unsatisfiable)
        );
    }

    #[test]
    fn resolve_rejects_bad_order() {
        assert_eq!(
            resolve_range(RangeSpec::FromTo { start: 10, end: 9 }, 1024),
            Err(RangeError::Unsatisfiable)
        );
    }

    #[test]
    fn content_range_format() {
        assert_eq!(content_range(0, 99, 1000), "bytes 0-99/1000");
        assert_eq!(content_range_star(1000), "bytes */1000");
    }
}
