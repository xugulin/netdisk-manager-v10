//! 弹幕格式矩阵集成测试（task-10 验收自动化）。
//!
//! 读取 `tests/fixtures/matrix/` 下 7 组固定样本 + `expected.json`（人工编写期望值），
//! 对每组断言：来源 / 条数 / 首尾时间 / 文本 / 模式 / 颜色 / 字号 / 用户，
//! 并验证导出往返（XML/JSON/ASS）与生成矩阵（1000 条/分钟、4K/1080p 一致、屏蔽通配符）。
//!
//! 运行：`cargo test -p wp-danmaku --test matrix`
//! 报告与集成素材：`cargo run -p wp-danmaku --example matrix_danmaku`

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use wp_danmaku::{
    export_ass, export_bilibili_xml, export_json, generate_ass, import_ass, layout,
    layout_with_stats, parse_bilibili_xml, parse_json, AssRenderOptions, Comment, DanmakuDoc,
    DanmakuFilter, DanmakuMode, DanmakuSource, DensityPolicy, LayoutConfig, PlacedTimeline,
};

#[derive(Debug, Deserialize)]
struct ExpectedFile {
    file: String,
    kind: String,
    source: String,
    comment_count: usize,
    first_time_ms: i64,
    last_time_ms: i64,
    #[serde(default)]
    texts: Option<Vec<String>>,
    #[serde(default)]
    modes: Option<Vec<String>>,
    #[serde(default)]
    colors: Option<Vec<u32>>,
    #[serde(default)]
    font_sizes: Option<Vec<f32>>,
    #[serde(default)]
    users: Option<Vec<Option<String>>>,
}

#[derive(Debug, Deserialize)]
struct Expected {
    files: Vec<ExpectedFile>,
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/matrix")
}

fn load_expected() -> Expected {
    let path = fixtures_dir().join("expected.json");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("缺少期望值文件 {}：{e}", path.display()));
    serde_json::from_str(&text).expect("expected.json 解析失败")
}

fn source_name(source: &DanmakuSource) -> &'static str {
    match source {
        DanmakuSource::BilibiliXml => "BilibiliXml",
        DanmakuSource::LocalFile => "LocalFile",
        DanmakuSource::Generated => "Generated",
        DanmakuSource::Online(_) => "Online",
    }
}

fn mode_name(mode: DanmakuMode) -> &'static str {
    match mode {
        DanmakuMode::Scroll => "Scroll",
        DanmakuMode::Top => "Top",
        DanmakuMode::Bottom => "Bottom",
    }
}

fn load_doc(item: &ExpectedFile) -> DanmakuDoc {
    let path = fixtures_dir().join(&item.file);
    let text =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("缺少样本 {}：{e}", path.display()));
    match item.kind.as_str() {
        "bilibili_xml" => parse_bilibili_xml(&text),
        "json" => parse_json(&text),
        "ass_import" => import_ass(&text),
        other => panic!("{} 未知样本类型 {other}", item.file),
    }
    .unwrap_or_else(|e| panic!("{} 解析失败：{e}", item.file))
}

#[test]
fn danmaku_matrix_has_enough_cases() {
    let expected = load_expected();
    assert!(
        expected.files.len() >= 6,
        "弹幕矩阵样本数不足：{}",
        expected.files.len()
    );
}

