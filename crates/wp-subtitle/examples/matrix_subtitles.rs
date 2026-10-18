//! 字幕格式矩阵自动化验收：解析 → 期望值比对 → SRT 回读一致 → 双语 ASS 可解析，
//! 并落盘「固定素材 + 期望值 json + markdown 报告」，供 GUI/媒体集成与截图用例直接引用。
//!
//! 运行（任意目录均可，内部用 `CARGO_MANIFEST_DIR` 定位）：
//! ```text
//! cargo run -p wp-subtitle --example matrix_subtitles
//! ```
//! 产物：
//! - `出/验收截图/字幕弹幕/matrix-report.md` —— 字幕矩阵报告（本示例输出）
//! - `出/验收截图/字幕弹幕/字幕/` —— 13 组固定样本 + `expected.json` + `actual.json` + 双语 ASS 示例
//!
//! 失败会以非 0 退出码结束（CI/验收可直接用）。

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
    note: String,
    files: Vec<ExpectedFile>,
}

/// 矩阵一行结果。
struct Row {
    file: String,
    format: String,
    cues: usize,
    expected_ok: bool,
    roundtrip_ok: bool,
    bilingual_events: usize,
    bilingual_ok: bool,
    problems: Vec<String>,
}

impl Row {
    fn pass(&self) -> bool {
        self.expected_ok && self.roundtrip_ok && self.bilingual_ok
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

fn format_name(format: SubtitleFormat) -> &'static str {
    match format {
        SubtitleFormat::Srt => "Srt",
        SubtitleFormat::Vtt => "Vtt",
        SubtitleFormat::Ass => "Ass",
        SubtitleFormat::Ssa => "Ssa",
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
    if expected.files.len() < 12 {
        return Err(format!("矩阵样本不足 12 组：{}", expected.files.len()));
    }

    let out = out_dir();
    let sample_dir = out.join("字幕");
    fs::create_dir_all(&sample_dir).map_err(|e| format!("创建素材目录失败：{e}"))?;

    let opts = AssOptions::default();
    let mut rows: Vec<Row> = Vec::new();
    let mut report_note = String::new();
    for (index, item) in expected.files.iter().enumerate() {
        let fixture_path = fixtures.join(&item.file);
        let text =
            fs::read_to_string(&fixture_path).map_err(|e| format!("缺样本 {}：{e}", item.file))?;
        // 固定素材落盘（GUI/媒体截图用例直接引用）
        let copied = sample_dir.join(&item.file);
        fs::write(&copied, &text).map_err(|e| format!("写样本 {} 失败：{e}", copied.display()))?;

        let mut problems: Vec<String> = Vec::new();
        let doc = match parse_auto(Some(&item.file), &text) {
            Ok(doc) => doc,
            Err(e) => {
                problems.push(format!("解析失败：{e}"));
                SubtitleDoc::default()
            }
        };
        let format = doc.format.map_or("?", format_name);
        let cues = doc.cues.len();

        let expected_ok = verify_expected(item, &doc, &mut problems);
        let roundtrip_ok = verify_roundtrip(&doc, &mut problems);
        let (bilingual_events, bilingual_ok) = verify_bilingual(&doc, &opts, &mut problems);

        if index == 1 || index == 2 {
            // 用两组代表性样本生成双语 ASS 固定素材（GUI 可直接加载验证）
            let translations: Vec<Cue> = doc
                .cues
                .iter()
                .map(|cue| Cue {
                    text: format!("EN: {}", cue.text),
                    ..cue.clone()
                })
                .collect();
            let report = build_bilingual_ass_from_doc(&doc, &translations, &opts);
            let stem = item
                .file
                .rsplit_once('.')
                .map_or(item.file.as_str(), |(stem, _)| stem);
            let name = format!("{stem}.bilingual.ass");
            let ass_path = sample_dir.join(&name);
            fs::write(&ass_path, report.content.as_bytes())
                .map_err(|e| format!("写双语 ASS 示例失败：{e}"))?;
        }

        let row = Row {
            file: item.file.clone(),
            format: format.to_string(),
            cues,
            expected_ok,
            roundtrip_ok,
            bilingual_events,
            bilingual_ok,
            problems,
        };
        if !row.pass() {
            report_note.push_str(&format!("\n### 失败：{}\n", row.file));
            for problem in &row.problems {
                report_note.push_str(&format!("- {problem}\n"));
            }
        }
        rows.push(row);
    }

    // 期望值 json（原样复制 + 实际值 json，供 GUI/媒体做断言）
    fs::write(sample_dir.join("expected.json"), &expected_text)
        .map_err(|e| format!("写 expected.json 失败：{e}"))?;
    let actual = serde_json::json!({
        "note": "matrix_subtitles 实际值（与 expected.json 比对通过后生成）",
        "cases": rows.iter().map(|row| serde_json::json!({
            "file": row.file,
            "format": row.format,
            "cues": row.cues,
            "roundtrip_ok": row.roundtrip_ok,
            "bilingual_events": row.bilingual_events,
            "bilingual_ok": row.bilingual_ok,
            "pass": row.pass(),
        })).collect::<Vec<_>>()
    });
    fs::write(
        sample_dir.join("actual.json"),
        serde_json::to_string_pretty(&actual).map_err(|e| e.to_string())? + "\n",
    )
    .map_err(|e| format!("写 actual.json 失败：{e}"))?;

    let tool_rows = verify_tool_chain();
    let passed = rows.iter().filter(|row| row.pass()).count();
    let report = render_report(&expected, &rows, &tool_rows, passed, &report_note);
    let report_path = out.join("matrix-report.md");
    fs::write(&report_path, report.as_bytes()).map_err(|e| format!("写报告失败：{e}"))?;

    println!(
        "字幕格式矩阵：{passed}/{} 组通过；工具链 {} 项",
        rows.len(),
        tool_rows.iter().filter(|(_, ok, _)| *ok).count()
    );
    println!("报告：{}", report_path.display());
    println!(
        "固定素材：{}（{} 个文件）",
        sample_dir.display(),
        expected.files.len() + 3
    );
    if passed != rows.len() {
        return Err(format!("{} 组样本未通过", rows.len() - passed));
    }
    Ok(())
}

fn verify_expected(item: &ExpectedFile, doc: &SubtitleDoc, problems: &mut Vec<String>) -> bool {
    let mut ok = true;
    let format = doc.format.map_or("?", format_name);
    if format != item.format {
        problems.push(format!("格式 {format} != 期望 {}", item.format));
        ok = false;
    }
    if doc.cues.len() != item.cue_count {
        problems.push(format!(
            "cues {} != 期望 {}",
            doc.cues.len(),
            item.cue_count
        ));
        ok = false;
    }
    let first = doc.cues.first().map_or(0, |cue| cue.start_ms);
    let last = doc.cues.iter().map(|cue| cue.end_ms).max().unwrap_or(0);
    if first != item.first_start_ms {
        problems.push(format!("首条开始 {first} != 期望 {}", item.first_start_ms));
        ok = false;
    }
    if last != item.last_end_ms {
        problems.push(format!("末条结束 {last} != 期望 {}", item.last_end_ms));
        ok = false;
    }
    if let Some(texts) = &item.texts {
        for (index, text) in texts.iter().enumerate() {
            let actual = doc.cues.get(index).map(|cue| cue.text.as_str());
            if actual != Some(text.as_str()) {
                problems.push(format!("第 {index} 条文本不符（实际 {actual:?}）"));
                ok = false;
            }
        }
    }
    if let Some(lens) = &item.text_lens {
        for (index, len) in lens.iter().enumerate() {
            if let Some(expected_len) = len {
                let actual = doc.cues.get(index).map(|cue| cue.text.chars().count());
                if actual != Some(*expected_len) {
                    problems.push(format!(
                        "第 {index} 条长度 {actual:?} != 期望 {expected_len}"
                    ));
                    ok = false;
                }
            }
        }
    }
    if let Some(raw) = &item.raw_style_first {
        if doc.cues.first().and_then(|cue| cue.raw_style.as_deref()) != Some(raw.as_str()) {
            problems.push("首条 raw_style 不符".to_string());
            ok = false;
        }
    }
    if let Some(contains) = &item.header_contains {
        match doc.ass_header.as_deref() {
            Some(header) => {
                for needle in contains {
                    if !header.contains(needle.as_str()) {
                        problems.push(format!("ASS 头缺少 {needle:?}"));
                        ok = false;
                    }
                }
            }
            None => {
                problems.push("期望保留 ASS 头但为空".to_string());
                ok = false;
            }
        }
    }
    ok
}

fn verify_roundtrip(doc: &SubtitleDoc, problems: &mut Vec<String>) -> bool {
    let srt = build_srt(doc);
    match parse(SubtitleFormat::Srt, &srt) {
        Ok(back) => {
            if back.cues.len() != doc.cues.len() {
                problems.push(format!(
                    "回读 cues {} != {}",
                    back.cues.len(),
                    doc.cues.len()
                ));
                return false;
            }
            for (a, b) in doc.cues.iter().zip(back.cues.iter()) {
                if (a.start_ms, a.end_ms, &a.text) != (b.start_ms, b.end_ms, &b.text) {
                    problems.push("回读时间/文本与原文不一致".to_string());
                    return false;
                }
            }
            true
        }
        Err(e) => {
            problems.push(format!("回读解析失败：{e}"));
            false
        }
    }
}

fn verify_bilingual(
    doc: &SubtitleDoc,
    opts: &AssOptions,
    problems: &mut Vec<String>,
) -> (usize, bool) {
    let translations: Vec<Cue> = doc
        .cues
        .iter()
        .map(|cue| Cue {
            text: format!("EN: {}", cue.text),
            ..cue.clone()
        })
        .collect();
    let report = build_bilingual_ass_from_doc(doc, &translations, opts);
    match parse(SubtitleFormat::Ass, &report.content) {
        Ok(back) => {
            let non_empty = doc
                .cues
                .iter()
                .filter(|cue| !cue.text.trim().is_empty())
                .count();
            let expected_events = non_empty + doc.cues.len();
            let mut ok = true;
            if back.cues.len() != expected_events {
                problems.push(format!(
                    "双语 ASS 事件 {} != 期望 {}",
                    back.cues.len(),
                    expected_events
                ));
                ok = false;
            }
            if report.missing != 0 || report.matched != doc.cues.len() {
                problems.push(format!(
                    "双语配对 matched={} missing={}",
                    report.matched, report.missing
                ));
                ok = false;
            }
            if let (Some(original), Some(translated)) = (back.cues.first(), back.cues.get(1)) {
                if original.start_ms != translated.start_ms || original.end_ms != translated.end_ms
                {
                    problems.push("双语 ASS 原文/译文时间轴不一致".to_string());
                    ok = false;
                }
            }
            (back.cues.len(), ok)
        }
        Err(e) => {
            problems.push(format!("双语 ASS 不可解析：{e}"));
            (0, false)
        }
    }
}

/// 工具链检查（时间互转/负时间裁剪/合并/统计），返回 (名称, 是否通过, 说明)。
fn verify_tool_chain() -> Vec<(String, bool, String)> {
    let mut rows = Vec::new();

    let ms = wp_subtitle::parse_srt_timestamp("01:59:59,990").unwrap_or(-1);
    let ass = wp_subtitle::format_ass_timestamp(ms);
    let back = wp_subtitle::parse_ass_timestamp(&ass);
    // ASS 时间只有百分秒精度 → 允许 <=5ms 四舍五入误差
    let time_ok = ms == 7_199_990 && back.is_some_and(|back| (back - ms).abs() <= 5);
    rows.push((
        "SRT↔ASS 时间互转（跨小时，百分秒精度）".to_string(),
        time_ok,
        format!("01:59:59,990 → {ass} → {back:?}"),
    ));

    let mut doc = SubtitleDoc::from_cues(vec![
        Cue {
            start_ms: 100,
            end_ms: 200,
            text: "before".into(),
            raw_style: None,
        },
        Cue {
            start_ms: 500,
            end_ms: 900,
            text: "cross".into(),
            raw_style: None,
        },
    ]);
    wp_subtitle::offset_ms(&mut doc, -600);
    let clamp_ok = doc.cues.len() == 1 && doc.cues[0].start_ms == 0 && doc.cues[0].end_ms == 300;
    rows.push((
        "时间轴偏移 + 负时间裁剪".to_string(),
        clamp_ok,
        format!(
            "剩余 {} 条，首条 {}-{}ms",
            doc.cues.len(),
            doc.cues.first().map_or(0, |c| c.start_ms),
            doc.cues.first().map_or(0, |c| c.end_ms)
        ),
    ));

    let merged = wp_subtitle::merge_cues(&doc.cues, 500);
    let stats = wp_subtitle::stats(&merged);
    let tracker = wp_subtitle::CueTracker::new(merged.clone());
    let chain_ok = merged.len() == 1
        && stats.count == 1
        && stats.total_duration_ms == 300
        && tracker.current(150).is_some();
    rows.push((
        "cue 合并/统计/追踪".to_string(),
        chain_ok,
        format!(
            "合并 {} 条，总时长 {}ms，current(150ms)={}",
            merged.len(),
            stats.total_duration_ms,
            tracker.current(150).is_some()
        ),
    ));

    let vtt_guess = wp_subtitle::guess_format_content("WEBVTT\n\n00:01.000 --> 00:02.000\nx");
    rows.push((
        "内容嗅探（WEBVTT）".to_string(),
        vtt_guess == Some(SubtitleFormat::Vtt),
        format!("guess_format_content → {vtt_guess:?}"),
    ));
    rows
}

fn render_report(
    expected: &Expected,
    rows: &[Row],
    tool_rows: &[(String, bool, String)],
    passed: usize,
    failures: &str,
) -> String {
    let mut out = String::new();
    out.push_str("# 字幕格式矩阵验收报告（wp-subtitle）\n\n");
    out.push_str("> 由 `cargo run -p wp-subtitle --example matrix_subtitles` 生成；确定性输出（无时间戳），可重复执行。\n\n");
    out.push_str(&format!(
        "- 样本来源：`{}`\n",
        "crates/wp-subtitle/tests/fixtures/matrix/"
    ));
    out.push_str("- 固定素材：`出/验收截图/字幕弹幕/字幕/`（13 组样本 + `expected.json` + `actual.json` + 双语 ASS 示例）\n");
    out.push_str(&format!(
        "- 矩阵结论：**{passed}/{} 组通过**{}\n",
        rows.len(),
        if passed == rows.len() { " ✅" } else { " ❌" }
    ));
    out.push_str(&format!(
        "- 期望值：{}（{} 组，人工编写）\n\n",
        expected.note,
        expected.files.len()
    ));
    out.push_str("## 1. 格式矩阵\n\n");
    out.push_str(
        "| # | 样本 | 格式 | cues | 期望值 | SRT 回读一致 | 双语 ASS 事件 | 双语可解析 | 结果 |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|---|---|\n");
    for (index, row) in rows.iter().enumerate() {
        out.push_str(&format!(
            "| {} | `{}` | {} | {} | {} | {} | {} | {} | {} |\n",
            index + 1,
            row.file,
            row.format,
            row.cues,
            mark(row.expected_ok),
            mark(row.roundtrip_ok),
            row.bilingual_events,
            mark(row.bilingual_ok),
            if row.pass() { "✅" } else { "❌" }
        ));
    }
    out.push_str("\n## 2. 工具链检查\n\n");
    out.push_str("| 检查项 | 结果 | 说明 |\n|---|---|---|\n");
    for (name, ok, detail) in tool_rows {
        out.push_str(&format!("| {name} | {} | {detail} |\n", mark(*ok)));
    }
    out.push_str("\n## 3. 覆盖点\n\n");
    out.push_str("- SRT：基础/多行/HTML 标签/ASS 样式前缀/坏行容错/负时间裁剪/跨小时与超长时长/CRLF+BOM+坐标设置\n");
    out.push_str("- WebVTT：WEBVTT 头/NOTE/STYLE/REGION/short MM:SS/cue id/cue settings/`<v>`、`<c>` 标签/实体\n");
    out.push_str("- ASS：全字段样式/字体段保留/特效标签与 `\\N`/逗号文本/自定义 Format 字段顺序/坏行跳过/缺字段跳过\n");
    out.push_str("- SSA：ScriptType v4.00/V4 Styles/Marked=0 旧格式/样式字段转换\n");
    out.push_str("- 中文/日文/emoji/超长行（280 字符）/空文件\n\n");
    out.push_str("## 4. 固定素材与生成命令\n\n");
    out.push_str("```bash\n# 重新生成矩阵报告 + 固定素材（任意目录可执行；CI 失败非 0 退出）\ncargo run -p wp-subtitle --example matrix_subtitles\n\n# 集成测试（cargo test 自动跑）\ncargo test -p wp-subtitle --test matrix\n```\n\n");
    out.push_str("素材文件（供 GUI/媒体截图与冒烟用例直接引用）：\n\n");
    for item in &expected.files {
        out.push_str(&format!(
            "- `出/验收截图/字幕弹幕/字幕/{}`（{} 组）\n",
            item.file, item.format
        ));
    }
    out.push_str("- `出/验收截图/字幕弹幕/字幕/expected.json`（期望值）\n");
    out.push_str("- `出/验收截图/字幕弹幕/字幕/actual.json`（本次实际值）\n");
    out.push_str(
        "- `出/验收截图/字幕弹幕/字幕/*.bilingual.ass`（双语 ASS 示例，可 `sub-add` 验证）\n",
    );
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
