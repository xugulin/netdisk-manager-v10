//! 字幕子系统（Rust 侧）：解析/生成/AI 双语 ASS 注入。
//!
//! 渲染由 mpv/libass 负责；本 crate 负责「我们可控」的部分：
//! - 解析 SRT/VTT/ASS/SSA → 结构化 cue（AI 翻译/搜索/导出用）
//! - 生成 SRT/VTT/ASS；生成双语 ASS（保留原样式/时间轴）→ 写临时文件 → mpv
//!   `sub-add`/`sub-reload`（T7 / T11）
//! - 时间轴偏移、格式猜测、去特效文本、cue 统计/合并/追踪（T8 的数据层）
//!
//! 纯逻辑，不依赖 GUI/mpv，可全量单测。
//!
//! # 快速上手
//! ```
//! use wp_subtitle::{parse, SubtitleFormat, build_srt, build_bilingual_ass, AssOptions, BilingualLayout};
//!
//! let doc = parse(SubtitleFormat::Srt, "1\n00:00:01,000 --> 00:00:02,000\nhi\n").unwrap();
//! assert_eq!(doc.cues.len(), 1);
//! let srt = build_srt(&doc);
//! assert!(srt.contains("00:00:01,000 --> 00:00:02,000"));
//!
//! // 双语 ASS：原文 + 译文双轨
//! let translated = doc.cues.clone();
//! let opts = AssOptions { layout: BilingualLayout::TranslatedBelow, ..AssOptions::default() };
//! let ass = build_bilingual_ass(&doc.cues, &translated, &opts);
//! assert!(ass.contains("Dialogue:"));
//! ```
//!
//! 任务 T7–T11。

#![forbid(unsafe_code)]
#![deny(missing_debug_implementations)]

pub mod ass;
pub mod inject;
pub mod model;
pub mod srt;
pub mod text;
pub mod time;
pub mod tools;
pub mod vtt;

pub use ass::{
    align_translations, ass_color_from_rgb, ass_color_from_rgba, build_ass_from_cues,
    build_bilingual_ass_report, build_bilingual_ass_report_with_header, normalize_ass_color,
    parse_ass_color_rgb, parse_ass_document, AssDocument, AssEvent, AssStyle,
};
pub use inject::{
    cleanup_temp_ass, layout_label, temp_ass_path, write_bilingual_ass,
    write_bilingual_ass_from_doc, write_temp_ass,
};
pub use model::{
    AssOptions, BilingualAss, BilingualLayout, Cue, CueStats, CueTracker, SubtitleDoc,
    SubtitleFormat,
};
pub use tools::{
    active_cues, dedupe_cues, guess_format_content, guess_format_path, merge_cues, offset_cues,
    offset_ms, stats, tracker,
};

use wp_core::Result;

/// 根据扩展名猜测格式（T7；`srt`/`vtt`/`ass`/`ssa`，大小写不敏感）。
pub fn guess_format(path: &str) -> Option<SubtitleFormat> {
    tools::guess_format_path(path)
}

/// 解析字幕文本为结构化文档（T7）。
///
/// 宽松解析：坏行/坏块跳过而不是整个失败；空文本返回空文档。
pub fn parse(format: SubtitleFormat, text: &str) -> Result<SubtitleDoc> {
    match format {
        SubtitleFormat::Srt => srt::parse_srt_doc(text),
        SubtitleFormat::Vtt => vtt::parse_vtt_doc(text),
        SubtitleFormat::Ass | SubtitleFormat::Ssa => ass::parse_ass(text),
    }
}

/// 自动识别格式后解析（先看扩展名，失败再看内容）。
pub fn parse_auto(path: Option<&str>, text: &str) -> Result<SubtitleDoc> {
    let format = path
        .and_then(tools::guess_format_path)
        .or_else(|| tools::guess_format_content(text))
        .unwrap_or(SubtitleFormat::Srt);
    parse(format, text)
}

/// 导出 SRT（T7）。按时间排序、空文本 cue 跳过、负时间裁剪。
pub fn build_srt(doc: &SubtitleDoc) -> String {
    srt::build_srt_cues(&doc.cues)
}

/// 导出 WebVTT。
pub fn build_vtt(doc: &SubtitleDoc) -> String {
    vtt::build_vtt_cues(&doc.cues)
}

/// 生成单语 ASS（SRT/VTT → ASS 转换；保留原 ASS 头如存在）。
pub fn build_ass(doc: &SubtitleDoc, opts: &AssOptions) -> String {
    ass::build_ass_from_cues(&doc.cues, opts, doc.ass_header.as_deref())
}

