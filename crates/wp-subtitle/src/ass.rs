//! ASS / SSA 解析与生成（含双语字幕生成）。
//!
//! 解析：`[Script Info]` / 样式段 / `[Events]` 的 `Format:` 字段顺序识别、
//! `Dialogue:` 字段解析、ASS 转义换行（`\N`）、文本开头 override 块保留为 `Cue::raw_style`。
//!
//! 生成：
//! - [`build_bilingual_ass`]：原文 + 译文双样式双轨（保留原 ASS 头/样式/字体段），
//!   支持 [`BilingualLayout`] 三种布局、`MarginV`/字号/颜色、与原文时间轴严格一致；
//! - [`build_ass_from_cues`]：单语 ASS（SRT/VTT 转 ASS 用）。
//!
//! 颜色约定：ASS 是 `&HAABBGGRR`；工具函数 [`ass_color_from_rgb`] / [`normalize_ass_color`]
//! 与 [`parse_ass_color_rgb`] 负责与 `0xRRGGBB` 互转。

use crate::model::{AssOptions, BilingualAss, BilingualLayout, Cue, SubtitleDoc, SubtitleFormat};
use crate::text::{escape_ass_text, normalize_newlines, split_leading_override, strip_effects};
use crate::time::{format_ass_timestamp, parse_ass_timestamp};
use serde::{Deserialize, Serialize};
use wp_core::Result;

/// ASS 样式段里的一条原始 `Style:` 行。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssStyle {
    pub name: String,
    /// 原始 `Style: ...` 行（保留字段顺序）。
    pub line: String,
}

/// ASS `[Events]` 里的一条 `Dialogue:`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssEvent {
    pub start_ms: i64,
    pub end_ms: i64,
    /// 样式名（如 `Default`）。
    pub style: String,
    /// 说话人/角色名。
    pub name: String,
    /// 特效字段（如 `Banner;5;1;80`）。
    pub effect: String,
    /// 去特效后的纯文本（`\N` 已还原为换行）。
    pub text: String,
    /// 文本开头的 override 块（`{\an8}{\pos(..)}`）。
    pub raw_style: Option<String>,
    /// 原始 Text 字段（未清洗，含标签与转义）。
    pub raw_text: String,
}

/// 解析后的 ASS 文档（保留头 + 样式 + 事件，供双语生成/往返）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssDocument {
    /// 首个 `Dialogue:` 之前的全部内容（含 `[Events]` 的 `Format:` 行）。
    pub header: String,
    pub styles: Vec<AssStyle>,
    pub events: Vec<AssEvent>,
}

impl AssDocument {
    /// 事件 → cue。
    pub fn to_cues(&self) -> Vec<Cue> {
        self.events
            .iter()
            .filter(|e| e.end_ms >= e.start_ms)
            .map(|e| Cue {
                start_ms: e.start_ms.max(0),
                end_ms: e.end_ms.max(0),
                text: e.text.clone(),
                raw_style: e.raw_style.clone(),
            })
            .collect()
    }

    /// 转成 `SubtitleDoc`（`ass_header` 保留）。
    pub fn to_doc(&self, format: SubtitleFormat) -> SubtitleDoc {
        SubtitleDoc {
            format: Some(format),
            cues: self.to_cues(),
            ass_header: if self.header.trim().is_empty() {
                None
            } else {
                Some(self.header.clone())
            },
        }
    }

    /// 按名字找样式（大小写不敏感）。
    pub fn style_named(&self, name: &str) -> Option<&AssStyle> {
        self.styles
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
    }
}

// ---------------------------------------------------------------------------
// 颜色工具
// ---------------------------------------------------------------------------

/// `0xRRGGBB` → ASS 颜色 `&H00BBGGRR`。
pub fn ass_color_from_rgb(rgb: u32) -> String {
    let r = (rgb >> 16) & 0xFF;
    let g = (rgb >> 8) & 0xFF;
    let b = rgb & 0xFF;
    format!("&H00{b:02X}{g:02X}{r:02X}")
}

/// RGBA 分量 → ASS 颜色 `&HAABBGGRR`（`a` 为 ASS 语义的透明度，0 不透明）。
pub fn ass_color_from_rgba(r: u8, g: u8, b: u8, a: u8) -> String {
    format!("&H{a:02X}{b:02X}{g:02X}{r:02X}")
}

/// ASS 颜色（`&HAABBGGRR` / `&HBBGGRR` / `#RRGGBB` / 6 位 hex）→ `0xRRGGBB`。
pub fn parse_ass_color_rgb(input: &str) -> Option<u32> {
    let s = input.trim();
    let lower = s.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("&h") {
        let body: String = rest
            .trim()
            .trim_end_matches('&')
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .collect();
        return match body.len() {
            6 => rgb_from_bgr_hex(&body),
            8 => rgb_from_bgr_hex(&body[2..]),
            _ => None,
        };
    }
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return u32::from_str_radix(hex, 16).ok();
    }
    None
}

