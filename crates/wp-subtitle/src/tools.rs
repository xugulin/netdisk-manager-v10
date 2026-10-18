//! 字幕工具：时间轴偏移、格式猜测、统计、合并/去重、cue 追踪。
//!
//! 全部是纯函数，便于 `wp-gui` 字幕面板与 `wp-ai` 翻译管线复用。

use crate::model::{Cue, CueStats, CueTracker, SubtitleDoc, SubtitleFormat};

/// 根据扩展名猜测格式（大小写不敏感；不认识返回 None）。
pub fn guess_format_path(path: &str) -> Option<SubtitleFormat> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    match ext.as_str() {
        "srt" => Some(SubtitleFormat::Srt),
        "vtt" | "webvtt" => Some(SubtitleFormat::Vtt),
        "ass" => Some(SubtitleFormat::Ass),
        "ssa" => Some(SubtitleFormat::Ssa),
        _ => None,
    }
}

/// 根据文本内容嗅探格式（外挂字幕自动加载/错误扩展名兜底用）。
pub fn guess_format_content(text: &str) -> Option<SubtitleFormat> {
    let head = text.trim_start_matches('\u{feff}').trim_start();
    let lower_head = head
        .chars()
        .take(64)
        .collect::<String>()
        .to_ascii_lowercase();
    if lower_head.starts_with("webvtt") {
        return Some(SubtitleFormat::Vtt);
    }
    if lower_head.starts_with("[script info]") || lower_head.starts_with("dialogue:") {
        // ASS 与 SSA 用 ScriptType 区分
        for line in text.lines().take(40) {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("ScriptType:") {
                let v = rest.trim().to_ascii_lowercase();
                return Some(if v.starts_with("v4.00") && !v.starts_with("v4.00+") {
                    SubtitleFormat::Ssa
                } else {
                    SubtitleFormat::Ass
                });
            }
        }
        return Some(SubtitleFormat::Ass);
    }
    if head.contains("[V4+ Styles]") || head.contains("[V4 Styles]") {
        return Some(SubtitleFormat::Ass);
    }
    if head.contains("-->") {
        return Some(SubtitleFormat::Srt);
    }
    None
}

/// 时间轴整体平移（就地修改文档）。
///
/// - 结果为负的 cue 裁剪到 0；`end_ms <= 0` 的 cue 丢弃；
/// - 平移后 `end < start` 的（原数据异常）修正为 `end = start`。
pub fn offset_ms(doc: &mut SubtitleDoc, delta_ms: i64) {
    let shifted: Vec<Cue> = doc
        .cues
        .iter()
        .filter_map(|cue| shift_cue(cue, delta_ms))
        .collect();
    doc.cues = shifted;
}

/// 时间轴整体平移（返回新 cue 列表，不改原文档）。
pub fn offset_cues(cues: &[Cue], delta_ms: i64) -> Vec<Cue> {
    cues.iter()
        .filter_map(|cue| shift_cue(cue, delta_ms))
        .collect()
}

fn shift_cue(cue: &Cue, delta_ms: i64) -> Option<Cue> {
    let start = cue.start_ms + delta_ms;
    let end = cue.end_ms + delta_ms;
    if end <= 0 {
        return None;
    }
    let start = start.max(0);
    Some(Cue {
        start_ms: start,
        end_ms: end.max(start),
        text: cue.text.clone(),
        raw_style: cue.raw_style.clone(),
    })
}

/// 统计 cue 信息（空数据返回全 0）。
pub fn stats(cues: &[Cue]) -> CueStats {
    let mut out = CueStats {
        count: cues.len(),
        ..CueStats::default()
    };
    if cues.is_empty() {
        return out;
    }
    let mut durations: Vec<i64> = Vec::with_capacity(cues.len());
    out.first_start_ms = i64::MAX;
    out.last_end_ms = i64::MIN;
    out.min_duration_ms = i64::MAX;
    for cue in cues {
        if cue.is_empty_text() {
            out.empty_count += 1;
        }
        durations.push(cue.duration_ms());
        out.total_duration_ms += cue.duration_ms();
        out.first_start_ms = out.first_start_ms.min(cue.start_ms);
        out.last_end_ms = out.last_end_ms.max(cue.end_ms);
        out.min_duration_ms = out.min_duration_ms.min(cue.duration_ms());
        out.max_duration_ms = out.max_duration_ms.max(cue.duration_ms());
    }
    // 按开始时间排序后统计重叠
    let mut sorted: Vec<&Cue> = cues.iter().collect();
    sorted.sort_by_key(|c| (c.start_ms, c.end_ms));
    for pair in sorted.windows(2) {
        if pair[1].start_ms < pair[0].end_ms {
            out.overlap_count += 1;
        }
    }
    out
}

/// 合并时间上相邻/重叠的 cue（`max_gap_ms` 为允许的最大间隙）。
///
/// 合并后用换行连接文本（保留原文顺序）；样式取第一条的 `raw_style`。
pub fn merge_cues(cues: &[Cue], max_gap_ms: i64) -> Vec<Cue> {
    let mut sorted: Vec<Cue> = cues.to_vec();
    sorted.sort_by_key(|c| (c.start_ms, c.end_ms));
    let mut out: Vec<Cue> = Vec::with_capacity(sorted.len());
    for cue in sorted {
        match out.last_mut() {
            Some(last) if cue.start_ms <= last.end_ms + max_gap_ms.max(0) => {
                if !cue.text.trim().is_empty() {
                    if !last.text.is_empty() {
                        last.text.push('\n');
                    }
                    last.text.push_str(&cue.text);
                }
                last.end_ms = last.end_ms.max(cue.end_ms);
            }
            _ => out.push(cue),
        }
    }
    out
}

