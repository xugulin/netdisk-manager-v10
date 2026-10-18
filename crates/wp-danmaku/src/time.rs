//! 弹幕时间工具：B 站秒→毫秒、ASS `H:MM:SS.cc` 解析/格式化、秒数字符串格式化。

/// 秒（浮点，B 站 XML `p` 的首字段）→ 毫秒（四舍五入，负值裁剪到 0）。
pub fn seconds_to_ms(seconds: f64) -> i64 {
    if !seconds.is_finite() {
        return 0;
    }
    (seconds * 1000.0).round().max(0.0) as i64
}

/// 毫秒 → 秒数字符串（固定 3 位小数，B 站 XML 导出用）。
pub fn ms_to_seconds_string(ms: i64) -> String {
    format!("{:.3}", ms.max(0) as f64 / 1000.0)
}

/// 解析 ASS/SSA 时间戳 `H:MM:SS.cc`（也接受 `HH:MM:SS,mmm` 等宽松格式）。
pub fn parse_ass_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: i64 = parts[0].trim().parse().ok()?;
    let m: i64 = parts[1].trim().parse().ok()?;
    if h < 0 || !(0..60).contains(&m) {
        return None;
    }
    let sec_part = parts[2].trim();
    let (sec_str, frac_str) = match sec_part.find(['.', ',']) {
        Some(idx) => (&sec_part[..idx], &sec_part[idx + 1..]),
        None => (sec_part, ""),
    };
    let sec: i64 = sec_str.parse().ok()?;
    if !(0..60).contains(&sec) {
        return None;
    }
    let millis: i64 = if frac_str.is_empty() {
        0
    } else {
        if frac_str.len() > 3 || !frac_str.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let padded = if frac_str.len() >= 3 {
            frac_str[..3].to_string()
        } else {
            format!("{frac_str:0<3}")
        };
        padded.parse().ok()?
    };
    Some((h * 3600 + m * 60 + sec) * 1000 + millis)
}

/// 格式化 ASS 时间戳 `H:MM:SS.cc`（毫秒四舍五入到百分秒；负值裁剪到 0）。
pub fn format_ass_timestamp(ms: i64) -> String {
    let ms = ms.max(0);
    let rounded_cs = (ms + 5) / 10;
    let total_sec = rounded_cs / 100;
    let cs = rounded_cs % 100;
    let h = total_sec / 3600;
    let m = (total_sec % 3600) / 60;
    let s = total_sec % 60;
    format!("{h}:{m:02}:{s:02}.{cs:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_conversion() {
        assert_eq!(seconds_to_ms(1.5), 1500);
        assert_eq!(seconds_to_ms(0.001), 1);
        assert_eq!(seconds_to_ms(-2.0), 0);
        assert_eq!(seconds_to_ms(f64::NAN), 0);
        assert_eq!(ms_to_seconds_string(1500), "1.500");
        assert_eq!(ms_to_seconds_string(0), "0.000");
    }

    #[test]
    fn ass_time_parse() {
        assert_eq!(parse_ass_timestamp("0:00:01.50"), Some(1500));
        assert_eq!(parse_ass_timestamp("1:02:03.00"), Some(3_723_000));
        assert_eq!(parse_ass_timestamp("0:00:01,5"), Some(1500));
        assert_eq!(parse_ass_timestamp("bad"), None);
        assert_eq!(parse_ass_timestamp("0:99:00.00"), None);
    }

    #[test]
    fn ass_time_format_roundtrip() {
        assert_eq!(format_ass_timestamp(1500), "0:00:01.50");
        assert_eq!(format_ass_timestamp(0), "0:00:00.00");
        assert_eq!(format_ass_timestamp(-100), "0:00:00.00");
        for ms in [0_i64, 1230, 3_723_000, 86_399_990] {
            let s = format_ass_timestamp(ms);
            let back = parse_ass_timestamp(&s).unwrap();
            assert!((back - ms).abs() <= 5, "{ms} -> {s} -> {back}");
        }
    }
}
