//! 时间戳解析/格式化：SRT（`,` 毫秒）、VTT/ASS（`.` 百分秒）互通。
//!
//! 统一内部表示是「毫秒，整数」；所有解析函数返回 `Option<i64>`（宽松，允许前导负号，
//! 便于解析异常样本后再由调用方裁剪）。格式化函数对负值输出 `-HH:MM:SS,mmm`
//! （SRT 规范不支持负时间，导出前请用 [`crate::Cue::normalized`]）。

/// 解析通用时间戳（SRT / VTT / ASS / SSA 都吃）。
///
/// 支持：
/// - `HH:MM:SS,mmm` / `HH:MM:SS.mmm`（毫秒 1~6 位，按 3 位归一）
/// - `H:MM:SS.cc`（ASS/SSA 百分秒）
/// - `MM:SS.mmm`（WebVTT 允许省略小时）
/// - 前导 `-`（返回负毫秒）、前后空白
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (negative, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest.trim()),
        None => (false, s),
    };
    if body.is_empty() {
        return None;
    }
    let parts: Vec<&str> = body.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let (hours, minutes, sec_part) = match parts.len() {
        2 => ("0", parts[0], parts[1]),
        _ => (parts[0], parts[1], parts[2]),
    };
    let h: i64 = hours.trim().parse().ok()?;
    let m: i64 = minutes.trim().parse().ok()?;
    if h < 0 || !(0..60).contains(&m) {
        return None;
    }
    let (sec_str, frac_str) = match sec_part.find(['.', ',']) {
        Some(idx) => (&sec_part[..idx], &sec_part[idx + 1..]),
        None => (sec_part, ""),
    };
    let sec_str = sec_str.trim();
    if sec_str.is_empty() {
        return None;
    }
    let sec: i64 = sec_str.parse().ok()?;
    if !(0..60).contains(&sec) {
        return None;
    }
    let millis: i64 = if frac_str.trim().is_empty() {
        0
    } else {
        let frac = frac_str.trim();
        if frac.len() > 6 || !frac.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        // 1 位 = 百毫秒，2 位 = 十毫秒，3 位 = 毫秒，>3 位截断到毫秒
        let padded = if frac.len() >= 3 {
            frac[..3].to_string()
        } else {
            format!("{frac:0<3}")
        };
        padded.parse().ok()?
    };
    let total_ms = ((h * 3600 + m * 60 + sec) * 1000 + millis) as i64;
    Some(if negative { -total_ms } else { total_ms })
}

/// 解析 SRT 时间戳（与通用解析一致；保留独立函数便于语义化调用）。
pub fn parse_srt_timestamp(s: &str) -> Option<i64> {
    parse_timestamp(s)
}

/// 解析 ASS/SSA 时间戳（`H:MM:SS.cc`）。
pub fn parse_ass_timestamp(s: &str) -> Option<i64> {
    parse_timestamp(s)
}

fn format_clock(ms: i64, comma: bool) -> String {
    let negative = ms < 0;
    let ms = ms.abs();
    let total_sec = ms / 1000;
    let millis = ms % 1000;
    let h = total_sec / 3600;
    let m = (total_sec % 3600) / 60;
    let s = total_sec % 60;
    let sep = if comma { ',' } else { '.' };
    format!(
        "{}{:02}:{:02}:{:02}{}{:03}",
        if negative { "-" } else { "" },
        h,
        m,
        s,
        sep,
        millis
    )
}

/// 格式化为 SRT 时间戳 `HH:MM:SS,mmm`（负值带 `-` 前缀）。
pub fn format_srt_timestamp(ms: i64) -> String {
    format_clock(ms, true)
}

/// 格式化为 WebVTT 时间戳 `HH:MM:SS.mmm`（负值带 `-` 前缀）。
pub fn format_vtt_timestamp(ms: i64) -> String {
    format_clock(ms, false)
}

/// 格式化为 ASS 时间戳 `H:MM:SS.cc`（毫秒四舍五入到百分秒）。
pub fn format_ass_timestamp(ms: i64) -> String {
    let negative = ms < 0;
    let rounded_cs = (ms.abs() + 5) / 10; // 四舍五入到 centisecond
    let total_sec = rounded_cs / 100;
    let cs = rounded_cs % 100;
    let h = total_sec / 3600;
    let m = (total_sec % 3600) / 60;
    let s = total_sec % 60;
    format!(
        "{}{}:{:02}:{:02}.{:02}",
        if negative { "-" } else { "" },
        h,
        m,
        s,
        cs
    )
}

/// 把毫秒裁剪到 `[0, i64::MAX]`。
pub fn clamp_ms(ms: i64) -> i64 {
    ms.max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_srt_basic_and_comma() {
        assert_eq!(parse_srt_timestamp("00:00:01,500"), Some(1500));
        assert_eq!(parse_srt_timestamp("00:00:01.500"), Some(1500));
        assert_eq!(parse_srt_timestamp("01:02:03,004"), Some(3_723_004));
        assert_eq!(parse_srt_timestamp(" 00:00:02,5 "), Some(2500));
        assert_eq!(parse_srt_timestamp("00:00:02,50"), Some(2500));
        assert_eq!(parse_srt_timestamp("00:02,250"), Some(2250));
    }

    #[test]
    fn parse_clock_boundaries_and_bad() {
        assert_eq!(parse_timestamp("12:34:56,789"), Some(45_296_789));
        assert_eq!(parse_timestamp("100:00:00,000"), Some(360_000_000));
        assert_eq!(parse_timestamp("-00:00:01,000"), Some(-1000));
        assert_eq!(parse_timestamp(""), None);
        assert_eq!(parse_timestamp("abc"), None);
        assert_eq!(parse_timestamp("00:99:00,000"), None);
        assert_eq!(parse_timestamp("00:00:99,000"), None);
        // 省略小时（MM:SS）宽松接受
        assert_eq!(parse_timestamp("1:2,500"), Some(62_500));
        assert_eq!(parse_timestamp("00:00:01,12a"), None);
    }

    #[test]
    fn format_srt_roundtrip() {
        for ms in [0_i64, 1000, 3_723_004, 45_296_789] {
            let s = format_srt_timestamp(ms);
            assert_eq!(parse_srt_timestamp(&s), Some(ms), "srt format {s}");
        }
        assert_eq!(format_srt_timestamp(1500), "00:00:01,500");
        assert_eq!(format_srt_timestamp(-1500), "-00:00:01,500");
    }

    #[test]
    fn format_ass_roundtrip() {
        assert_eq!(format_ass_timestamp(0), "0:00:00.00");
        assert_eq!(format_ass_timestamp(1500), "0:00:01.50");
        assert_eq!(format_ass_timestamp(3_723_004), "1:02:03.00");
        assert_eq!(format_ass_timestamp(45_296_789), "12:34:56.79");
        // 毫秒 -> 百分秒四舍五入后能被解析回来（误差 <= 5ms）
        for ms in [0_i64, 1230, 45_296_789, 86_399_999] {
            let s = format_ass_timestamp(ms);
            let back = parse_ass_timestamp(&s).unwrap();
            assert!((back - ms).abs() <= 5, "{ms} -> {s} -> {back}");
        }
        assert_eq!(format_ass_timestamp(-1000), "-0:00:01.00");
    }

    #[test]
    fn clamp_and_convert() {
        assert_eq!(clamp_ms(-5), 0);
        // SRT -> ASS 输出
        let ms = parse_srt_timestamp("00:00:05,500").unwrap();
        assert_eq!(format_ass_timestamp(ms), "0:00:05.50");
    }
}