/// 生成双语 ASS（T7/T11）。
///
/// 签名与任务书一致；不保留原 ASS 头（默认样式表）。需要保留原样式/字体段时用
/// [`build_bilingual_ass_report_with_header`]。
pub fn build_bilingual_ass(original: &[Cue], translated: &[Cue], opts: &AssOptions) -> String {
    ass::build_bilingual_ass_report(original, translated, opts).content
}

/// 生成双语 ASS 并保留原 ASS 头（从文档中取 `ass_header`）。
pub fn build_bilingual_ass_from_doc(
    doc: &SubtitleDoc,
    translated: &[Cue],
    opts: &AssOptions,
) -> BilingualAss {
    ass::build_bilingual_ass_report_with_header(
        &doc.cues,
        translated,
        opts,
        doc.ass_header.as_deref(),
    )
}

pub use text::{
    decode_entities, escape_ass_text, normalize_newlines, split_leading_override, strip_effects,
    strip_html_tags, strip_override_blocks, unescape_ass_text,
};
pub use time::{
    clamp_ms, format_ass_timestamp, format_srt_timestamp, format_vtt_timestamp,
    parse_ass_timestamp, parse_srt_timestamp, parse_timestamp,
};

/// 字幕时间轴偏移（就地）。
pub fn shift_doc(doc: &mut SubtitleDoc, delta_ms: i64) {
    tools::offset_ms(doc, delta_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_to_end_srt_to_bilingual_ass() {
        let srt =
            "1\n00:00:01,000 --> 00:00:02,500\n你好\n\n2\n00:00:03,000 --> 00:00:04,000\n世界\n";
        let doc = parse_auto(Some("sample.srt"), srt).unwrap();
        assert_eq!(doc.format, Some(SubtitleFormat::Srt));
        assert_eq!(doc.cues.len(), 2);

        let translated = vec![
            Cue {
                start_ms: 1000,
                end_ms: 2500,
                text: "Hello".into(),
                raw_style: None,
            },
            Cue {
                start_ms: 3000,
                end_ms: 4000,
                text: "World".into(),
                raw_style: None,
            },
        ];
        let ass = build_bilingual_ass(&doc.cues, &translated, &AssOptions::default());
        let back = parse(SubtitleFormat::Ass, &ass).unwrap();
        // 2 条原文 + 2 条译文
        assert_eq!(back.cues.len(), 4);
        // 时间轴一致
        assert_eq!(back.cues[0].start_ms, 1000);
        assert_eq!(back.cues[1].start_ms, 1000);
        assert_eq!(back.cues[2].start_ms, 3000);
        assert_eq!(back.cues[3].start_ms, 3000);
    }

    #[test]
    fn parse_auto_content_sniffing() {
        let vtt = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nvtt text\n";
        let doc = parse_auto(Some("mismatched.srt"), vtt).unwrap();
        // 扩展名优先（srt 解析器也能吃这个时间行），但内容识别函数本身是对的
        assert_eq!(doc.cues.len(), 1);
        let doc2 = parse_auto(None, vtt).unwrap();
        assert_eq!(doc2.format, Some(SubtitleFormat::Vtt));
    }

    #[test]
    fn build_vtt_and_ass_wrappers() {
        let doc = SubtitleDoc::from_cues(vec![Cue {
            start_ms: 0,
            end_ms: 1000,
            text: "x".into(),
            raw_style: None,
        }]);
        assert!(build_vtt(&doc).starts_with("WEBVTT"));
        assert!(build_ass(&doc, &AssOptions::default()).contains("Dialogue:"));
    }

    #[test]
    fn build_bilingual_from_doc_preserves_header() {
        let ass_text = "[Script Info]\nScriptType: v4.00+\nPlayResX: 1280\nPlayResY: 720\n\n\
[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
Style: CN,Arial,40,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,1,0,2,10,10,20,1\n\n\
[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
Dialogue: 0,0:00:01.00,0:00:02.00,CN,,0,0,0,,原文\n";
        let doc = parse(SubtitleFormat::Ass, ass_text).unwrap();
        let translated = vec![Cue {
            start_ms: 1000,
            end_ms: 2000,
            text: "trans".into(),
            raw_style: None,
        }];
        let report = build_bilingual_ass_from_doc(&doc, &translated, &AssOptions::default());
        assert!(report.content.contains("Style: CN,"));
        assert!(report.content.contains("PlayResX: 1280"));
        assert_eq!(report.matched, 1);
    }

    #[test]
    fn shift_doc_wrapper() {
        let mut doc = SubtitleDoc::from_cues(vec![Cue {
            start_ms: 1000,
            end_ms: 2000,
            text: "x".into(),
            raw_style: None,
        }]);
        shift_doc(&mut doc, 500);
        assert_eq!(doc.cues[0].start_ms, 1500);
    }
}
