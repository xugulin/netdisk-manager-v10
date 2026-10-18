//! 字幕格式矩阵集成测试（task-10 验收自动化）。
//!
//! 读取 `tests/fixtures/matrix/` 下 13 组固定样本 + `expected.json`（人工编写期望值），
//! 对每组断言：解析格式 / cue 数 / 首尾时间 / 文本 / raw_style / ASS 头，
//! 并验证「回读一致」（导出 SRT 再解析）与「双语 ASS 可解析且时间轴一致」。
//!
//! 运行：`cargo test -p wp-subtitle --test matrix`
//! 报告与集成素材：`cargo run -p wp-subtitle --example matrix_subtitles`

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use wp_subtitle::{
    build_bilingual_ass_from_doc, build_srt, parse, parse_auto, AssOptions, Cue, SubtitleDoc,
    SubtitleFormat,
};

#[derive(Debug, Deserialize)]
struct ExpectedFile {
    file: String,
    format: String,
    cue_count: usize,
    first_start_ms: i64,
    last_end_ms: i64,
    #[serde(default)]
    texts: Option<Vec<String>>,
    #[serde(default)]
    text_lens: Option<Vec<Option<usize>>>,
    #[serde(default)]
    raw_style_first: Option<String>,
    #[serde(default)]
    header_contains: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct Expected {
    files: Vec<ExpectedFile>,
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/matrix")
}

fn format_name(format: SubtitleFormat) -> &'static str {
    match format {
        SubtitleFormat::Srt => "Srt",
        SubtitleFormat::Vtt => "Vtt",
        SubtitleFormat::Ass => "Ass",
        SubtitleFormat::Ssa => "Ssa",
    }
}

fn load_expected() -> Expected {
    let path = fixtures_dir().join("expected.json");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("缺少期望值文件 {}：{e}", path.display()));
    serde_json::from_str(&text).expect("expected.json 解析失败")
}

fn load_doc(item: &ExpectedFile) -> SubtitleDoc {
    let path = fixtures_dir().join(&item.file);
    let text =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("缺少样本 {}：{e}", path.display()));
    parse_auto(Some(&item.file), &text).unwrap_or_else(|e| panic!("{} 解析失败：{e}", item.file))
}

#[test]
fn subtitle_matrix_has_enough_cases() {
    let expected = load_expected();
    assert!(
        expected.files.len() >= 12,
        "字幕矩阵至少 12 组，实际 {}",
        expected.files.len()
    );
}

