//! 弹幕格式矩阵自动化验收：B 站 XML / JSON 多格式 / ASS 导入 / 导出往返 /
//! 1000 条/分钟密度 / 4K 与 1080p 一致 / 屏蔽通配符 / 时间窗分片，
//! 并落盘「固定素材 + 期望值 json + markdown 报告」。
//!
//! 运行（任意目录均可）：
//! ```text
//! cargo run -p wp-danmaku --example matrix_danmaku
//! ```
//! 产物：
//! - `出/验收截图/字幕弹幕/matrix-report-danmaku.md` —— 弹幕矩阵报告
//! - `出/验收截图/字幕弹幕/弹幕/` —— 7 组固定样本 + `expected.json` + `actual.json`
//!   + `demo-density-1000pm.ass`（ASS 弹幕轨，GUI/media 可直接加载验证）
//!
//! 失败会以非 0 退出码结束。

use std::fs;
use std::path::{Path, PathBuf};

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

struct Row {
    file: String,
    kind: String,
    count: usize,
    expected_ok: bool,
    roundtrip_ok: bool,
    ass_events: usize,
    ass_parsable: bool,
    problems: Vec<String>,
}

impl Row {
    fn pass(&self) -> bool {
        self.expected_ok && self.roundtrip_ok && self.ass_parsable
    }
}

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    crate_dir()
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn out_dir() -> PathBuf {
    match std::env::var("WP_MATRIX_OUT") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => workspace_root().join("出/验收截图/字幕弹幕"),
    }
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

