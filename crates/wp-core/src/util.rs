//! 通用工具：统一逻辑路径、大小/时间/速率格式化、文件名清洗、ID 生成。

use chrono::{Local, TimeZone};
use std::time::{SystemTime, UNIX_EPOCH};

/// 当前 Unix 秒。
/// 截断字符串到指定字符数（超出补 `…`，按字符而不是字节，避免中文被截半）。
pub fn ellipsis(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars || max_chars == 0 {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 规范化统一逻辑路径：
/// * 保证以 `/` 开头；
/// * 折叠重复 `/`；
/// * 去掉末尾 `/`（根目录保持 `/`）；
/// * 处理 `.` / `..`。
pub fn norm_path(path: &str) -> String {
    let p = path.trim().replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("/{}", parts.join("/"))
}

/// 拼接目录与名称。
pub fn join_path(dir: &str, name: &str) -> String {
    let d = norm_path(dir);
    let n = name.trim_matches('/');
    if n.is_empty() {
        return d;
    }
    if d == "/" {
        format!("/{n}")
    } else {
        format!("{d}/{n}")
    }
}

/// 父目录（根目录的父目录还是根）。
pub fn parent_path(path: &str) -> String {
    let p = norm_path(path);
    if p == "/" {
        return "/".into();
    }
    match p.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => p[..i].to_string(),
    }
}

/// 取文件名（根目录返回 "/"）。
pub fn base_name(path: &str) -> String {
    let p = norm_path(path);
    if p == "/" {
        return "/".into();
    }
    p.rsplit('/').next().unwrap_or("").to_string()
}

/// 路径面包屑：[("根目录", "/"), ("电影", "/电影"), ...]
pub fn breadcrumb(path: &str) -> Vec<(String, String)> {
    let p = norm_path(path);
    let mut out = vec![("根目录".to_string(), "/".to_string())];
    let mut cur = String::new();
    for seg in p.split('/').filter(|s| !s.is_empty()) {
        cur.push('/');
        cur.push_str(seg);
        out.push((seg.to_string(), cur.clone()));
    }
    out
}

/// 判断 child 是否在 parent 之下（都应是规范路径）。
pub fn is_under(parent: &str, child: &str) -> bool {
    let p = norm_path(parent);
    let c = norm_path(child);
    if p == "/" {
        return c != "/";
    }
    c != p && c.starts_with(&format!("{p}/"))
}

/// 字节数格式化：`1.5 KB` / `3.25 MB` / `1.00 TB`。
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut i = 0usize;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[i])
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

/// Unix 秒 → 本地时间 `2026-10-18 21:30`。
pub fn format_time(ts: i64) -> String {
    match Local.timestamp_opt(ts, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        _ => "—".to_string(),
    }
}

/// 文件名用的短时间 `10-18 21:30:05`。
pub fn format_clock(ts: i64) -> String {
    match Local.timestamp_opt(ts, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%H:%M:%S").to_string(),
        _ => "--:--:--".to_string(),
    }
}

/// Unix 秒 → 文件名安全时间戳 `20261018_213005`。
pub fn format_stamp(ts: i64) -> String {
    match Local.timestamp_opt(ts, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y%m%d_%H%M%S").to_string(),
        _ => format!("{ts}"),
    }
}

/// 速率格式化：`1.2 MB/s`。
pub fn format_speed(bps: f64) -> String {
    if bps <= 0.5 {
        return "—".into();
    }
    format!("{}/s", format_size(bps as u64))
}

/// 时长格式化：`01:23:45` / `12:34`。
pub fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// 下载文件名清洗：去掉 Windows/Linux 非法字符，避免路径穿越。
pub fn sanitize_file_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        let bad = matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            || (ch as u32) < 0x20;
        if bad {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    let out = out.trim().trim_end_matches('.').to_string();
    if out.is_empty() {
        "未命名".to_string()
    } else {
        out
    }
}