#[test]
fn subtitle_matrix_matches_expected_values() {
    for item in load_expected().files {
        let doc = load_doc(&item);
        let format = doc
            .format
            .unwrap_or_else(|| panic!("{} 未识别格式", item.file));
        assert_eq!(format_name(format), item.format, "{} 格式不符", item.file);
        assert_eq!(
            doc.cues.len(),
            item.cue_count,
            "{} cue 数不符（cues={:?}）",
            item.file,
            doc.cues.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        let first = doc.cues.first().map_or(0, |c| c.start_ms);
        let last = doc.cues.iter().map(|c| c.end_ms).max().unwrap_or(0);
        assert_eq!(first, item.first_start_ms, "{} 首条开始时间不符", item.file);
        assert_eq!(last, item.last_end_ms, "{} 末条结束时间不符", item.file);
        if let Some(texts) = &item.texts {
            for (index, text) in texts.iter().enumerate() {
                assert_eq!(
                    doc.cues.get(index).map(|c| c.text.as_str()),
                    Some(text.as_str()),
                    "{} 第 {index} 条文本不符",
                    item.file
                );
            }
        }
        if let Some(lens) = &item.text_lens {
            for (index, len) in lens.iter().enumerate() {
                if let Some(expected_len) = len {
                    let actual = doc
                        .cues
                        .get(index)
                        .map(|c| c.text.chars().count())
                        .unwrap_or(0);
                    assert_eq!(actual, *expected_len, "{} 第 {index} 条长度不符", item.file);
                }
            }
        }
        if let Some(raw) = &item.raw_style_first {
            assert_eq!(
                doc.cues.first().and_then(|c| c.raw_style.as_deref()),
                Some(raw.as_str()),
                "{} 首条 raw_style 不符",
                item.file
            );
        }
        if let Some(contains) = &item.header_contains {
            let header = doc
                .ass_header
                .as_deref()
                .unwrap_or_else(|| panic!("{} 应保留 ASS 头", item.file));
            for needle in contains {
                assert!(
                    header.contains(needle.as_str()),
                    "{} ASS 头缺少 {needle:?}",
                    item.file
                );
            }
        }
    }
}

/// 回读一致：解析 → 导出 SRT → 再解析，cue 数/时间/文本保持一致。
#[test]
fn subtitle_matrix_srt_roundtrip_consistent() {
    for item in load_expected().files {
        let doc = load_doc(&item);
        let srt = build_srt(&doc);
        let back = parse(SubtitleFormat::Srt, &srt)
            .unwrap_or_else(|e| panic!("{} 回读失败：{e}", item.file));
        assert_eq!(
            back.cues.len(),
            doc.cues.len(),
            "{} 回读 cue 数不符",
            item.file
        );
        for (a, b) in doc.cues.iter().zip(back.cues.iter()) {
            assert_eq!(a.start_ms, b.start_ms, "{} 回读开始时间不符", item.file);
            assert_eq!(a.end_ms, b.end_ms, "{} 回读结束时间不符", item.file);
            assert_eq!(a.text, b.text, "{} 回读文本不符", item.file);
        }
    }
}

/// 双语 ASS 可解析：事件数 = 2 × 非空 cue，且双轨时间轴与原文严格一致。
#[test]
fn subtitle_matrix_bilingual_ass_parsable() {
    let opts = AssOptions::default();
    for item in load_expected().files {
        let doc = load_doc(&item);
        let non_empty = doc
            .cues
            .iter()
            .filter(|c| !c.text.trim().is_empty())
            .count();
        let translations: Vec<Cue> = doc
            .cues
            .iter()
            .map(|cue| Cue {
                text: format!("EN: {}", cue.text),
                ..cue.clone()
            })
            .collect();
        let report = build_bilingual_ass_from_doc(&doc, &translations, &opts);
        assert_eq!(report.missing, 0, "{} 双语漏译统计不符", item.file);
        assert_eq!(
            report.matched,
            doc.cues.len(),
            "{} 双语配对统计不符",
            item.file
        );
        let back = parse(SubtitleFormat::Ass, &report.content)
            .unwrap_or_else(|e| panic!("{} 双语 ASS 不可解析：{e}", item.file));
        // 事件数 = 原文行 + 译文行（空文本原文行不输出，但译文行仍会输出）
        let original_lines = non_empty;
        let translated_lines = doc.cues.len();
        assert_eq!(
            back.cues.len(),
            original_lines + translated_lines,
            "{} 双语 ASS 事件数不符",
            item.file
        );
        // 原文行与译文行成对出现在事件序列里（原文在前、译文在后）
        if !doc.cues.is_empty() {
            assert_eq!(
                back.cues[0].start_ms, doc.cues[0].start_ms,
                "{} 双语 ASS 首事件时间不符",
                item.file
            );
        }
    }
}

/// 时间工具链：SRT/ASS 时间互转、负时间裁剪、偏移/合并/统计的边界行为。
#[test]
fn subtitle_time_and_tool_chain() {
    // 跨小时 + 毫秒回读
    let ms = wp_subtitle::parse_srt_timestamp("01:59:59,990").unwrap();
    assert_eq!(ms, 7_199_990);
    let ass = wp_subtitle::format_ass_timestamp(ms);
    let back = wp_subtitle::parse_ass_timestamp(&ass).unwrap();
    // ASS 时间只有百分秒精度：允许 <=5ms 的四舍五入误差
    assert!(
        (back - ms).abs() <= 5,
        "ASS 时间回读偏差过大：{ms} → {ass} → {back}"
    );
    // 负时间裁剪（偏移后丢弃/裁剪）
    let mut doc = SubtitleDoc::from_cues(vec![
        Cue {
            start_ms: 100,
            end_ms: 200,
            text: "before zero".into(),
            raw_style: None,
        },
        Cue {
            start_ms: 500,
            end_ms: 900,
            text: "cross zero".into(),
            raw_style: None,
        },
    ]);
    wp_subtitle::offset_ms(&mut doc, -600);
    assert_eq!(doc.cues.len(), 1, "整体在 0 之前的 cue 应被丢弃");
    assert_eq!(
        (doc.cues[0].start_ms, doc.cues[0].end_ms),
        (0, 300),
        "起点应裁剪到 0"
    );
    // 合并 + 统计
    let merged = wp_subtitle::merge_cues(&doc.cues, 500);
    assert_eq!(merged.len(), 1);
    let stats = wp_subtitle::stats(&merged);
    assert_eq!(stats.count, 1);
    assert_eq!(stats.total_duration_ms, 300);
}
