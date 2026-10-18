//! 字幕子系统（Rust 侧）：解析/生成/AI 双语 ASS 注入。
//!
//! 渲染由 mpv/libass 负责；本 crate 负责"我们可控"的部分：
//! - 解析 SRT/VTT/ASS/SSA → 结构化 cue（AI 翻译/搜索/导出用）
//! - 生成双语 ASS（保留原样式/时间轴）→ 写临时文件 → mpv `sub-add`/`sub-reload`
//! - 字幕轨道/双字幕/样式偏好模型
//!
//! 任务 T7–T11。

use serde::{Deserialize, Serialize};
use wp_core::{Error, Result};

/// 单条字幕 cue。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Cue {
    /// 开始时间（毫秒）。
    pub start_ms: i64,
    /// 结束时间（毫秒）。
    pub end_ms: i64,
    /// 纯文本（ASS 去特效后的可读文本；保留原始文本供生成用）。
    pub text: String,
    /// ASS 原始样式字段（如 `{\pos(..)}` 前缀），生成双语 ASS 时保留。
    pub raw_style: Option<String>,
}

/// 字幕格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubtitleFormat {
    Srt,
    Vtt,
    Ass,
    Ssa,
}

/// 字幕文档（解析结果）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubtitleDoc {
    pub format: Option<SubtitleFormat>,
    pub cues: Vec<Cue>,
    /// 原始 ASS 头（生成双语字幕时保留样式）。
    pub ass_header: Option<String>,
}

/// 双语排版方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BilingualLayout {
    /// 原文上、译文下（默认）。
    #[default]
    TranslatedBelow,
    TranslatedAbove,
    OnlyTranslated,
}

/// 生成 ASS 的样式选项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssOptions {
    pub font_name: String,
    pub font_size: u32,
    pub primary_color: String,
    pub outline_color: String,
    pub margin_v: i32,
    pub layout: BilingualLayout,
    /// 原文轨道样式名（若存在）。
    pub original_style: String,
    /// 译文轨道样式名。
    pub translated_style: String,
}

impl Default for AssOptions {
    fn default() -> Self {
        Self {
            font_name: "Noto Sans CJK SC".into(),
            font_size: 48,
            primary_color: "&H00FFFFFF".into(),
            outline_color: "&H00000000".into(),
            margin_v: 40,
            layout: BilingualLayout::TranslatedBelow,
            original_style: "Default".into(),
            translated_style: "AI-Translated".into(),
        }
    }
}

/// 根据扩展名猜测格式。
pub fn guess_format(path: &str) -> Option<SubtitleFormat> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    match ext.as_str() {
        "srt" => Some(SubtitleFormat::Srt),
        "vtt" => Some(SubtitleFormat::Vtt),
        "ass" => Some(SubtitleFormat::Ass),
        "ssa" => Some(SubtitleFormat::Ssa),
        _ => None,
    }
}

/// 解析字幕文本（T7 实现）。
pub fn parse(_format: SubtitleFormat, _text: &str) -> Result<SubtitleDoc> {
    Err(Error::unsupported("字幕解析尚未实现（T7 任务）"))
}

/// 导出 SRT（T7 实现）。
pub fn build_srt(_doc: &SubtitleDoc) -> String {
    String::new()
}

/// 生成双语 ASS（T7/T11 实现；AI 翻译结果注入用）。
pub fn build_bilingual_ass(_original: &[Cue], _translated: &[Cue], _opts: &AssOptions) -> String {
    String::new()
}