fn rgb_from_bgr_hex(hex: &str) -> Option<u32> {
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let b = u32::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u32::from_str_radix(&hex[2..4], 16).ok()?;
    let r = u32::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r << 16) | (g << 8) | b)
}

/// 归一化颜色字符串为合法 ASS 颜色（`&HAABBGGRR`）。
///
/// 接受 `&H...`（原样大写、6 位补 AA）、`#RRGGBB`、`0xRRGGBB`、`RRGGBB`；
/// 无法识别时退回白色。
pub fn normalize_ass_color(input: &str) -> String {
    let s = input.trim();
    if s.is_empty() {
        return "&H00FFFFFF".to_string();
    }
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("&h") {
        let body: String = lower
            .trim_start_matches("&h")
            .trim_end_matches('&')
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .collect();
        return match body.len() {
            6 => format!("&H00{}", body.to_ascii_uppercase()),
            8 => format!("&H{}", body.to_ascii_uppercase()),
            _ => "&H00FFFFFF".to_string(),
        };
    }
    if let Some(rgb) = parse_ass_color_rgb(s) {
        return ass_color_from_rgb(rgb);
    }
    "&H00FFFFFF".to_string()
}

// ---------------------------------------------------------------------------
// 解析
// ---------------------------------------------------------------------------

const DEFAULT_EVENT_FIELDS: [&str; 10] = [
    "Layer", "Start", "End", "Style", "Name", "MarginL", "MarginR", "MarginV", "Effect", "Text",
];

const DEFAULT_STYLE_FIELDS: [&str; 23] = [
    "Name",
    "Fontname",
    "Fontsize",
    "PrimaryColour",
    "SecondaryColour",
    "OutlineColour",
    "BackColour",
    "Bold",
    "Italic",
    "Underline",
    "StrikeOut",
    "ScaleX",
    "ScaleY",
    "Spacing",
    "Angle",
    "BorderStyle",
    "Outline",
    "Shadow",
    "Alignment",
    "MarginL",
    "MarginR",
    "MarginV",
    "Encoding",
];

/// 各样式字段的兜底默认值（SSA V4 样式转 V4+ 时补缺字段用）。
const STYLE_FIELD_DEFAULTS: [&str; 23] = [
    "Default",
    "Arial",
    "48",
    "&H00FFFFFF",
    "&H000000FF",
    "&H00000000",
    "&H00000000",
    "0",
    "0",
    "0",
    "0",
    "100",
    "100",
    "0",
    "0",
    "1",
    "1",
    "0",
    "2",
    "10",
    "10",
    "40",
    "1",
];

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .map(|head| head.eq_ignore_ascii_case(prefix))
        .unwrap_or(false)
}

fn section_header_name(line: &str) -> Option<&str> {
    let t = line.trim();
    if t.starts_with('[') && t.ends_with(']') && t.len() >= 2 {
        Some(t)
    } else {
        None
    }
}

