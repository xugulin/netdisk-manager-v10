//! 字幕数据模型（对外公开的类型都从 crate 根 re-export）。
//!
//! 这里只放「纯数据」：cue / 文档 / 格式枚举 / 双语样式选项 / 统计与追踪。
//! 解析、生成、时间轴工具分别在 `srt` / `vtt` / `ass` / `tools` 模块。

use serde::{Deserialize, Serialize};

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

impl Cue {
    /// 时长（毫秒，负数按 0 处理）。
    pub fn duration_ms(&self) -> i64 {
        (self.end_ms - self.start_ms).max(0)
    }

    /// 文本是否为空（全空白）。
    pub fn is_empty_text(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// 规范化：起点/终点不为负，且 `end >= start`。
    pub fn normalized(&self) -> Cue {
        let start = self.start_ms.max(0);
        let end = self.end_ms.max(start);
        Cue {
            start_ms: start,
            end_ms: end,
            text: self.text.clone(),
            raw_style: self.raw_style.clone(),
        }
    }
}

/// 字幕格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubtitleFormat {
    Srt,
    Vtt,
    Ass,
    Ssa,
}

impl SubtitleFormat {
    /// 常见扩展名（用于另存为/建议文件名）。
    pub fn extension(self) -> &'static str {
        match self {
            SubtitleFormat::Srt => "srt",
            SubtitleFormat::Vtt => "vtt",
            SubtitleFormat::Ass => "ass",
            SubtitleFormat::Ssa => "ssa",
        }
    }

    /// 是否为 ASS 家族（含 SSA），需要保留/生成 ASS 头。
    pub fn is_ass_family(self) -> bool {
        matches!(self, SubtitleFormat::Ass | SubtitleFormat::Ssa)
    }
}

/// 字幕文档（解析结果）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubtitleDoc {
    pub format: Option<SubtitleFormat>,
    pub cues: Vec<Cue>,
    /// 原始 ASS 头（生成双语字幕时保留样式）。
    pub ass_header: Option<String>,
}

impl SubtitleDoc {
    /// 构造一个纯 SRT 风格文档。
    pub fn from_cues(cues: Vec<Cue>) -> Self {
        Self {
            format: Some(SubtitleFormat::Srt),
            cues,
            ass_header: None,
        }
    }

    /// cue 数。
    pub fn len(&self) -> usize {
        self.cues.len()
    }

    /// 是否没有 cue。
    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    /// 按开始时间排序（稳定）。
    pub fn sort_by_time(&mut self) {
        self.cues
            .sort_by_key(|c| (c.start_ms, c.end_ms, c.text.clone()));
    }
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

impl AssOptions {
    /// 校验/修正明显不合法的值（字号为 0、负 margin 等），返回修正后的副本。
    pub fn sanitized(&self) -> AssOptions {
        let mut out = self.clone();
        if out.font_size == 0 {
            out.font_size = 48;
        }
        out.font_size = out.font_size.clamp(8, 512);
        out.margin_v = out.margin_v.max(0);
        if out.font_name.trim().is_empty() {
            out.font_name = "Noto Sans CJK SC".into();
        }
        if out.original_style.trim().is_empty() {
            out.original_style = "Default".into();
        }
        if out.translated_style.trim().is_empty() {
            out.translated_style = "AI-Translated".into();
        }
        out
    }
}

/// 生成双语 ASS 的结果（含配对统计，便于 AI 层提示「漏译」）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BilingualAss {
    /// ASS 全文。
    pub content: String,
    /// 成功配上译文的 cue 数。
    pub matched: usize,
    /// 没配上译文（仅输出原文）的 cue 数。
    pub missing: usize,
}

/// cue 统计（字幕面板/timeline 用）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CueStats {
    pub count: usize,
    /// 文本为空的 cue 数。
    pub empty_count: usize,
    /// 所有 cue 时长之和（毫秒）。
    pub total_duration_ms: i64,
    /// 最早开始时间（无 cue 时为 0）。
    pub first_start_ms: i64,
    /// 最晚结束时间（无 cue 时为 0）。
    pub last_end_ms: i64,
    pub min_duration_ms: i64,
    pub max_duration_ms: i64,
    /// 与「上一条」时间重叠的 cue 数（按开始时间排序后统计）。
    pub overlap_count: usize,
}

