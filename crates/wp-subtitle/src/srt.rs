//! SRT 解析/生成。
//!
//! - 解析：宽松处理（缺序号、多行文本、行尾坐标/设置、HTML 标签、ASS 样式前缀、坏行跳过）
//! - 生成：标准 SRT（序号 + `HH:MM:SS,mmm -->` + 文本），按时间排序、跳过空文本

use crate::model::{Cue, SubtitleDoc, SubtitleFormat};
use crate::text::{normalize_newlines, split_leading_override, strip_effects};
use crate::time::{format_srt_timestamp, parse_srt_timestamp};
use wp_core::Result;

/// 解析 SRT 文本为 cue 列表（坏块/坏行静默跳过）。
pub fn parse_srt(text: &str) -> Result<Vec<Cue>> {
    let normalized = normalize_newlines(text);
    let mut cues: Vec<Cue> = Vec::new();
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

/// 解析 SRT 文本为完整文档。
pub fn parse_srt_doc(text: &str) -> Result<SubtitleDoc> {
    Ok(SubtitleDoc {
        format: Some(SubtitleFormat::Srt),
        cues: parse_srt(text)?,
        ass_header: None,
    })
}

fn flush_block(block: &mut Vec<&str>, out: &mut Vec<Cue>) {
    if block.is_empty() {
        return;
    }
    let timing_idx = block.iter().position(|l| l.contains("-->"));
    let idx = match timing_idx {
        Some(i) => i,
        None => {
            block.clear();
            return;
        }
    };
    let (start, end) = match parse_timing_line(block[idx]) {
        Some(v) => v,
        None => {
            block.clear();
            return;
        }
    };
    let raw = block[idx + 1..].join("\n");
    block.clear();
    // 坏时间（end < start）丢弃
    if end < start {
        return;
    }
    let (raw_style, rest) = split_leading_override(&raw);
    let text = strip_effects(&rest).trim().to_string();
    // 整体在 0 之前的 cue 丢弃
    if end <= 0 {
        return;
    }
    // 负时间裁剪；零时长且无文本的块丢弃
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

/// 解析一行 SRT 时间行（`start --> end [坐标/设置]`）。
fn parse_timing_line(line: &str) -> Option<(i64, i64)> {
    let (left, right) = line.split_once("-->")?;
    let start = parse_srt_timestamp(left.trim())?;
    // 结束时间后可能跟 VTT 风格设置（`align:start`）或 SRT 坐标（`X1:...`），取第一个 token
    let end_token = right.split_whitespace().next()?;
    let end = parse_srt_timestamp(end_token)?;
    Some((start, end))
}

/// 生成 SRT 文本（空文本 cue 跳过；时间轴裁剪到非负）。
pub fn build_srt_cues(cues: &[Cue]) -> String {
    let mut sorted: Vec<Cue> = cues.iter().map(|c| c.normalized()).collect();
    sorted.sort_by_key(|c| (c.start_ms, c.end_ms));
    let mut out = String::with_capacity(sorted.len() * 64);
    let mut index = 0usize;
    for cue in &sorted {
        let text = cue.text.trim_end();
        if text.trim().is_empty() {
            continue;
        }
        index += 1;
        out.push_str(&index.to_string());
        out.push('\n');
        out.push_str(&format_srt_timestamp(cue.start_ms));
        out.push_str(" --> ");
        out.push_str(&format_srt_timestamp(cue.end_ms));
        out.push('\n');
        out.push_str(text);
        out.push_str("\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "1\n\
00:00:01,000 --> 00:00:04,000\n\
Hello world\n\
\n\
2\n\
00:00:05,500 --> 00:00:08,000\n\
<i>第二行</i>\n\
第二行续\n\
\n\
3\n\
00:00:09,000 --> 00:00:12,500\n\
{\\an8}顶部样式\\N换行文本\n";

    #[test]
    fn parse_basic_with_multiline_and_html() {
        let cues = parse_srt(SAMPLE).unwrap();
        assert_eq!(cues.len(), 3);
        assert_eq!(cues[0].start_ms, 1000);
        assert_eq!(cues[0].end_ms, 4000);
        assert_eq!(cues[0].text, "Hello world");
        assert_eq!(cues[1].text, "第二行\n第二行续");
        // ASS 样式前缀被保留，文本去标签、\N 还原成换行
        assert_eq!(cues[2].raw_style.as_deref(), Some("{\\an8}"));
        assert_eq!(cues[2].text, "顶部样式\n换行文本");
    }

    #[test]
    fn parse_negative_and_bad_lines() {
        let text = "1\n00:00:00,500 --> 00:00:02,000\n负数起点\n\n\
                    2\n-00:00:01,000 --> -00:00:00,500\n整体为负\n\n\
                    garbage block without timing\n\n\
                    3\n00:00:05,000 --> 00:00:04,000\n结束早于开始\n\n\
                    4\nnot-a-time --> 00:00:09,000\n坏时间\n\n\
                    5\n00:00:10,000 --> 00:00:11,000\nok\n";
        let cues = parse_srt(text).unwrap();
        let texts: Vec<&str> = cues.iter().map(|c| c.text.as_str()).collect();
        // 全负块丢弃；无时间行块/坏时间块丢弃；end<start 丢弃
        assert_eq!(texts, vec!["负数起点", "ok"]);
        assert_eq!(cues[0].start_ms, 500);
        assert_eq!(cues[1].start_ms, 10_000);
    }

    #[test]
    fn parse_coordinates_and_dot_separator() {
        let text = "1\n00:00:01.250 --> 00:00:03.750 X1:10 X2:20 Y1:5 Y2:30\n坐标\n";
        let cues = parse_srt(text).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ms, 1250);
        assert_eq!(cues[0].end_ms, 3750);
    }

    #[test]
    fn parse_empty() {
        assert!(parse_srt("").unwrap().is_empty());
        assert!(parse_srt("\n\n\n").unwrap().is_empty());
    }

    #[test]
    fn build_and_roundtrip() {
        let doc = parse_srt_doc(SAMPLE).unwrap();
        let out = build_srt_cues(&doc.cues);
        assert!(out.starts_with("1\n00:00:01,000 --> 00:00:04,000\n"));
        let back = parse_srt(&out).unwrap();
        assert_eq!(back.len(), doc.cues.len());
        for (a, b) in doc.cues.iter().zip(back.iter()) {
            assert_eq!(a.start_ms, b.start_ms);
            assert_eq!(a.end_ms, b.end_ms);
            assert_eq!(a.text, b.text);
        }
    }

    #[test]
    fn build_sorts_and_skips_empty() {
        let cues = vec![
            Cue {
                start_ms: 5000,
                end_ms: 6000,
                text: "后".into(),
                raw_style: None,
            },
            Cue {
                start_ms: 1000,
                end_ms: 2000,
                text: "前".into(),
                raw_style: None,
            },
            Cue {
                start_ms: 3000,
                end_ms: 4000,
                text: "   ".into(),
                raw_style: None,
            },
        ];
        let out = build_srt_cues(&cues);
        let back = parse_srt(&out).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].text, "前");
        assert_eq!(back[1].text, "后");
    }

    #[test]
    fn build_clamps_negative_times() {
        let cues = vec![Cue {
            start_ms: -500,
            end_ms: 1000,
            text: "x".into(),
            raw_style: None,
        }];
        let out = build_srt_cues(&cues);
        assert!(out.contains("00:00:00,000 --> 00:00:01,000"));
    }
}