/// 解析 `Format: A, B, C` 为字段名列表（小写？保留原样，比较时忽略大小写）。
fn parse_field_list(line: &str) -> Vec<String> {
    let after = line.split_once(':').map(|(_, r)| r).unwrap_or("");
    after
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 解析 ASS/SSA 全文（宽松：坏行跳过）。
pub fn parse_ass_document(text: &str) -> AssDocument {
    let normalized = normalize_newlines(text);
    let mut header_lines: Vec<String> = Vec::new();
    let mut styles: Vec<AssStyle> = Vec::new();
    let mut events: Vec<AssEvent> = Vec::new();
    let mut section = String::new();
    let mut events_fields: Option<Vec<String>> = None;
    let mut found_first_dialogue = false;

    for line in normalized.lines() {
        let trimmed = line.trim();
        let is_dialogue = starts_with_ci(trimmed, "Dialogue:");
        if is_dialogue {
            found_first_dialogue = true;
            if section.eq_ignore_ascii_case("[events]") {
                if let Some(event) = parse_dialogue_line(trimmed, events_fields.as_deref()) {
                    events.push(event);
                }
            }
            continue;
        }
        if let Some(name) = section_header_name(trimmed) {
            section = name.to_ascii_lowercase();
            if !found_first_dialogue {
                header_lines.push(line.to_string());
            }
            continue;
        }
        if !found_first_dialogue {
            header_lines.push(line.to_string());
            match section.as_str() {
                "[events]" => {
                    if starts_with_ci(trimmed, "Format:") {
                        events_fields = Some(parse_field_list(trimmed));
                    }
                }
                "[v4+ styles]" | "[v4 styles]" => {
                    if starts_with_ci(trimmed, "Style:") {
                        let name = style_name_of(trimmed).to_string();
                        styles.push(AssStyle {
                            name,
                            line: trimmed.to_string(),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    let header = {
        let joined = header_lines.join("\n");
        let trimmed_end = joined.trim_end().to_string();
        if trimmed_end.is_empty() {
            String::new()
        } else {
            format!("{trimmed_end}\n")
        }
    };
    AssDocument {
        header,
        styles,
        events,
    }
}

fn style_name_of(style_line: &str) -> &str {
    style_line
        .split_once(':')
        .map(|(_, rest)| rest)
        .unwrap_or("")
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
}

fn parse_dialogue_line(line: &str, format: Option<&[String]>) -> Option<AssEvent> {
    let rest = line.split_once(':').map(|(_, r)| r.trim_start())?;
    let mut parts: Vec<&str> = rest.splitn(format.map_or(10, |f| f.len()), ',').collect();
    // 旧版 SSA 兜底：`Dialogue: Marked=0,0:00:01.00,...`（无 Format 行时）
    let ssa_marked = format.is_none()
        && parts
            .first()
            .map(|p| p.trim().to_ascii_lowercase().starts_with("marked="))
            .unwrap_or(false);
    if ssa_marked {
        parts.remove(0);
    }
    let field_names: Vec<String> = if ssa_marked {
        // 去掉 Marked 字段后，字段名整体前移一位（Start 起）
        DEFAULT_EVENT_FIELDS[1..]
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        match format {
            Some(f) => f.to_vec(),
            None => DEFAULT_EVENT_FIELDS.iter().map(|s| s.to_string()).collect(),
        }
    };
    let get = |name: &str| -> Option<&str> {
        field_names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(name))
            .and_then(|i| parts.get(i).copied())
    };
    // `splitn` 后 Text 字段是最后一个，字段不足时视为坏行
    if parts.len() < field_names.len() {
        return None;
    }
    let start_ms = parse_ass_timestamp(get("Start")?.trim())?;
    let end_ms = parse_ass_timestamp(get("End")?.trim())?;
    let style = get("Style").unwrap_or("Default").trim().to_string();
    let name = get("Name").unwrap_or("").trim().to_string();
    let effect = get("Effect").unwrap_or("").trim().to_string();
    let raw_text = get("Text").unwrap_or("").to_string();
    let (raw_style, rest_text) = split_leading_override(&raw_text);
    let text = strip_effects(&rest_text).trim().to_string();
    Some(AssEvent {
        start_ms,
        end_ms,
        style,
        name,
        effect,
        text,
        raw_style,
        raw_text,
    })
}

/// 解析 ASS/SSA 文本为 `SubtitleDoc`。
pub fn parse_ass(text: &str) -> Result<SubtitleDoc> {
    let doc = parse_ass_document(text);
    let format = detect_ssa_format(text);
    Ok(doc.to_doc(format))
}

fn detect_ssa_format(text: &str) -> SubtitleFormat {
    for line in normalize_newlines(text).lines() {
        let t = line.trim();
        if starts_with_ci(t, "ScriptType:") {
            let value = t[11..].trim().to_ascii_lowercase();
            return if value.starts_with("v4.00") && !value.starts_with("v4.00+") {
                SubtitleFormat::Ssa
            } else {
                SubtitleFormat::Ass
            };
        }
        if t.starts_with('[') && !t.eq_ignore_ascii_case("[script info]") {
            break;
        }
    }
    SubtitleFormat::Ass
}

// ---------------------------------------------------------------------------
// 双语 ASS 生成
// ---------------------------------------------------------------------------

/// 生成双语 ASS 并返回配对统计（无原 ASS 头，使用默认样式表）。
///
/// 这是 T11「AI 字幕注入」的主入口：翻译结果按时间轴配对 → 双轨 ASS → 写临时文件 →
/// `sub-add`/`sub-reload`。
pub fn build_bilingual_ass_report(
    original: &[Cue],
    translated: &[Cue],
    opts: &AssOptions,
) -> BilingualAss {
    build_bilingual_ass_report_with_header(original, translated, opts, None)
}

/// 生成双语 ASS（可保留原 ASS 头/样式/字体段）。
pub fn build_bilingual_ass_report_with_header(
    original: &[Cue],
    translated: &[Cue],
    opts: &AssOptions,
    header: Option<&str>,
) -> BilingualAss {
    let opts = opts.sanitized();
    let line_h = (opts.font_size as f32 * 1.25).round() as i32;
    let (original_margin, translated_margin) = match opts.layout {
        BilingualLayout::TranslatedBelow => (opts.margin_v + line_h, opts.margin_v),
        BilingualLayout::TranslatedAbove => (opts.margin_v, opts.margin_v + line_h),
        BilingualLayout::OnlyTranslated => (opts.margin_v, opts.margin_v),
    };
    let specs = [
        StyleSpec {
            name: &opts.original_style,
            margin_v: original_margin,
            preserve_original: true,
        },
        StyleSpec {
            name: &opts.translated_style,
            margin_v: translated_margin,
            preserve_original: false,
        },
    ];
    let mut content = build_header(header, &opts, &specs);
    let pairs = align_translations(original, translated);
    let mut matched = 0usize;
    let mut missing = 0usize;
    for (cue, pair) in original.iter().zip(pairs.iter()) {
        let cue = cue.normalized();
        let show_original = opts.layout != BilingualLayout::OnlyTranslated && !cue.is_empty_text();
        if show_original {
            content.push_str(&format_dialogue(
                &opts.original_style,
                cue.start_ms,
                cue.end_ms,
                cue.raw_style.as_deref(),
                &cue.text,
            ));
            content.push('\n');
        }
        match pair {
            Some(trans) if !trans.text.trim().is_empty() => {
                matched += 1;
                content.push_str(&format_dialogue(
                    &opts.translated_style,
                    cue.start_ms,
                    cue.end_ms,
                    None,
                    trans.text.trim(),
                ));
                content.push('\n');
            }
            _ => missing += 1,
        }
    }
    BilingualAss {
        content,
        matched,
        missing,
    }
}

/// 生成单语 ASS（原文轨；SRT/VTT → ASS 转换用）。
pub fn build_ass_from_cues(cues: &[Cue], opts: &AssOptions, header: Option<&str>) -> String {
    let opts = opts.sanitized();
    let specs = [
        StyleSpec {
            name: &opts.original_style,
            margin_v: opts.margin_v,
            preserve_original: true,
        },
        StyleSpec {
            name: &opts.translated_style,
            margin_v: opts.margin_v,
            preserve_original: false,
        },
    ];
    let mut content = build_header(header, &opts, &specs);
    let mut sorted: Vec<Cue> = cues.iter().map(|c| c.normalized()).collect();
    sorted.sort_by_key(|c| (c.start_ms, c.end_ms));
    for cue in &sorted {
        if cue.is_empty_text() {
            continue;
        }
        content.push_str(&format_dialogue(
            &opts.original_style,
            cue.start_ms,
            cue.end_ms,
            cue.raw_style.as_deref(),
            &cue.text,
        ));
        content.push('\n');
    }
    content
}

/// 原文/译文配对：条数相同按序号；不同则按时间重叠度贪心匹配。
pub fn align_translations<'a>(original: &[Cue], translated: &'a [Cue]) -> Vec<Option<&'a Cue>> {
    if original.is_empty() || translated.is_empty() {
        return vec![None; original.len()];
    }
    if original.len() == translated.len() {
        // AI 翻译管线通常严格保持时间轴与条数，直接按序号配对
        return translated.iter().map(Some).collect();
    }
    original
        .iter()
        .map(|o| {
            let mut best: Option<(&Cue, i64)> = None;
            for t in translated {
                let overlap = overlap_ms(o, t);
                if overlap <= 0 {
                    continue;
                }
                let start_diff = (o.start_ms - t.start_ms).abs();
                let end_diff = (o.end_ms - t.end_ms).abs();
                let score = overlap
                    .saturating_mul(2)
                    .saturating_sub(start_diff)
                    .saturating_sub(end_diff);
                if best.map_or(true, |(_, s)| score > s) {
                    best = Some((t, score));
                }
            }
            best.map(|(t, _)| t)
        })
        .collect()
}

fn overlap_ms(a: &Cue, b: &Cue) -> i64 {
    a.end_ms.min(b.end_ms) - a.start_ms.max(b.start_ms)
}

fn format_dialogue(
    style: &str,
    start_ms: i64,
    end_ms: i64,
    raw_style: Option<&str>,
    text: &str,
) -> String {
    let mut body = String::new();
    if let Some(raw) = raw_style {
        if !raw.trim().is_empty() {
            body.push_str(raw.trim());
        }
    }
    body.push_str(&escape_ass_text(text));
    format!(
        "Dialogue: 0,{},{},{},,0,0,0,,{}",
        format_ass_timestamp(start_ms),
        format_ass_timestamp(end_ms),
        style,
        body
    )
}

/// 一条待生成样式的描述（双语双轨 / 单轨共用）。
#[derive(Debug, Clone, Copy)]
struct StyleSpec<'a> {
    name: &'a str,
    margin_v: i32,
    /// 原 ASS 头里有同名样式时：保留其字体/字号/颜色，只覆盖布局字段
    preserve_original: bool,
}

fn default_style_fields(opts: &AssOptions, name: &str, margin_v: i32) -> Vec<String> {
    let primary = normalize_ass_color(&opts.primary_color);
    let outline = normalize_ass_color(&opts.outline_color);
    let font = opts.font_name.replace(',', " ");
    vec![
        name.to_string(),
        font,
        opts.font_size.to_string(),
        primary,
        "&H000000FF".to_string(),
        outline,
        "&H00000000".to_string(),
        "0".to_string(),
        "0".to_string(),
        "0".to_string(),
        "0".to_string(),
        "100".to_string(),
        "100".to_string(),
        "0".to_string(),
        "0".to_string(),
        "1".to_string(),
        "1".to_string(),
        "0".to_string(),
        "2".to_string(),
        "10".to_string(),
        "10".to_string(),
        margin_v.to_string(),
        "1".to_string(),
    ]
}

fn style_line_from_fields(fields: &[String]) -> String {
    format!("Style: {}", fields.join(","))
}

/// 用原样式字段做底，覆盖样式名/对齐（底部居中）/MarginV。
fn merged_style_fields(original_reordered: &str, name: &str, margin_v: i32) -> Option<Vec<String>> {
    let rest = original_reordered.split_once(':').map(|(_, r)| r.trim())?;
    let mut fields: Vec<String> = rest.split(',').map(|s| s.trim().to_string()).collect();
    if fields.len() != DEFAULT_STYLE_FIELDS.len() {
        return None;
    }
    fields[0] = name.to_string();
    fields[18] = "2".to_string(); // Alignment：底部居中，MarginV 布局才生效
    fields[21] = margin_v.to_string();
    Some(fields)
}

// ---------------------------------------------------------------------------
// 头部重建（保留原样式/字体段）
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct HeaderParts {
    script_info: Vec<String>,
    style_fields: Option<Vec<String>>,
    style_lines: Vec<String>,
    other_sections: Vec<(String, Vec<String>)>,
}

fn parse_header(header: &str) -> HeaderParts {
    let mut parts = HeaderParts::default();
    let mut section = String::new();
    let mut current_other: Option<usize> = None;
    for line in normalize_newlines(header).lines() {
        let trimmed = line.trim_end();
        if let Some(name) = section_header_name(trimmed) {
            current_other = None;
            let lower = name.to_ascii_lowercase();
            match lower.as_str() {
                "[script info]" => section = "[script info]".to_string(),
                "[v4+ styles]" => section = "[v4+ styles]".to_string(),
                "[v4 styles]" => section = "[v4 styles]".to_string(),
                "[events]" => section = "[events]".to_string(),
                _ => {
                    parts.other_sections.push((name.to_string(), Vec::new()));
                    current_other = Some(parts.other_sections.len() - 1);
                    section = lower;
                }
            }
            continue;
        }
        let s = trimmed.to_string();
        match section.as_str() {
            "" | "[script info]" => parts.script_info.push(s),
            "[v4+ styles]" | "[v4 styles]" => {
                if starts_with_ci(trimmed, "Format:") {
                    parts.style_fields = Some(parse_field_list(trimmed));
                } else if starts_with_ci(trimmed, "Style:") {
                    parts.style_lines.push(trimmed.to_string());
                }
            }
            // 事件段的 Format/Comment 由生成逻辑重建，不保留
            "[events]" => {}
            _ => {
                if let Some(i) = current_other {
                    parts.other_sections[i].1.push(s);
                }
            }
        }
    }
    parts
}

fn default_script_info_lines() -> Vec<String> {
    vec![
        "ScriptType: v4.00+".to_string(),
        "PlayResX: 1920".to_string(),
        "PlayResY: 1080".to_string(),
        "WrapStyle: 2".to_string(),
        "ScaledBorderAndShadow: yes".to_string(),
        "YCbCr Matrix: TV.709".to_string(),
    ]
}

fn build_header(header: Option<&str>, opts: &AssOptions, specs: &[StyleSpec]) -> String {
    let parts = header.map(parse_header).unwrap_or_default();
    let wanted_names: Vec<String> = specs.iter().map(|s| s.name.to_ascii_lowercase()).collect();
    // 原样式统一重排为 V4+ 字段顺序，按小写名索引
    let mut original_styles: Vec<(String, String)> = Vec::new();
    for line in &parts.style_lines {
        if let Some(reordered) = reorder_style_line(line, parts.style_fields.as_deref()) {
            original_styles.push((style_name_of(&reordered).to_ascii_lowercase(), reordered));
        }
    }
    let mut out = String::with_capacity(4096);
    out.push_str("[Script Info]\n");
    let mut script_info = parts.script_info.clone();
    if script_info.iter().all(|l| l.trim().is_empty()) {
        script_info = default_script_info_lines();
    }
    // ScriptType 统一升级为 v4.00+（SSA 头也要能装 [V4+ Styles]）
    let mut has_script_type = false;
    for line in &mut script_info {
        if starts_with_ci(line.trim(), "ScriptType:") {
            *line = "ScriptType: v4.00+".to_string();
            has_script_type = true;
        }
    }
    if !has_script_type {
        script_info.insert(0, "ScriptType: v4.00+".to_string());
    }
    let has_playres_x = script_info
        .iter()
        .any(|l| starts_with_ci(l.trim(), "PlayResX:"));
    let has_playres_y = script_info
        .iter()
        .any(|l| starts_with_ci(l.trim(), "PlayResY:"));
    if !has_playres_x {
        script_info.push("PlayResX: 1920".to_string());
    }
    if !has_playres_y {
        script_info.push("PlayResY: 1080".to_string());
    }
    for line in &script_info {
        if !line.trim().is_empty() {
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    out.push('\n');

    // [V4+ Styles]：原样式（统一重排为 V4+ 字段顺序）+ 我们的双轨样式
    out.push_str("[V4+ Styles]\n");
    out.push_str(&format!("Format: {}\n", DEFAULT_STYLE_FIELDS.join(", ")));
    for (lower, line) in &original_styles {
        if wanted_names.contains(lower) {
            continue; // 同名样式由我们的定义（保留原字段+布局覆盖）接管
        }
        out.push_str(line);
        out.push('\n');
    }
    for spec in specs {
        let original = original_styles
            .iter()
            .find(|(lower, _)| lower == &spec.name.to_ascii_lowercase())
            .map(|(_, line)| line.as_str());
        let fields = match original {
            Some(line) if spec.preserve_original => {
                merged_style_fields(line, spec.name, spec.margin_v)
                    .unwrap_or_else(|| default_style_fields(opts, spec.name, spec.margin_v))
            }
            _ => default_style_fields(opts, spec.name, spec.margin_v),
        };
        out.push_str(&style_line_from_fields(&fields));
        out.push('\n');
    }
    out.push('\n');

    // 其它段原样保留（[Fonts] / [Graphics] / [Aegisub Project Garbage] ...）
    for (name, lines) in &parts.other_sections {
        out.push_str(name.trim());
        out.push('\n');
        for line in lines {
            if !line.trim().is_empty() {
                out.push_str(line.trim_end());
                out.push('\n');
            }
        }
        out.push('\n');
    }

    out.push_str("[Events]\n");
    out.push_str(&format!("Format: {}\n\n", DEFAULT_EVENT_FIELDS.join(", ")));
    out
}

/// 把任意字段顺序的 `Style:` 行重排为标准 V4+ 顺序（V4/SSA 样式补默认值）。
fn reorder_style_line(line: &str, format: Option<&[String]>) -> Option<String> {
    if !starts_with_ci(line, "Style:") {
        return None;
    }
    let rest = line.split_once(':').map(|(_, r)| r.trim()).unwrap_or("");
    let fields = match format {
        Some(f) => f,
        None => return Some(format!("Style: {rest}")),
    };
    let values: Vec<&str> = rest.splitn(fields.len(), ',').collect();
    if values.len() < fields.len() {
        return None;
    }
    let mut out: Vec<String> = Vec::with_capacity(DEFAULT_STYLE_FIELDS.len());
    for (i, def) in DEFAULT_STYLE_FIELDS.iter().enumerate() {
        let idx = fields
            .iter()
            .position(|f| f.eq_ignore_ascii_case(def))
            .or_else(|| {
                // SSA V4 兼容：TertiaryColour 充当 OutlineColour
                if def.eq_ignore_ascii_case("OutlineColour") {
                    fields
                        .iter()
                        .position(|f| f.eq_ignore_ascii_case("TertiaryColour"))
                } else {
                    None
                }
            });
        match idx.and_then(|i| values.get(i)) {
            Some(v) => out.push(v.trim().to_string()),
            None => out.push(STYLE_FIELD_DEFAULTS[i].to_string()),
        }
    }
    Some(format!("Style: {}", out.join(",")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(start: i64, end: i64, text: &str) -> Cue {
        Cue {
            start_ms: start,
            end_ms: end,
            text: text.into(),
            raw_style: None,
        }
    }

    const ASS_SAMPLE: &str = "[Script Info]\n\
ScriptType: v4.00+\n\
PlayResX: 1920\n\
PlayResY: 1080\n\
\n\
[V4+ Styles]\n\
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
Style: CN,Noto Sans CJK SC,52,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,48,1\n\
\n\
[Events]\n\
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
Dialogue: 0,0:00:01.00,0:00:03.50,CN,,0,0,0,,{\\an8}第一句\\N换行\n\
Dialogue: 0,0:00:04.00,0:00:06.00,CN,,0,0,0,,第二句\n";

    #[test]
    fn parse_ass_preserves_header_styles_and_events() {
        let doc = parse_ass_document(ASS_SAMPLE);
        assert_eq!(doc.events.len(), 2);
        assert_eq!(doc.styles.len(), 1);
        assert_eq!(doc.styles[0].name, "CN");
        assert!(doc.header.contains("[V4+ Styles]"));
        assert!(doc.header.contains("Style: CN,"));
        let cues = doc.to_cues();
        assert_eq!(cues[0].start_ms, 1000);
        assert_eq!(cues[0].end_ms, 3500);
        assert_eq!(cues[0].raw_style.as_deref(), Some("{\\an8}"));
        assert_eq!(cues[0].text, "第一句\n换行");
        assert_eq!(cues[1].text, "第二句");
    }

    #[test]
    fn parse_ass_ssa_marked_fallback() {
        let ssa = "[Script Info]\nScriptType: v4.00\n\n[Events]\n\
Dialogue: Marked=0,0:00:02.00,0:00:04.00,Default,,0000,0000,0000,,SSA文本\n";
        let doc = parse_ass_document(ssa);
        assert_eq!(doc.events.len(), 1);
        assert_eq!(doc.events[0].start_ms, 2000);
        assert_eq!(doc.events[0].end_ms, 4000);
        assert_eq!(doc.events[0].text, "SSA文本");
        let subtitle = parse_ass(ssa).unwrap();
        assert_eq!(subtitle.format, Some(SubtitleFormat::Ssa));
        assert_eq!(subtitle.cues.len(), 1);
    }

    #[test]
    fn parse_ass_bad_lines_skipped() {
        let text = "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
Dialogue: 0,bad-time,0:00:02.00,Default,,0,0,0,,坏\n\
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,好\n\
Comment: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,注释不算\n";
        let doc = parse_ass_document(text);
        assert_eq!(doc.events.len(), 1);
        assert_eq!(doc.events[0].text, "好");
    }

    #[test]
    fn colors_convert() {
        assert_eq!(ass_color_from_rgb(0xFFFFFF), "&H00FFFFFF");
        assert_eq!(ass_color_from_rgb(0xFF0000), "&H000000FF");
        assert_eq!(ass_color_from_rgb(0x00FF00), "&H0000FF00");
        assert_eq!(parse_ass_color_rgb("&H000000FF"), Some(0xFF0000));
        assert_eq!(parse_ass_color_rgb("&H00FFFFFF"), Some(0xFFFFFF));
        assert_eq!(parse_ass_color_rgb("#00FF00"), Some(0x00FF00));
        assert_eq!(normalize_ass_color("#FF0000"), "&H000000FF");
        assert_eq!(normalize_ass_color("&h00ffffff"), "&H00FFFFFF");
        assert_eq!(normalize_ass_color("FFFFFF"), "&H00FFFFFF");
        assert_eq!(ass_color_from_rgba(0, 255, 0, 128), "&H8000FF00");
    }

    #[test]
    fn bilingual_generation_roundtrip_and_times() {
        let originals = vec![cue(1000, 3000, "第一句"), cue(4000, 6000, "第二句")];
        let translated = vec![cue(1000, 3000, "First"), cue(4000, 6000, "Second")];
        let opts = AssOptions::default();
        let report = build_bilingual_ass_report(&originals, &translated, &opts);
        assert_eq!(report.matched, 2);
        assert_eq!(report.missing, 0);
        assert!(report.content.contains("[V4+ Styles]"));
        assert!(report.content.contains("Style: Default,"));
        assert!(report.content.contains("Style: AI-Translated,"));
        // 必须是合法的 ASS：能解析回来，且时间轴与原文严格一致
        let doc = parse_ass_document(&report.content);
        assert_eq!(doc.events.len(), 4);
        let orig_events: Vec<&AssEvent> =
            doc.events.iter().filter(|e| e.style == "Default").collect();
        let trans_events: Vec<&AssEvent> = doc
            .events
            .iter()
            .filter(|e| e.style == "AI-Translated")
            .collect();
        assert_eq!(orig_events.len(), 2);
        assert_eq!(trans_events.len(), 2);
        for (a, b) in orig_events.iter().zip(trans_events.iter()) {
            assert_eq!(a.start_ms, b.start_ms);
            assert_eq!(a.end_ms, b.end_ms);
        }
        assert_eq!(orig_events[0].text, "第一句");
        assert_eq!(trans_events[0].text, "First");
    }

    #[test]
    fn bilingual_preserves_original_header_style_and_layout() {
        let originals = vec![cue(1000, 3000, "原文")];
        let translated = vec![cue(1000, 3000, "译文")];
        let mut opts = AssOptions {
            original_style: "CN".into(),
            translated_style: "EN".into(),
            margin_v: 40,
            font_size: 40,
            ..AssOptions::default()
        };
        opts.layout = BilingualLayout::TranslatedBelow;
        let report = build_bilingual_ass_report_with_header(
            &originals,
            &translated,
            &opts,
            Some(ASS_SAMPLE),
        );
        let doc = parse_ass_document(&report.content);
        // 原 ASS 头/样式保留
        assert!(doc.header.contains("PlayResX: 1920"));
        assert!(report.content.contains("Style: CN,Noto Sans CJK SC,52,"));
        // 两种布局 margin 不同：原文在上（MarginV 更大），译文在下
        let m = |name: &str| {
            doc.style_named(name)
                .map(|s| s.line.split(',').nth(21).unwrap_or("").trim().to_string())
                .unwrap()
        };
        let orig_margin: i32 = m("CN").parse().unwrap();
        let trans_margin: i32 = m("EN").parse().unwrap();
        assert!(
            orig_margin > trans_margin,
            "{orig_margin} vs {trans_margin}"
        );
        assert_eq!(trans_margin, 40);
        assert_eq!(orig_margin, 40 + 50); // line_h = 40*1.25
                                          // 事件时间轴一致
        let events = parse_ass_document(&report.content).events;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].start_ms, events[1].start_ms);
    }

    #[test]
    fn bilingual_layout_translated_above() {
        let originals = vec![cue(0, 1000, "orig")];
        let translated = vec![cue(0, 1000, "trans")];
        let opts = AssOptions {
            layout: BilingualLayout::TranslatedAbove,
            original_style: "O".into(),
            translated_style: "T".into(),
            margin_v: 10,
            font_size: 30,
            ..AssOptions::default()
        };
        let report = build_bilingual_ass_report(&originals, &translated, &opts);
        let doc = parse_ass_document(&report.content);
        let margin = |name: &str| -> i32 {
            doc.style_named(name)
                .unwrap()
                .line
                .split(',')
                .nth(21)
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        };
        // 译文在上：译文 MarginV 更大
        assert!(margin("T") > margin("O"));
        assert_eq!(margin("O"), 10);
    }

    #[test]
    fn bilingual_only_translated_emits_single_track() {
        let originals = vec![cue(0, 1000, "原文")];
        let translated = vec![cue(0, 1000, "译文")];
        let opts = AssOptions {
            layout: BilingualLayout::OnlyTranslated,
            ..AssOptions::default()
        };
        let report = build_bilingual_ass_report(&originals, &translated, &opts);
        let doc = parse_ass_document(&report.content);
        assert_eq!(doc.events.len(), 1);
        assert_eq!(doc.events[0].text, "译文");
    }

    #[test]
    fn bilingual_missing_translations() {
        let originals = vec![
            cue(0, 1000, "a"),
            cue(2000, 3000, "b"),
            cue(4000, 5000, "c"),
        ];
        let translated = vec![cue(0, 1000, "A"), cue(4000, 5000, "C")];
        let opts = AssOptions::default();
        let report = build_bilingual_ass_report(&originals, &translated, &opts);
        assert_eq!(report.matched, 2);
        assert_eq!(report.missing, 1);
        let doc = parse_ass_document(&report.content);
        // 3 条原文 + 2 条译文
        assert_eq!(doc.events.len(), 5);
    }

    #[test]
    fn align_translations_by_overlap() {
        let originals = vec![
            cue(0, 1000, "a"),
            cue(2000, 3000, "b"),
            cue(4000, 5000, "c"),
        ];
        let translated = vec![cue(0, 1000, "A"), cue(4000, 5000, "C")];
        let pairs = align_translations(&originals, &translated);
        assert_eq!(pairs[0].unwrap().text, "A");
        assert!(pairs[1].is_none());
        assert_eq!(pairs[2].unwrap().text, "C");
    }

    #[test]
    fn bilingual_negative_and_empty_cues_handled() {
        let originals = vec![
            Cue {
                start_ms: -500,
                end_ms: 1000,
                text: "negative".into(),
                raw_style: Some("{\\an8}".into()),
            },
            cue(2000, 3000, "   "),
        ];
        let opts = AssOptions::default();
        let report = build_bilingual_ass_report(&originals, &[], &opts);
        assert_eq!(report.missing, 2);
        let doc = parse_ass_document(&report.content);
        // 负时间去负；空文本原文行不输出（missing 仍计数）
        assert_eq!(doc.events.len(), 1);
        assert_eq!(doc.events[0].start_ms, 0);
        assert!(doc.events[0].raw_text.contains("{\\an8}"));
    }

    #[test]
    fn build_ass_from_cues_roundtrip() {
        let cues = vec![cue(10000, 12500, "hello"), cue(62000, 63000, "world")];
        let out = build_ass_from_cues(&cues, &AssOptions::default(), None);
        assert!(out.contains("Dialogue: 0,0:00:10.00,0:00:12.50,Default"));
        let doc = parse_ass_document(&out);
        assert_eq!(doc.events.len(), 2);
        assert_eq!(doc.events[1].start_ms, 62000);
        assert_eq!(doc.events[1].text, "world");
    }

    #[test]
    fn header_with_fonts_section_preserved() {
        let header = "[Script Info]\nScriptType: v4.00+\n\n[Fonts]\nfontname: MyFont\nAAAA\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n";
        let out = build_ass_from_cues(&[cue(0, 1000, "x")], &AssOptions::default(), Some(header));
        assert!(out.contains("[Fonts]"));
        assert!(out.contains("fontname: MyFont"));
        assert!(out.contains("AAAA"));
        assert!(out.contains("Dialogue: 0,0:00:00.00,0:00:01.00,Default"));
    }
}
