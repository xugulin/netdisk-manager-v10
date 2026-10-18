//! WebVTT 解析/生成。
//!
//! 解析：`WEBVTT` 头、`NOTE`/`STYLE`/`REGION` 块跳过、cue 标识行、cue settings 忽略、
//! HTML/VTT 标签清洗（`<v Speaker>`/`<c.class>`/`<b>` 等）、短格式 `MM:SS.mmm`。
//! 生成：标准 WebVTT（`HH:MM:SS.mmm --> HH:MM:SS.mmm`）。

use crate::model::{Cue, SubtitleDoc, SubtitleFormat};
use crate::text::{normalize_newlines, split_leading_override, strip_effects};
use crate::time::{format_vtt_timestamp, parse_timestamp};
use wp_core::Result;

/// 解析 WebVTT 文本（坏块静默跳过）。
pub fn parse_vtt(text: &str) -> Result<Vec<Cue>> {
    let normalized = normalize_newlines(text);
    let mut cues = Vec::new();
    let mut block: Vec<&str> = Vec::new();
    for line in normalized.lines() {
        if line.trim().is_empty() {
            flush_block(&mut block, &mut cues);
        } else {
            block.push(line);
        }
    }
    flush_block(&mut block, &mut cues);
    Ok(cues)
}

/// 解析 WebVTT 文本为完整文档。
pub fn parse_vtt_doc(text: &str) -> Result<SubtitleDoc> {
    Ok(SubtitleDoc {
        format: Some(SubtitleFormat::Vtt),
        cues: parse_vtt(text)?,
        ass_header: None,
    })
}

fn flush_block(block: &mut Vec<&str>, out: &mut Vec<Cue>) {
    if block.is_empty() {
        return;
    }
    // 纯元数据块：WEBVTT 头 / NOTE / STYLE / REGION
    let first = block[0].trim();
    let is_metadata = first.eq_ignore_ascii_case("WEBVTT")
        || first.starts_with("WEBVTT ")
        || first.starts_with("NOTE")
        || first.starts_with("STYLE")
        || first.starts_with("REGION");
    if is_metadata {
        block.clear();
        return;
    }
    let timing_idx = match block.iter().position(|l| l.contains("-->")) {
        Some(i) => i,
        None => {
            block.clear();
            return;
        }
    };
    let (start, end) = match parse_timing_line(block[timing_idx]) {
        Some(v) => v,
        None => {
            block.clear();
            return;
        }
    };
    let raw = block[timing_idx + 1..].join("\n");
    block.clear();
    if end < start {
        return;
    }
    let (raw_style, rest) = split_leading_override(&raw);
    let text = strip_effects(&rest).trim().to_string();
    // 整体在 0 之前的 cue 丢弃
    if end <= 0 {
        return;
    }
    let start = start.max(0);
    let end = end.max(start);
    if text.is_empty() && end == start {
        return;
    }
    out.push(Cue {
        start_ms: start,
        end_ms: end,
        text,
        raw_style,
    });
}

fn parse_timing_line(line: &str) -> Option<(i64, i64)> {
    let (left, right) = line.split_once("-->")?;
    let start = parse_timestamp(left.trim())?;
    // 结束时间后面可能跟 cue settings（`line:0 position:10% align:start`）
    let end_token = right.split_whitespace().next()?;
    let end = parse_timestamp(end_token)?;
    Some((start, end))
}

/// 生成 WebVTT 文本（空文本 cue 跳过；时间轴裁剪到非负）。
pub fn build_vtt_cues(cues: &[Cue]) -> String {
    let mut sorted: Vec<Cue> = cues.iter().map(|c| c.normalized()).collect();
    sorted.sort_by_key(|c| (c.start_ms, c.end_ms));
    let mut out = String::from("WEBVTT\n\n");
    for cue in &sorted {
        let text = cue.text.trim_end();
        if text.trim().is_empty() {
            continue;
        }
        out.push_str(&format_vtt_timestamp(cue.start_ms));
        out.push_str(" --> ");
        out.push_str(&format_vtt_timestamp(cue.end_ms));
        out.push('\n');
        out.push_str(text);
        out.push_str("\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "WEBVTT\n\
\n\
NOTE 这是注释块\n\
随便写点\n\
\n\
cue-1\n\
00:00:01.000 --> 00:00:03.500 line:0 position:10% align:start\n\
<v Speaker>你好</v>\n\
\n\
00:04.000 --> 00:06.000\n\
第二段 <b>加粗</b>\n";

    #[test]
    fn parse_vtt_basic() {
        let cues = parse_vtt(SAMPLE).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "你好");
        assert_eq!(cues[1].text, "第二段 加粗");
        assert_eq!(cues[0].start_ms, 1000);
        assert_eq!(cues[0].end_ms, 3500);
        // 短格式 MM:SS.mmm
        assert_eq!(cues[1].start_ms, 4000);
    }

    #[test]
    fn parse_vtt_entities_and_empty() {
        let text = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nA &amp; B\n";
        let cues = parse_vtt(text).unwrap();
        assert_eq!(cues[0].text, "A & B");
        assert!(parse_vtt("WEBVTT\n").unwrap().is_empty());
        assert!(parse_vtt("").unwrap().is_empty());
    }

    #[test]
    fn parse_vtt_skips_style_region_blocks() {
        let text = "WEBVTT\n\nSTYLE\n::cue { color: red }\n\nREGION\nid:r1\n\n\
                    00:00:01.000 --> 00:00:02.000\ntext\n";
        let cues = parse_vtt(text).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "text");
    }

    #[test]
    fn build_and_roundtrip() {
        let doc = parse_vtt_doc(SAMPLE).unwrap();
        let out = build_vtt_cues(&doc.cues);
        assert!(out.starts_with("WEBVTT\n"));
        assert!(out.contains("00:00:01.000 --> 00:00:03.500"));
        let back = parse_vtt(&out).unwrap();
        assert_eq!(back.len(), doc.cues.len());
        for (a, b) in doc.cues.iter().zip(back.iter()) {
            assert_eq!((a.start_ms, a.end_ms), (b.start_ms, b.end_ms));
            assert_eq!(a.text, b.text);
        }
    }
}