/// 生成任务 id：`t-<毫秒时间戳>-<4 位随机>`。
pub fn gen_task_id() -> String {
    use rand::Rng;
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let r: u32 = rand::thread_rng().gen_range(0..10000);
    format!("t-{ms}-{r:04}")
}

/// 生成一个短随机串（设备 id / state 参数用）。
pub fn random_hex(len: usize) -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"0123456789abcdef";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

/// 从 URL 或字符串里提取最后一个路径段（用于猜文件名）。
pub fn file_name_from_url(url: &str) -> String {
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    let seg = no_query.rsplit('/').next().unwrap_or("");
    let decoded = percent_encoding_decode(seg);
    sanitize_file_name(&decoded)
}

fn percent_encoding_decode(s: &str) -> String {
    // 不引入额外依赖：手工解码 %XX（UTF-8 字节序列拼好后整体解码）
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 在列表里按名称排序（目录优先），`desc` 控制大小/时间列的升降。
pub fn sort_entries(entries: &mut [crate::model::RemoteEntry], by: SortBy, desc: bool) {
    use crate::model::RemoteEntry;
    let cmp = |a: &RemoteEntry, b: &RemoteEntry| -> std::cmp::Ordering {
        match by {
            SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortBy::Size => a.size.cmp(&b.size),
            SortBy::Modified => a.modified.unwrap_or(0).cmp(&b.modified.unwrap_or(0)),
            SortBy::Kind => a.is_dir.cmp(&b.is_dir),
        }
    };
    entries.sort_by(|a, b| {
        // 目录永远在前
        match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => {
                let o = cmp(a, b);
                if desc {
                    o.reverse()
                } else {
                    o
                }
            }
        }
    });
}

/// 文件列表排序依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    #[default]
    Name,
    Size,
    Modified,
    Kind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_normalize() {
        assert_eq!(norm_path("/"), "/");
        assert_eq!(norm_path(""), "/");
        assert_eq!(norm_path("/a/"), "/a");
        assert_eq!(norm_path("//a//b/"), "/a/b");
        assert_eq!(norm_path("/a/./b/../c"), "/a/c");
        assert_eq!(norm_path("/a/../../b"), "/b");
    }

    #[test]
    fn path_join_parent() {
        assert_eq!(join_path("/", "a"), "/a");
        assert_eq!(join_path("/a", "b"), "/a/b");
        assert_eq!(join_path("/a/", "b"), "/a/b");
        assert_eq!(parent_path("/a/b"), "/a");
        assert_eq!(parent_path("/a"), "/");
        assert_eq!(parent_path("/"), "/");
        assert_eq!(base_name("/a/b.txt"), "b.txt");
        assert_eq!(base_name("/"), "/");
    }

    #[test]
    fn breadcrumb_works() {
        let b = breadcrumb("/电影/科幻");
        assert_eq!(b.len(), 3);
        assert_eq!(b[2].1, "/电影/科幻");
    }

    #[test]
    fn size_format() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.00 KB");
        assert_eq!(format_size(1536), "1.50 KB");
        assert_eq!(format_size(8 * 1024 * 1024), "8.00 MB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024 * 1024), "2.00 TB");
    }

    #[test]
    fn sanitize_works() {
        assert_eq!(sanitize_file_name("a/b:c?.txt"), "a_b_c_.txt");
        assert_eq!(sanitize_file_name(""), "未命名");
        assert_eq!(sanitize_file_name("..."), "未命名");
    }

    #[test]
    fn url_name() {
        assert_eq!(
            file_name_from_url("https://x/y/%E7%94%B5%E5%BD%B1.mp4?token=1"),
            "电影.mp4"
        );
    }

    #[test]
    fn sorting_dirs_first() {
        use crate::model::RemoteEntry;
        let mut v = vec![
            RemoteEntry::file("b", "/b", 10),
            RemoteEntry::dir("a", "/a"),
            RemoteEntry::file("c", "/c", 30),
        ];
        sort_entries(&mut v, SortBy::Name, false);
        assert!(v[0].is_dir);
    }
}
