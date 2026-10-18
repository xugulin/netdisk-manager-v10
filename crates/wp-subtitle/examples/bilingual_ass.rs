//! 示例：SRT 字幕 → 双语 ASS（AI 翻译注入用）。
//!
//! 运行：`cargo run -p wp-subtitle --example bilingual_ass`

use wp_subtitle::{
    build_bilingual_ass_from_doc, parse, AssOptions, BilingualLayout, Cue, SubtitleFormat,
};

fn main() {
    let srt = "1\n\
00:00:01,000 --> 00:00:03,500\n\
你好，世界\n\
\n\
2\n\
00:00:04,000 --> 00:00:06,000\n\
这是第二句\n";
    let doc = parse(SubtitleFormat::Srt, srt).expect("解析 SRT");
    // 模拟 AI 翻译结果（时间轴保持一致）
    let translated: Vec<Cue> = doc
        .cues
        .iter()
        .map(|cue| Cue {
            text: format!("EN: {}", cue.text),
            ..cue.clone()
        })
        .collect();
    let opts = AssOptions {
        layout: BilingualLayout::TranslatedBelow,
        margin_v: 40,
        font_size: 48,
        ..AssOptions::default()
    };
    let report = build_bilingual_ass_from_doc(&doc, &translated, &opts);
    eprintln!(
        "matched={} missing={} bytes={}",
        report.matched,
        report.missing,
        report.content.len()
    );
    print!("{}", report.content);
}