impl CueStats {
    /// 字幕覆盖时长（首到尾，毫秒）。
    pub fn span_ms(&self) -> i64 {
        (self.last_end_ms - self.first_start_ms).max(0)
    }

    /// 平均每条时长（毫秒）。
    pub fn avg_duration_ms(&self) -> i64 {
        if self.count == 0 {
            0
        } else {
            self.total_duration_ms / self.count as i64
        }
    }
}

/// 当前 cue 追踪器（mpv `sub-start`/`sub-text` 观察结果的纯逻辑镜像）。
///
/// 内部按开始时间排序，查找用二分，O(log n)。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CueTracker {
    cues: Vec<Cue>,
    /// 最长 cue 时长（查找 active 时用来限定回扫窗口）。
    max_duration_ms: i64,
}

impl CueTracker {
    /// 用一份 cue 列表构造（内部会排序）。
    pub fn new(mut cues: Vec<Cue>) -> Self {
        cues.sort_by_key(|c| (c.start_ms, c.end_ms));
        let max_duration_ms = cues.iter().map(|c| c.duration_ms()).max().unwrap_or(0);
        Self {
            cues,
            max_duration_ms,
        }
    }

    /// 全部 cue（已排序）。
    pub fn cues(&self) -> &[Cue] {
        &self.cues
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    /// 某一时刻正在显示的全部 cue（重叠字幕可能多条）。
    pub fn active(&self, time_ms: i64) -> Vec<&Cue> {
        // start <= time 的上界
        let hi = self.cues.partition_point(|c| c.start_ms <= time_ms);
        // 只有 start > time - max_duration 的 cue 才可能仍在显示
        let lo = self
            .cues
            .partition_point(|c| c.start_ms <= time_ms - self.max_duration_ms)
            .min(hi);
        let mut out = Vec::new();
        for cue in &self.cues[lo..hi] {
            if cue.end_ms > time_ms {
                out.push(cue);
            }
        }
        out
    }

    /// 某一时刻「当前」的字幕：active 中开始时间最晚的一条。
    pub fn current(&self, time_ms: i64) -> Option<&Cue> {
        self.active(time_ms).into_iter().max_by_key(|c| c.start_ms)
    }

    /// 下一批即将出现的 cue（start 最接近且 > time 的所有同起点 cue）。
    pub fn next(&self, time_ms: i64) -> Option<&Cue> {
        let idx = self.cues.partition_point(|c| c.start_ms <= time_ms);
        self.cues.get(idx)
    }

    /// 距离下一条 cue 的时间（无下一条返回 None）。
    pub fn ms_until_next(&self, time_ms: i64) -> Option<i64> {
        self.next(time_ms).map(|c| c.start_ms - time_ms)
    }
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

    #[test]
    fn cue_duration_and_normalize() {
        let c = cue(-100, -50, "x");
        assert_eq!(c.duration_ms(), 50);
        let n = c.normalized();
        assert_eq!((n.start_ms, n.end_ms), (0, 0));
        let c2 = cue(100, 50, "x");
        assert_eq!(c2.normalized().end_ms, 100);
    }

    #[test]
    fn tracker_active_current_next() {
        let t = CueTracker::new(vec![
            cue(3000, 4000, "c"),
            cue(0, 2000, "a"),
            cue(1000, 2500, "b"),
        ]);
        // 已排序
        assert_eq!(t.cues()[0].text, "a");
        assert_eq!(t.active(500).len(), 1);
        assert_eq!(t.active(1500).len(), 2);
        // 当前取开始最晚的
        assert_eq!(t.current(1500).unwrap().text, "b");
        assert_eq!(t.current(5000), None);
        assert_eq!(t.next(1500).unwrap().text, "c");
        assert_eq!(t.ms_until_next(1500), Some(1500));
        assert!(t.next(3500).is_none());
    }

    #[test]
    fn tracker_empty() {
        let t = CueTracker::new(vec![]);
        assert!(t.is_empty());
        assert_eq!(t.current(0), None);
        assert_eq!(t.next(0), None);
    }

    #[test]
    fn options_sanitized() {
        let mut o = AssOptions::default();
        o.font_size = 0;
        o.margin_v = -10;
        o.font_name = "  ".into();
        let s = o.sanitized();
        assert_eq!(s.font_size, 48);
        assert_eq!(s.margin_v, 0);
        assert!(!s.font_name.trim().is_empty());
    }
}