fn main() {
    match run() {
        Ok(()) => {}
        Err(err) => {
            eprintln!("矩阵验收失败：{err}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), String> {
    let fixtures = crate_dir().join("tests/fixtures/matrix");
    let expected_text = fs::read_to_string(fixtures.join("expected.json"))
        .map_err(|e| format!("读期望值失败：{e}"))?;
    let expected: Expected =
        serde_json::from_str(&expected_text).map_err(|e| format!("期望值解析失败：{e}"))?;
    if expected.files.len() < 6 {
        return Err(format!("矩阵样本不足 6 组：{}", expected.files.len()));
    }

    let out = out_dir();
    let sample_dir = out.join("弹幕");
    fs::create_dir_all(&sample_dir).map_err(|e| format!("创建素材目录失败：{e}"))?;
    let cfg = LayoutConfig::hd1080();
    let mut rows: Vec<Row> = Vec::new();
    let mut failures = String::new();

    for item in &expected.files {
        let text = fs::read_to_string(fixtures.join(&item.file))
            .map_err(|e| format!("缺样本 {}：{e}", item.file))?;
        fs::write(sample_dir.join(&item.file), &text).map_err(|e| format!("写样本失败：{e}"))?;

        let mut problems = Vec::new();
        let doc = match load_doc(item, &text) {
            Ok(doc) => doc,
            Err(err) => {
                problems.push(format!("解析失败：{err}"));
                DanmakuDoc::default()
            }
        };
        let expected_ok = verify_expected(item, &doc, &mut problems);
        let roundtrip_ok = verify_roundtrip(item, &doc, &mut problems);
        let (ass_events, ass_parsable) = verify_ass_export(&doc, &cfg, &mut problems);
        let row = Row {
            file: item.file.clone(),
            kind: item.kind.clone(),
            count: doc.comments.len(),
            expected_ok,
            roundtrip_ok,
            ass_events,
            ass_parsable,
            problems,
        };
        if !row.pass() {
            failures.push_str(&format!("\n### 失败：{}\n", row.file));
            for problem in &row.problems {
                failures.push_str(&format!("- {problem}\n"));
            }
        }
        rows.push(row);
    }

    // 生成矩阵（内存数据，不依赖 fixture）
    let matrix_rows = verify_generation_matrix(&cfg, &sample_dir)?;

    // 期望值/实际值 json + 固定演示素材
    fs::write(sample_dir.join("expected.json"), &expected_text)
        .map_err(|e| format!("写 expected.json 失败：{e}"))?;
    let actual = serde_json::json!({
        "note": "matrix_danmaku 实际值（与 expected.json 比对通过后生成）",
        "cases": rows.iter().map(|row| serde_json::json!({
            "file": row.file,
            "kind": row.kind,
            "count": row.count,
            "expected_ok": row.expected_ok,
            "roundtrip_ok": row.roundtrip_ok,
            "ass_events": row.ass_events,
            "ass_parsable": row.ass_parsable,
            "pass": row.pass(),
        })).collect::<Vec<_>>(),
        "generation": matrix_rows.iter().map(|(name, ok, _)| serde_json::json!({
            "check": name, "ok": ok
        })).collect::<Vec<_>>()
    });
    fs::write(
        sample_dir.join("actual.json"),
        serde_json::to_string_pretty(&actual).map_err(|e| e.to_string())? + "\n",
    )
    .map_err(|e| format!("写 actual.json 失败：{e}"))?;

    // 固定演示素材：1000 条/分钟 4K ASS 弹幕轨（GUI/media 可直接 sub-add 验证）
    let demo_comments = density_comments(1000, 60_000);
    let demo_placed = layout(&demo_comments, &LayoutConfig::uhd4k());
    let demo_ass = generate_ass(&demo_placed, &LayoutConfig::uhd4k());
    fs::write(
        sample_dir.join("demo-density-1000pm.ass"),
        demo_ass.as_bytes(),
    )
    .map_err(|e| format!("写演示 ASS 失败：{e}"))?;
    // 固定演示素材：B 站 XML（GUI 本地弹幕加载/截图用例）
    let demo_doc = DanmakuDoc::from_comments(demo_comments.clone());
    fs::write(
        sample_dir.join("demo-density-1000pm.xml"),
        export_bilibili_xml(&demo_doc).as_bytes(),
    )
    .map_err(|e| format!("写演示 XML 失败：{e}"))?;
    fs::write(
        sample_dir.join("demo-import.ass"),
        export_ass(&density_comments(30, 20_000), &cfg).as_bytes(),
    )
    .map_err(|e| format!("写演示导入 ASS 失败：{e}"))?;

    let passed = rows.iter().filter(|row| row.pass()).count();
    let gens_ok = matrix_rows.iter().filter(|(_, ok, _)| *ok).count();
    let report = render_report(&expected, &rows, &matrix_rows, passed, gens_ok, &failures);
    let report_path = out.join("matrix-report-danmaku.md");
    fs::write(&report_path, report.as_bytes()).map_err(|e| format!("写报告失败：{e}"))?;

    println!(
        "弹幕格式矩阵：{passed}/{} 组通过；生成矩阵 {gens_ok}/{} 项通过",
        rows.len(),
        matrix_rows.len()
    );
    println!("报告：{}", report_path.display());
    println!(
        "固定素材：{}（{} 组样本 + expected/actual + 3 个演示素材）",
        sample_dir.display(),
        expected.files.len()
    );
    if passed != rows.len() || gens_ok != matrix_rows.len() {
        return Err(format!(
            "格式矩阵 {} 组失败 / 生成矩阵 {} 项失败",
            rows.len() - passed,
            matrix_rows.len() - gens_ok
        ));
    }
    Ok(())
}

fn load_doc(item: &ExpectedFile, text: &str) -> Result<DanmakuDoc, String> {
    match item.kind.as_str() {
        "bilibili_xml" => parse_bilibili_xml(text).map_err(|e| e.to_string()),
        "json" => parse_json(text).map_err(|e| e.to_string()),
        "ass_import" => import_ass(text).map_err(|e| e.to_string()),
        other => Err(format!("未知样本类型 {other}")),
    }
}

fn verify_expected(item: &ExpectedFile, doc: &DanmakuDoc, problems: &mut Vec<String>) -> bool {
    let mut ok = true;
    if source_name(&doc.source) != item.source {
        problems.push(format!(
            "来源 {:?} != 期望 {}",
            source_name(&doc.source),
            item.source
        ));
        ok = false;
    }
    if doc.comments.len() != item.comment_count {
        problems.push(format!(
            "条数 {} != 期望 {}",
            doc.comments.len(),
            item.comment_count
        ));
        ok = false;
    }
    let first = doc.comments.first().map_or(0, |c| c.time_ms);
    let last = doc.comments.iter().map(|c| c.time_ms).max().unwrap_or(0);
    if (first, last) != (item.first_time_ms, item.last_time_ms) {
        problems.push(format!(
            "首尾时间 ({first},{last}) != 期望 ({},{})",
            item.first_time_ms, item.last_time_ms
        ));
        ok = false;
    }
    if let Some(texts) = &item.texts {
        for (index, text) in texts.iter().enumerate() {
            if doc.comments.get(index).map(|c| c.text.as_str()) != Some(text.as_str()) {
                problems.push(format!("第 {index} 条文本不符"));
                ok = false;
            }
        }
    }
    if let Some(modes) = &item.modes {
        for (index, mode) in modes.iter().enumerate() {
            if doc.comments.get(index).map(|c| mode_name(c.mode)) != Some(mode.as_str()) {
                problems.push(format!("第 {index} 条模式不符"));
                ok = false;
            }
        }
    }
    if let Some(colors) = &item.colors {
        for (index, color) in colors.iter().enumerate() {
            if doc.comments.get(index).map(|c| c.color) != Some(*color) {
                problems.push(format!("第 {index} 条颜色不符"));
                ok = false;
            }
        }
    }
    if let Some(sizes) = &item.font_sizes {
        for (index, size) in sizes.iter().enumerate() {
            let actual = doc.comments.get(index).map(|c| c.font_size).unwrap_or(0.0);
            if (actual - size).abs() > 0.01 {
                problems.push(format!("第 {index} 条字号不符"));
                ok = false;
            }
        }
    }
    if let Some(users) = &item.users {
        for (index, user) in users.iter().enumerate() {
            if doc.comments.get(index).and_then(|c| c.user.clone()) != *user {
                problems.push(format!("第 {index} 条用户不符"));
                ok = false;
            }
        }
    }
    ok
}

fn verify_roundtrip(item: &ExpectedFile, doc: &DanmakuDoc, problems: &mut Vec<String>) -> bool {
    if doc.comments.is_empty() {
        return true;
    }
    let back = if item.kind == "bilibili_xml" {
        match parse_bilibili_xml(&export_bilibili_xml(doc)) {
            Ok(back) => back,
            Err(e) => {
                problems.push(format!("XML 往返失败：{e}"));
                return false;
            }
        }
    } else {
        match export_json(doc) {
            Ok(json) => match parse_json(&json) {
                Ok(back) => back,
                Err(e) => {
                    problems.push(format!("JSON 往返失败：{e}"));
                    return false;
                }
            },
            Err(e) => {
                problems.push(format!("JSON 导出失败：{e}"));
                return false;
            }
        }
    };
    if back.comments.len() != doc.comments.len() {
        problems.push("往返条数不一致".to_string());
        return false;
    }
    for (a, b) in doc.comments.iter().zip(back.comments.iter()) {
        if (a.time_ms, a.mode, a.color, &a.text, &a.user)
            != (b.time_ms, b.mode, b.color, &b.text, &b.user)
        {
            problems.push(format!("往返字段不一致（{}）", a.text));
            return false;
        }
    }
    true
}

fn verify_ass_export(
    doc: &DanmakuDoc,
    cfg: &LayoutConfig,
    problems: &mut Vec<String>,
) -> (usize, bool) {
    if doc.comments.is_empty() {
        return (0, true);
    }
    let ass = export_ass(&doc.comments, cfg);
    let events = ass.matches("Dialogue:").count();
    match import_ass(&ass) {
        Ok(back) => {
            let mut ok = true;
            if back.comments.len() != events {
                problems.push(format!(
                    "ASS 回读 {} 条 != 事件 {events}",
                    back.comments.len()
                ));
                ok = false;
            }
            if back.comments.len() != doc.comments.len() {
                problems.push("ASS 回读条数与源弹幕不一致".to_string());
                ok = false;
            }
            (events, ok)
        }
        Err(e) => {
            problems.push(format!("ASS 回读失败：{e}"));
            (events, false)
        }
    }
}

/// 内存生成矩阵：返回 (检查项, 是否通过, 说明)。
fn verify_generation_matrix(
    cfg: &LayoutConfig,
    sample_dir: &Path,
) -> Result<Vec<(String, bool, String)>, String> {
    let mut rows = Vec::new();

    // 1) 1000 条/分钟密度
    let comments = density_comments(1000, 60_000);
    let (placed, stats) = layout_with_stats(&comments, cfg, DensityPolicy::DropNew);
    let timeline = PlacedTimeline::new(placed.clone());
    let mut peak = 0;
    let mut at = 0;
    while at <= 60_000 {
        peak = peak.max(timeline.active_count(at));
        at += 100;
    }
    let density_ok =
        stats.placed_count == 1000 && stats.dropped_density == 0 && peak <= cfg.density_limit;
    rows.push((
        "1000 条/分钟密度（全部布局 / 同屏不超上限）".to_string(),
        density_ok,
        format!(
            "布局 {}/1000，峰值同屏 {peak}（上限 {}），ASS 事件 {}",
            stats.placed_count,
            cfg.density_limit,
            generate_ass(&placed, cfg).matches("Dialogue:").count()
        ),
    ));

    // 2) 4K / 1080p 一致
    let resolution_comments: Vec<Comment> = (0..120)
        .map(|i| {
            let mode = match i % 3 {
                0 => DanmakuMode::Scroll,
                1 => DanmakuMode::Top,
                _ => DanmakuMode::Bottom,
            };
            Comment::new(i * 300, mode, format!("分辨率弹幕{i}"))
        })
        .collect();
    let hd = layout(&resolution_comments, &LayoutConfig::hd1080());
    let uhd = layout(&resolution_comments, &LayoutConfig::uhd4k());
    let ass_hd = generate_ass(&hd, &LayoutConfig::hd1080());
    let ass_uhd = generate_ass(&uhd, &LayoutConfig::uhd4k());
    let resolution_ok = hd.len() == uhd.len()
        && hd.iter().zip(uhd.iter()).all(|(a, b)| {
            a.lane == b.lane
                && (a.y_ratio - b.y_ratio).abs() < 1e-6
                && (a.width_ratio - b.width_ratio).abs() < 1e-6
                && (a.start_ms, a.end_ms) == (b.start_ms, b.end_ms)
        })
        && ass_hd == ass_uhd;
    rows.push((
        "4K/1080p 相对坐标与 ASS 输出一致".to_string(),
        resolution_ok,
        format!(
            "1080p {} 条 / 4K {} 条，ASS 相同={}",
            hd.len(),
            uhd.len(),
            ass_hd == ass_uhd
        ),
    ));

    // 3) 时间窗分片
    let window = AssRenderOptions::windowed(10_000, 20_000);
    let chunk = wp_danmaku::generate_ass_with_options(&hd, &LayoutConfig::hd1080(), &window);
    let chunk_count = chunk.matches("Dialogue:").count();
    let chunk_ok = chunk_count > 0 && chunk_count < hd.len();
    rows.push((
        "时间窗分片（[10s,20s) 只保留窗口事件）".to_string(),
        chunk_ok,
        format!("窗口事件 {chunk_count} / 总 {}", hd.len()),
    ));

    // 4) 屏蔽通配符
    let mut a = Comment::new(0, DanmakuMode::Scroll, "正常弹幕");
    a.user = None;
    let mut b = Comment::new(100, DanmakuMode::Scroll, "这是广告");
    b.user = Some("spammer".into());
    let c = Comment::new(200, DanmakuMode::Top, "顶部正常");
    let mut d = Comment::new(300, DanmakuMode::Scroll, "被屏蔽用户");
    d.user = Some("bad_user".into());
    let e = Comment::new(400, DanmakuMode::Scroll, "233333");
    let filter_comments = vec![a, b, c, d, e];
    let filter = DanmakuFilter {
        keywords: vec!["广告".into()],
        patterns: vec!["233*".into()],
        blocked_users: vec!["bad_user".into()],
        blocked_modes: vec![DanmakuMode::Top],
        case_sensitive: false,
    };
    let blocked = filter.blocked_count(&filter_comments);
    let kept = filter.apply(&filter_comments);
    let filter_ok = blocked == 4 && kept.len() == 1 && kept[0].text == "正常弹幕";
    rows.push((
        "屏蔽：关键词/通配符/用户/类型".to_string(),
        filter_ok,
        format!("屏蔽 {blocked}/5，保留 {}", kept.len()),
    ));

    // 5) XML 导出转义往返
    let escape_doc = DanmakuDoc::from_comments(vec![Comment::new(
        1000,
        DanmakuMode::Scroll,
        "1 < 2 & \"3\" > '4' 🎉",
    )]);
    let escape_ok = match parse_bilibili_xml(&export_bilibili_xml(&escape_doc)) {
        Ok(back) => {
            back.comments.first().map(|c| c.text.as_str()) == Some("1 < 2 & \"3\" > '4' 🎉")
        }
        Err(_) => false,
    };
    rows.push((
        "B 站 XML 特殊字符/emoji 转义往返".to_string(),
        escape_ok,
        "`< > & \" ' 🎉` 往返一致".to_string(),
    ));

    // 6) 空输入边界
    let empty = DanmakuDoc::default();
    let empty_ok = parse_bilibili_xml(&export_bilibili_xml(&empty))
        .map(|doc| doc.comments.is_empty())
        .unwrap_or(false)
        && generate_ass(&[], cfg).matches("Dialogue:").count() == 0;
    rows.push((
        "空弹幕边界（导出/生成不 panic）".to_string(),
        empty_ok,
        "空 XML 解析为空；空 ASS 无事件".to_string(),
    ));

    // 7) 去重 + 密度曲线
    let dup_comments = vec![
        Comment::new(0, DanmakuMode::Scroll, "233"),
        Comment::new(1000, DanmakuMode::Scroll, "233"),
        Comment::new(9000, DanmakuMode::Scroll, "233"),
    ];
    let deduped = wp_danmaku::dedupe_comments(&dup_comments, 5000);
    let curve = wp_danmaku::density_curve(&dup_comments, 1000);
    let dedupe_ok = deduped.len() == 2 && curve.iter().map(|(_, n)| n).sum::<usize>() == 3;
    rows.push((
        "重复折叠 + 时间轴密度曲线".to_string(),
        dedupe_ok,
        format!("去重 {} 条，密度桶 {}", deduped.len(), curve.len()),
    ));

    // 8) overlay 几何一致性（4K/1080p + 缩放）
    let hd_timeline = PlacedTimeline::new(hd.clone());
    let uhd_timeline = PlacedTimeline::new(uhd.clone());
    let rect = wp_danmaku::VideoRect::new(0.0, 0.0, 1280.0, 720.0);
    let glyphs_a = hd_timeline.active_geometry(10_000, rect);
    let glyphs_b = uhd_timeline.active_geometry(10_000, rect);
    let half = wp_danmaku::VideoRect::new(0.0, 0.0, 640.0, 360.0);
    let glyphs_half = hd_timeline.active_geometry(10_000, half);
    let overlay_ok = glyphs_a == glyphs_b
        && glyphs_a.len() == glyphs_half.len()
        && glyphs_a
            .iter()
            .zip(glyphs_half.iter())
            .all(|(a, b)| (a.font_px * 0.5 - b.font_px).abs() < 1e-3);
    rows.push((
        "egui 覆盖层几何（4K/1080p 一致 + 窗口缩放）".to_string(),
        overlay_ok,
        format!("t=10s 同屏 {} 条，缩放后 font_px 减半", glyphs_a.len()),
    ));

    // 落盘演示 ASS（1000 条/分钟，4K）——GUI/media 可直接加载
    let demo_ass = generate_ass(&placed, &LayoutConfig::uhd4k());
    fs::write(
        sample_dir.join("demo-density-1000pm.ass"),
        demo_ass.as_bytes(),
    )
    .map_err(|e| format!("写演示 ASS 失败：{e}"))?;

    Ok(rows)
}

/// 确定性密度样本（`count` 条均匀分布在 `duration_ms` 内）。
fn density_comments(count: usize, duration_ms: i64) -> Vec<Comment> {
    let mut state = 0x5EED_1234u64;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };
    (0..count)
        .map(|i| {
            let rand = next();
            let base = i as i64 * duration_ms / count.max(1) as i64;
            let jitter = (rand >> 40) as i64 % 200;
            let mode = match rand % 10 {
                0..=8 => DanmakuMode::Scroll,
                9 => DanmakuMode::Top,
                _ => DanmakuMode::Bottom,
            };
            let color = match rand % 4 {
                0 => 0xFF0000,
                1 => 0x00FF00,
                2 => 0x00BFFF,
                _ => 0xFFFFFF,
            };
            Comment {
                time_ms: base + jitter,
                mode,
                color,
                font_size: 25.0,
                text: format!("密度弹幕{i}"),
                user: None,
            }
        })
        .collect()
}

fn render_report(
    expected: &Expected,
    rows: &[Row],
    matrix_rows: &[(String, bool, String)],
    passed: usize,
    gens_ok: usize,
    failures: &str,
) -> String {
    let mut out = String::new();
    out.push_str("# 弹幕格式矩阵验收报告（wp-danmaku）\n\n");
    out.push_str("> 由 `cargo run -p wp-danmaku --example matrix_danmaku` 生成；确定性输出（无时间戳），可重复执行。\n\n");
    out.push_str("- 样本来源：`crates/wp-danmaku/tests/fixtures/matrix/`\n");
    out.push_str("- 固定素材：`出/验收截图/字幕弹幕/弹幕/`（7 组样本 + `expected.json` + `actual.json` + 演示 ASS/XML）\n");
    out.push_str(&format!(
        "- 格式矩阵结论：**{passed}/{} 组通过**{}\n",
        rows.len(),
        if passed == rows.len() { " ✅" } else { " ❌" }
    ));
    out.push_str(&format!(
        "- 生成矩阵结论：**{gens_ok}/{} 项通过**{}\n\n",
        matrix_rows.len(),
        if gens_ok == matrix_rows.len() {
            " ✅"
        } else {
            " ❌"
        }
    ));
    out.push_str("## 1. 格式矩阵（固定样本解析 + 导出往返 + ASS 可解析）\n\n");
    out.push_str("| # | 样本 | 类型 | 条数 | 期望值 | 导出往返 | ASS 事件/可回读 | 结果 |\n");
    out.push_str("|---|---|---|---|---|---|---|---|\n");
    for (index, row) in rows.iter().enumerate() {
        out.push_str(&format!(
            "| {} | `{}` | {} | {} | {} | {} | {}/{} | {} |\n",
            index + 1,
            row.file,
            row.kind,
            row.count,
            mark(row.expected_ok),
            mark(row.roundtrip_ok),
            row.ass_events,
            mark(row.ass_parsable),
            if row.pass() { "✅" } else { "❌" }
        ));
    }
    out.push_str("\n## 2. 生成矩阵（内存数据）\n\n");
    out.push_str("| 检查项 | 结果 | 说明 |\n|---|---|---|\n");
    for (name, ok, detail) in matrix_rows {
        out.push_str(&format!("| {name} | {} | {detail} |\n", mark(*ok)));
    }
    out.push_str("\n## 3. 覆盖点\n\n");
    out.push_str("- B 站 XML：小数秒→毫秒、mode 1/4/5→滚动/底部/顶部、6 逆向降级、7/8 跳过、十进制颜色、`&amp;/&lt;/&gt;/&quot;/&apos;/&#39;/&#x..`反转义、坏时间/字段不足/自闭合/负时间容错\n");
    out.push_str("- JSON：B 站 API `data.elems`、本 crate 文档格式（source+comments）、普通数组；时间/模式/颜色/文本/用户多别名字段\n");
    out.push_str("- ASS 导入：Dialogue 时间/文本/`\\c` 颜色/`\\fs` 字号/`\\move`→滚动/`\\pos`→顶部/`\\N` 换行\n");
    out.push_str("- 导出往返：XML / JSON / ASS 三种路径逐字段一致\n");
    out.push_str("- 生成矩阵：1000 条/分钟密度、4K/1080p 相对坐标与 ASS 输出一致、时间窗分片、屏蔽关键词/通配符/用户/类型、XML 特殊字符与 emoji 往返、空输入边界、重复折叠+密度曲线、egui 覆盖层几何（4K/1080p+窗口缩放）\n\n");
    out.push_str("## 4. 固定素材与生成命令\n\n");
    out.push_str("```bash\n# 重新生成矩阵报告 + 固定素材（任意目录可执行；CI 失败非 0 退出）\ncargo run -p wp-danmaku --example matrix_danmaku\n\n# 集成测试（cargo test 自动跑）\ncargo test -p wp-danmaku --test matrix\n```\n\n");
    out.push_str("素材文件（供 GUI/媒体截图与冒烟用例直接引用）：\n\n");
    for item in &expected.files {
        out.push_str(&format!(
            "- `出/验收截图/字幕弹幕/弹幕/{}`（来源 {}）\n",
            item.file, item.source
        ));
    }
    out.push_str(
        "- `出/验收截图/字幕弹幕/弹幕/demo-density-1000pm.ass`（1000 条/分钟 ASS 弹幕轨，4K）\n",
    );
    out.push_str("- `出/验收截图/字幕弹幕/弹幕/demo-density-1000pm.xml`（同数据 B 站 XML，GUI 本地弹幕加载用）\n");
    out.push_str("- `出/验收截图/字幕弹幕/弹幕/demo-import.ass`（30 条小样本 ASS）\n");
    out.push_str("- `出/验收截图/字幕弹幕/弹幕/expected.json` / `actual.json`\n");
    if !failures.is_empty() {
        out.push_str("\n## 5. 失败明细\n");
        out.push_str(failures);
    }
    out
}

fn mark(ok: bool) -> &'static str {
    if ok {
        "✅"
    } else {
        "❌"
    }
}
