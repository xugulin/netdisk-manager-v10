//! 从 ASS 字幕文件导入弹幕（把已有的“弹幕 ASS”当作本地弹幕库来源）。
//!
//! 提取：`Dialogue:` 的时间/文本/颜色（`\c&HBBGGRR&` / `\1c&H...&` 覆盖标签）、
//! `\move` → 滚动、`\pos` → 顶部（无移动标签时默认滚动）、`\fs` → 字号。
//! 文本去 override 块、`\N` 转空格。

use crate::model::{Comment, DanmakuDoc, DanmakuMode, DanmakuSource};
use crate::text::clean_danmaku_text;
use crate::time::parse_ass_timestamp;
use wp_core::Result;

/// 解析 ASS/SSA 中的 Dialogue 为弹幕。
pub fn import_ass(ass: &str) -> Result<DanmakuDoc> {
    let mut comments: Vec<Comment> = Vec::new();
    let mut section = String::new();
    for line in ass.replace("\r\n", "\n").replace('\r', "\n").lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed.to_ascii_lowercase();
            continue;
        }
        if !section.eq_ignore_ascii_case("[events]") {
            continue;
        }
        let Some(rest) = strip_prefix_ci(trimmed, "Dialogue:") else {
            continue;
        };
        if let Some(comment) = parse_dialogue(rest.trim()) {
            comments.push(comment);
        }
    }
    comments.sort_by_key(|c| c.time_ms);
    Ok(DanmakuDoc::new(DanmakuSource::LocalFile, comments))
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s.is_char_boundary(prefix.len()) {
        let head = &s[..prefix.len()];
        if head.eq_ignore_ascii_case(prefix) {
            return Some(&s[prefix.len()..]);
        }
    }
    None
}

fn parse_dialogue(rest: &str) -> Option<Comment> {
    // 标准 10 字段 + Text（splitn 保证 Text 里的逗号保留）
    let parts: Vec<&str> = rest.splitn(10, ',').collect();
    if parts.len() < 10 {
        return None;
    }
    let start_ms = parse_ass_timestamp(parts[1].trim())?;
    let raw_text = parts[9];
    let color = extract_color(raw_text).unwrap_or(0xFFFFFF);
    let font_size = extract_font_size(raw_text).unwrap_or(25.0);
    let mode = infer_mode(raw_text);
    let text = clean_danmaku_text(raw_text);
    if text.is_empty() {
        return None;
    }
    Some(Comment {
        time_ms: start_ms,
        mode,
        color,
        font_size,
        text,
        user: None,
    })
}

/// 从 override 标签提取颜色：`\c&HBBGGRR&` 或 `\1c&HBBGGRR&` → `0xRRGGBB`。
fn extract_color(raw: &str) -> Option<u32> {
    for tag in ["\\1c&H", "\\c&H"] {
        if let Some(idx) = raw.find(tag) {
            let after = &raw[idx + tag.len()..];
            let hex: String = after
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            if hex.len() == 6 {
                let b = u32::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u32::from_str_radix(&hex[2..4], 16).ok()?;
                let r = u32::from_str_radix(&hex[4..6], 16).ok()?;
                return Some((r << 16) | (g << 8) | b);
            }
        }
    }
    None
}

/// 从 `\fs<px>` 提取字号（以 1080p 为基准；ASS 弹幕轨通常按 1080p 生成）。
///
/// 跳过 `\fscx`/`\fscy`/`\fsp` 等同样以 `\fs` 开头的标签。
fn extract_font_size(raw: &str) -> Option<f32> {
    let mut search_from = 0usize;
    while let Some(rel) = raw[search_from..].find("\\fs") {
        let idx = search_from + rel;
        let after = &raw[idx + 3..];
        let num: String = after
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if let Ok(value) = num.parse::<f32>() {
            if value > 0.0 {
                return Some(value);
            }
        }
        search_from = idx + 3;
    }
    None
}

/// 推断模式：`\move` → 滚动；`\pos` → 顶部（弹幕 ASS 的固定弹幕通常在顶部）。
fn infer_mode(raw: &str) -> DanmakuMode {
    if raw.contains("\\move") {
        DanmakuMode::Scroll
    } else if raw.contains("\\pos") {
        DanmakuMode::Top
    } else {
        DanmakuMode::Scroll
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "[Script Info]\nScriptType: v4.00+\n\n\
[Events]\n\
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
Dialogue: 0,0:00:01.50,0:00:09.50,Danmaku,,0,0,0,,{\\move(1920,50,-200,50)\\c&H0000FF&}红色滚动\n\
Dialogue: 0,0:00:03.00,0:00:07.00,Danmaku,,0,0,0,,{\\pos(960,100)\\c&HFF0000&\\fs30}蓝色顶部\n\
Dialogue: 0,0:00:05.00,0:00:13.00,Danmaku,,0,0,0,,{\\an8\\move(2000,300,-200,300)}普通\\N换行\n\
Comment: 0,0:00:06.00,0:00:07.00,Danmaku,,0,0,0,,注释不算\n";

    #[test]
    fn import_basic() {
        let doc = import_ass(SAMPLE).unwrap();
        assert_eq!(doc.comments.len(), 3);
        assert_eq!(doc.comments[0].time_ms, 1500);
        assert_eq!(doc.comments[0].mode, DanmakuMode::Scroll);
        // &H0000FF = BGR：蓝
        assert_eq!(doc.comments[0].color, 0xFF0000);
        assert_eq!(doc.comments[0].text, "红色滚动");
        assert_eq!(doc.comments[1].mode, DanmakuMode::Top);
        assert_eq!(doc.comments[1].color, 0x0000FF);
        assert_eq!(doc.comments[1].font_size, 30.0);
        assert_eq!(doc.comments[2].text, "普通 换行");
    }

    #[test]
    fn import_empty_and_no_events() {
        assert!(import_ass("").unwrap().is_empty());
        assert!(import_ass("[Script Info]\nScriptType: v4.00+\n")
            .unwrap()
            .is_empty());
        let only_comments = "[Events]\nComment: 0,0:00:01.00,0:00:02.00,S,,0,0,0,,x\n";
        assert!(import_ass(only_comments).unwrap().is_empty());
    }

    #[test]
    fn import_skips_bad_times() {
        let ass = "[Events]\nDialogue: 0,bad,0:00:02.00,S,,0,0,0,,x\n\
Dialogue: 0,0:00:01.00,0:00:02.00,S,,0,0,0,,ok\n";
        let doc = import_ass(ass).unwrap();
        assert_eq!(doc.comments.len(), 1);
        assert_eq!(doc.comments[0].time_ms, 1000);
    }
}