/// 去重（相同文本 + 相同起止时间视为重复，保留第一条）。
pub fn dedupe_cues(cues: &[Cue]) -> Vec<Cue> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(cues.len());
    for cue in cues {
        if seen.insert((cue.start_ms, cue.end_ms, cue.text.clone())) {
            out.push(cue.clone());
        }
    }
    out
}

/// 某一时刻正在显示的 cue（线性扫描；播放器每帧热路径请用 [`CueTracker`]）。
pub fn active_cues<'a>(cues: &'a [Cue], time_ms: i64) -> Vec<&'a Cue> {
    cues.iter()
        .filter(|c| c.start_ms <= time_ms && c.end_ms > time_ms)
        .collect()
}

/// 用 cue 列表构造追踪器（供播放器每帧查询）。
pub fn tracker(cues: Vec<Cue>) -> CueTracker {
    CueTracker::new(cues)
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
    fn guess_format_by_path_and_content() {
        assert_eq!(guess_format_path("a.SRT"), Some(SubtitleFormat::Srt));
        assert_eq!(guess_format_path("a.ass"), Some(SubtitleFormat::Ass));
        assert_eq!(guess_format_path("a.txt"), None);
        assert_eq!(guess_format_path("noext"), None);
        assert_eq!(
            guess_format_content("WEBVTT\n\n00:01.000 --> 00:02.000\nx"),
            Some(SubtitleFormat::Vtt)
        );
        assert_eq!(
            guess_format_content("1\n00:00:01,000 --> 00:00:02,000\nx"),
            Some(SubtitleFormat::Srt)
        );
        assert_eq!(
            guess_format_content("[Script Info]\nScriptType: v4.00+\n"),
            Some(SubtitleFormat::Ass)
        );
        assert_eq!(
            guess_format_content("[Script Info]\nScriptType: v4.00\n"),
            Some(SubtitleFormat::Ssa)
        );
        assert_eq!(guess_format_content(""), None);
    }

    #[test]
    fn offset_shifts_and_clamps() {
        let mut doc = SubtitleDoc::from_cues(vec![
            cue(1000, 2000, "a"),
            cue(500, 800, "b"),
            cue(100, 200, "c"),
        ]);
        offset_ms(&mut doc, -600);
        let texts: Vec<&str> = doc.cues.iter().map(|c| c.text.as_str()).collect();
        // 100..200 整体变负 → 丢弃
        assert_eq!(texts, vec!["a", "b"]);
        assert_eq!(doc.cues[0].start_ms, 400);
        assert_eq!(doc.cues[0].end_ms, 1400);
        // b: 500-600=-100 → 起点裁剪 0，end 200
        assert_eq!(doc.cues[1].start_ms, 0);
        assert_eq!(doc.cues[1].end_ms, 200);
    }

    #[test]
    fn offset_positive_and_no_mutation_variant() {
        let cues = vec![cue(100, 200, "a")];
        let shifted = offset_cues(&cues, 1000);
        assert_eq!(shifted[0].start_ms, 1100);
        // 原列表未变
        assert_eq!(cues[0].start_ms, 100);
    }

    #[test]
    fn stats_basic() {
        let cues = vec![cue(0, 1000, "a"), cue(500, 1500, "b"), cue(2000, 3000, "")];
        let s = stats(&cues);
        assert_eq!(s.count, 3);
        assert_eq!(s.empty_count, 1);
        assert_eq!(s.total_duration_ms, 3000);
        assert_eq!(s.first_start_ms, 0);
        assert_eq!(s.last_end_ms, 3000);
        assert_eq!(s.overlap_count, 1);
        assert_eq!(s.max_duration_ms, 1000);
        assert_eq!(s.span_ms(), 3000);
        assert_eq!(s.avg_duration_ms(), 1000);
    }

    #[test]
    fn stats_empty() {
        let s = stats(&[]);
        assert_eq!(s.count, 0);
        assert_eq!(s.avg_duration_ms(), 0);
        assert_eq!(s.span_ms(), 0);
    }

    #[test]
    fn merge_overlapping_and_nearby() {
        let cues = vec![
            cue(0, 1000, "a"),
            cue(900, 2000, "b"),
            cue(2100, 3000, "c"),
            cue(10_000, 11_000, "far"),
        ];
        let merged = merge_cues(&cues, 200);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].text, "a\nb\nc");
        assert_eq!(merged[0].start_ms, 0);
        assert_eq!(merged[0].end_ms, 3000);
        assert_eq!(merged[1].text, "far");
    }

    #[test]
    fn merge_zero_gap_only_overlaps() {
        let cues = vec![cue(0, 1000, "a"), cue(1001, 2000, "b")];
        assert_eq!(merge_cues(&cues, 0).len(), 2);
        assert_eq!(merge_cues(&cues, 1).len(), 1);
    }

    #[test]
    fn dedupe_removes_exact_duplicates() {
        let cues = vec![cue(0, 1000, "a"), cue(0, 1000, "a"), cue(0, 1000, "b")];
        assert_eq!(dedupe_cues(&cues).len(), 2);
    }

    #[test]
    fn active_uses_tracker() {
        let cues = vec![cue(0, 1000, "a"), cue(1500, 2000, "b")];
        let active = active_cues(&cues, 500);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].text, "a");
        assert!(active_cues(&cues, 1200).is_empty());
        let t = tracker(cues);
        assert_eq!(t.current(1800).unwrap().text, "b");
    }
}