#[test]
fn danmaku_matrix_matches_expected_values() {
    for item in load_expected().files {
        let doc = load_doc(&item);
        assert_eq!(
            source_name(&doc.source),
            item.source,
            "{} 来源不符",
            item.file
        );
        assert_eq!(
            doc.comments.len(),
            item.comment_count,
            "{} 条数不符（texts={:?}）",
            item.file,
            doc.comments.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        let first = doc.comments.first().map_or(0, |c| c.time_ms);
        let last = doc.comments.iter().map(|c| c.time_ms).max().unwrap_or(0);
        assert_eq!(first, item.first_time_ms, "{} 首条时间不符", item.file);
        assert_eq!(last, item.last_time_ms, "{} 末条时间不符", item.file);
        if let Some(texts) = &item.texts {
            for (index, text) in texts.iter().enumerate() {
                assert_eq!(
                    doc.comments.get(index).map(|c| c.text.as_str()),
                    Some(text.as_str()),
                    "{} 第 {index} 条文本不符",
                    item.file
                );
            }
        }
        if let Some(modes) = &item.modes {
            for (index, mode) in modes.iter().enumerate() {
                let actual = doc.comments.get(index).map(|c| mode_name(c.mode));
                assert_eq!(
                    actual,
                    Some(mode.as_str()),
                    "{} 第 {index} 条模式不符",
                    item.file
                );
            }
        }
        if let Some(colors) = &item.colors {
            for (index, color) in colors.iter().enumerate() {
                assert_eq!(
                    doc.comments.get(index).map(|c| c.color),
                    Some(*color),
                    "{} 第 {index} 条颜色不符",
                    item.file
                );
            }
        }
        if let Some(sizes) = &item.font_sizes {
            for (index, size) in sizes.iter().enumerate() {
                let actual = doc.comments.get(index).map(|c| c.font_size).unwrap_or(0.0);
                assert!(
                    (actual - size).abs() < 0.01,
                    "{} 第 {index} 条字号不符（{actual} vs {size}）",
                    item.file
                );
            }
        }
        if let Some(users) = &item.users {
            for (index, user) in users.iter().enumerate() {
                assert_eq!(
                    doc.comments.get(index).and_then(|c| c.user.clone()),
                    *user,
                    "{} 第 {index} 条用户不符",
                    item.file
                );
            }
        }
    }
}

/// 导出往返：XML / JSON 经导出再解析后，条数/时间/模式/颜色/文本/用户一致。
#[test]
fn danmaku_matrix_export_roundtrip() {
    for item in load_expected().files {
        let doc = load_doc(&item);
        let (back, label) = if item.kind == "bilibili_xml" {
            let xml = export_bilibili_xml(&doc);
            (
                parse_bilibili_xml(&xml)
                    .unwrap_or_else(|e| panic!("{} XML 往返失败：{e}", item.file)),
                "XML",
            )
        } else if item.kind == "json" {
            let json = export_json(&doc).unwrap();
            (
                parse_json(&json).unwrap_or_else(|e| panic!("{} JSON 往返失败：{e}", item.file)),
                "JSON",
            )
        } else {
            // ASS 导入的弹幕：导出成 ASS 弹幕轨再导入，事件数/文本一致
            let ass = export_ass(&doc.comments, &LayoutConfig::hd1080());
            (
                import_ass(&ass).unwrap_or_else(|e| panic!("{} ASS 往返失败：{e}", item.file)),
                "ASS",
            )
        };
        assert_eq!(
            back.comments.len(),
            doc.comments.len(),
            "{} {label} 往返条数不符",
            item.file
        );
        for (a, b) in doc.comments.iter().zip(back.comments.iter()) {
            assert_eq!(a.time_ms, b.time_ms, "{} {label} 往返时间不符", item.file);
            assert_eq!(a.text, b.text, "{} {label} 往返文本不符", item.file);
            assert_eq!(a.mode, b.mode, "{} {label} 往返模式不符", item.file);
            assert_eq!(a.color, b.color, "{} {label} 往返颜色不符", item.file);
            assert_eq!(a.user, b.user, "{} {label} 往返用户不符", item.file);
        }
    }
}

/// 生成矩阵：1000 条/分钟密度样本全部布局、同屏不超密度上限、ASS 事件数一致。
#[test]
fn danmaku_matrix_density_1000_per_minute() {
    let comments: Vec<Comment> = (0..1000)
        .map(|i| Comment::new(i * 60, DanmakuMode::Scroll, format!("密度{i}")))
        .collect();
    let cfg = LayoutConfig::hd1080();
    let (placed, stats) = layout_with_stats(&comments, &cfg, DensityPolicy::DropNew);
    assert_eq!(stats.input_count, 1000);
    assert_eq!(stats.placed_count, 1000, "1000 条/分钟应全部成功布局");
    assert_eq!(stats.dropped_no_lane, 0);
    assert_eq!(stats.dropped_density, 0);
    let timeline = PlacedTimeline::new(placed.clone());
    // 每 100ms 采样峰值同屏，必须 ≤ density_limit
    let mut peak = 0;
    let mut at = 0;
    while at <= 60_000 {
        peak = peak.max(timeline.active_count(at));
        at += 100;
    }
    assert!(
        peak <= cfg.density_limit,
        "峰值同屏 {peak} 超过密度上限 {}",
        cfg.density_limit
    );
    let ass = generate_ass(&placed, &cfg);
    assert_eq!(ass.matches("Dialogue:").count(), 1000);
}

/// 4K 与 1080p：布局相对坐标一致、ASS 输出完全一致。
#[test]
fn danmaku_matrix_4k_1080p_consistent() {
    let comments: Vec<Comment> = (0..120)
        .map(|i| {
            let mode = match i % 3 {
                0 => DanmakuMode::Scroll,
                1 => DanmakuMode::Top,
                _ => DanmakuMode::Bottom,
            };
            Comment::new(i * 300, mode, format!("分辨率弹幕{i}"))
        })
        .collect();
    let hd = layout(&comments, &LayoutConfig::hd1080());
    let uhd = layout(&comments, &LayoutConfig::uhd4k());
    assert_eq!(hd.len(), uhd.len());
    for (a, b) in hd.iter().zip(uhd.iter()) {
        assert_eq!(a.comment.text, b.comment.text);
        assert_eq!(a.lane, b.lane);
        assert!((a.y_ratio - b.y_ratio).abs() < 1e-6);
        assert!((a.width_ratio - b.width_ratio).abs() < 1e-6);
        assert_eq!((a.start_ms, a.end_ms), (b.start_ms, b.end_ms));
    }
    let ass_hd = generate_ass(&hd, &LayoutConfig::hd1080());
    let ass_uhd = generate_ass(&uhd, &LayoutConfig::uhd4k());
    assert_eq!(ass_hd, ass_uhd);
    // 时间窗分片：只保留窗口内事件
    let window = AssRenderOptions::windowed(10_000, 20_000);
    let chunk = wp_danmaku::generate_ass_with_options(&hd, &LayoutConfig::hd1080(), &window);
    let chunk_count = chunk.matches("Dialogue:").count();
    assert!(chunk_count > 0 && chunk_count < hd.len(), "时间窗裁剪无效");
}

/// 屏蔽矩阵：关键词/通配符/用户/类型 过滤计数与保留文本。
#[test]
fn danmaku_matrix_filter() {
    let mut a = Comment::new(0, DanmakuMode::Scroll, "这里是正常弹幕");
    a.user = None;
    let mut b = Comment::new(100, DanmakuMode::Scroll, "这是广告内容");
    b.user = Some("spammer".into());
    let c = Comment::new(200, DanmakuMode::Top, "顶部正常");
    let mut d = Comment::new(300, DanmakuMode::Scroll, "用户被屏蔽的人");
    d.user = Some("bad_user".into());
    let e = Comment::new(400, DanmakuMode::Scroll, "233333");
    let comments = vec![a, b, c, d, e];
    let filter = DanmakuFilter {
        keywords: vec!["广告".into()],
        patterns: vec!["233*".into()],
        blocked_users: vec!["bad_user".into()],
        blocked_modes: vec![DanmakuMode::Top],
        case_sensitive: false,
    };
    assert_eq!(filter.blocked_count(&comments), 4);
    let kept = filter.apply(&comments);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].text, "这里是正常弹幕");
    assert!(filter.is_blocked(&comments[1]));
    assert_eq!(
        filter.reason(&comments[3]),
        Some(wp_danmaku::FilterReason::User)
    );
}
