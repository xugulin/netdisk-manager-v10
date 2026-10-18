//! 生成 ASS 弹幕轨（Mode B：交给 mpv/libass 渲染）。
//!
//! - 滚动弹幕用 `\an8\move(x1,y,x2,y)`（右→左），固定弹幕用 `\an8\pos`（顶部）/
//!   `\an2\pos`（底部）
//! - 颜色 `0xRRGGBB` → `\c&HBBGGRR&`，透明度 `\alpha&HAA&`，字号按视频高度换算
//! - ASS PlayRes 统一为「高度 1080、宽度按视频宽高比」：1080p 与 4K 生成的坐标/字号
//!   完全一致（相对布局不变），避免 4K 下弹幕变小/变大
//! - 时间窗裁剪：只导出窗口内活跃事件（用于分片 `sub-reload`），**时间保持视频绝对
//!   时间**，因为 mpv/libass 按媒体时间戳比较 ASS 事件

use crate::model::{AssRenderOptions, DanmakuMode, LayoutConfig, PlacedComment};
use crate::time::format_ass_timestamp;

/// 1080p 基准高度。
const BASE_HEIGHT: f32 = 1080.0;

/// 计算 ASS 的 PlayRes（高度固定 1080，宽度按视频宽高比）。
pub fn play_res(cfg: &LayoutConfig) -> (u32, u32) {
    let cfg = cfg.sanitized();
    let aspect = cfg.video_width as f32 / cfg.video_height as f32;
    let width = (aspect * BASE_HEIGHT).round().max(1.0) as u32;
    (width, 1080)
}

/// 生成 ASS 弹幕轨（全部弹幕，默认选项）。
pub fn generate_ass(placed: &[PlacedComment], cfg: &LayoutConfig) -> String {
    generate_ass_with_options(placed, cfg, &AssRenderOptions::default())
}

/// 生成指定时间窗内的 ASS 弹幕轨（分片 `sub-reload`）。
pub fn generate_ass_windowed(
    placed: &[PlacedComment],
    cfg: &LayoutConfig,
    start_ms: i64,
    end_ms: i64,
) -> String {
    generate_ass_with_options(placed, cfg, &AssRenderOptions::windowed(start_ms, end_ms))
}

/// 生成 ASS 弹幕轨（可配置时间窗/透明度/字体/描边）。
pub fn generate_ass_with_options(
    placed: &[PlacedComment],
    cfg: &LayoutConfig,
    opts: &AssRenderOptions,
) -> String {
    let cfg = cfg.sanitized();
    let geom = crate::layout::geometry(&cfg);
    let (play_w, play_h) = play_res(&cfg);
    let mut out = ass_header(play_w, opts);
    let mut events: Vec<(i64, String)> = Vec::with_capacity(placed.len());
    for item in placed {
        let text = item.comment.text.trim();
        if text.is_empty() {
            continue;
        }
        let mut start = item.start_ms;
        let mut end = item.end_ms;
        if let Some((window_start, window_end)) = opts.window {
            if end <= window_start || start >= window_end {
                continue;
            }
            start = start.max(window_start);
            end = end.min(window_end);
        }
        if end <= start {
            continue;
        }
        let font_px = (item.comment.font_size * cfg.font_scale).max(1.0);
        let color = ass_color(item.comment.color);
        let mut tags = String::from("{");
        match item.comment.mode {
            DanmakuMode::Scroll => {
                // 文本中心从右边界外到左边界外
                let half_width = item.width_ratio * play_w as f32 * 0.5;
                let y = item.y_ratio * play_h as f32;
                tags.push_str(&format!(
                    "\\an8\\move({:.0},{:.0},{:.0},{:.0})",
                    play_w as f32 + half_width,
                    y,
                    -half_width,
                    y
                ));
            }
            DanmakuMode::Top => {
                let y = item.y_ratio * play_h as f32;
                tags.push_str(&format!("\\an8\\pos({:.0},{:.0})", play_w as f32 * 0.5, y));
            }
            DanmakuMode::Bottom => {
                // 文本底部 y = (顶部 y + 行高) * play_h
                let y = (item.y_ratio + geom.lane_height_ratio) * play_h as f32;
                tags.push_str(&format!("\\an2\\pos({:.0},{:.0})", play_w as f32 * 0.5, y));
            }
        }
        tags.push_str(&format!("\\fs{font_px:.0}"));
        tags.push_str("\\c");
        tags.push_str(&color);
        tags.push_str(&format!("\\alpha&H{:02X}&", opts.alpha));
        tags.push('}');
        let body = escape_danmaku_text(text);
        let layer = if item.comment.mode.is_fixed() { 2 } else { 1 };
        let name = item.comment.user.as_deref().unwrap_or("").replace(',', " ");
        let line = format!(
            "Dialogue: {layer},{},{},Danmaku,{name},0,0,0,,{tags}{body}",
            format_ass_timestamp(start),
            format_ass_timestamp(end)
        );
        events.push((start, line));
    }
    events.sort_by_key(|(start, _)| *start);
    for (_, line) in events {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// 布局 + 生成 ASS 一步到位（本地导出弹幕 ASS 用）。
pub fn export_ass(comments: &[crate::model::Comment], cfg: &LayoutConfig) -> String {
    let placed = crate::layout::layout(comments, cfg);
    generate_ass(&placed, cfg)
}

/// `0xRRGGBB` → ASS 颜色 `&HBBGGRR&`。
pub fn ass_color(rgb: u32) -> String {
    let r = (rgb >> 16) & 0xFF;
    let g = (rgb >> 8) & 0xFF;
    let b = rgb & 0xFF;
    format!("&H{b:02X}{g:02X}{r:02X}&")
}

/// 弹幕文本转义：换行 → `\N`，`{}` → 全角（避免被 libass 当 override 块）。
pub fn escape_danmaku_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '\n' | '\r' => out.push_str("\\N"),
            '{' => out.push('｛'),
            '}' => out.push('｝'),
            _ => out.push(ch),
        }
    }
    out
}

fn ass_header(play_w: u32, opts: &AssRenderOptions) -> String {
    let font = opts.font_name.replace(',', " ");
    let outline = opts.outline;
    let shadow = opts.shadow;
    format!(
        "[Script Info]\n\
; 由 wp-danmaku 生成（Mode B：ASS 弹幕轨）\n\
ScriptType: v4.00+\n\
PlayResX: {play_w}\n\
PlayResY: 1080\n\
WrapStyle: 2\n\
ScaledBorderAndShadow: yes\n\
YCbCr Matrix: TV.709\n\
\n\
[V4+ Styles]\n\
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
Style: Danmaku,{font},25,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,{outline},{shadow},7,0,0,0,1\n\
\n\
[Events]\n\
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::layout;
    use crate::model::Comment;

    fn placed_scroll(start: i64, end: i64, text: &str, color: u32) -> PlacedComment {
        let mut comment = Comment::new(start, DanmakuMode::Scroll, text);
        comment.color = color;
        PlacedComment {
            comment,
            lane: 0,
            y_ratio: 0.1,
            width_ratio: 0.2,
            start_ms: start,
            end_ms: end,
        }
    }

    #[test]
    fn play_res_normalized() {
        assert_eq!(play_res(&LayoutConfig::hd1080()), (1920, 1080));
        // 4K 同宽高比 → 与 1080p 相同的 PlayRes/坐标体系
        assert_eq!(play_res(&LayoutConfig::uhd4k()), (1920, 1080));
        let mut wide = LayoutConfig::hd1080();
        wide.video_width = 2560;
        assert_eq!(play_res(&wide), (2560, 1080));
    }

    #[test]
    fn generate_scroll_has_move_and_color() {
        let cfg = LayoutConfig::hd1080();
        let placed = vec![placed_scroll(1000, 9000, "滚动弹幕", 0xFF0000)];
        let ass = generate_ass(&placed, &cfg);
        assert!(ass.starts_with("[Script Info]"));
        assert!(ass.contains("[V4+ Styles]"));
        assert!(ass.contains("\\move("));
        assert!(ass.contains("\\an8"));
        // 红色 0xFF0000 → &H0000FF&
        assert!(ass.contains("\\c&H0000FF&"));
        assert!(ass.contains("\\alpha&H00&"));
        assert!(ass.contains("\\fs25"));
        assert!(ass.contains("Dialogue: 1,0:00:01.00,0:00:09.00,Danmaku,"));
    }

    #[test]
    fn generate_fixed_top_bottom_has_pos() {
        let cfg = LayoutConfig::hd1080();
        let top = PlacedComment {
            comment: Comment::new(1000, DanmakuMode::Top, "顶部"),
            lane: 0,
            y_ratio: 0.05,
            width_ratio: 0.1,
            start_ms: 1000,
            end_ms: 5000,
        };
        let bottom = PlacedComment {
            comment: Comment::new(2000, DanmakuMode::Bottom, "底部"),
            lane: 0,
            y_ratio: 0.65,
            width_ratio: 0.1,
            start_ms: 2000,
            end_ms: 6000,
        };
        let ass = generate_ass(&[top, bottom], &cfg);
        assert!(ass.contains("\\an8\\pos(960,54)"));
        assert!(ass.contains("\\an2\\pos(960,"));
        assert!(ass.contains("Dialogue: 2,0:00:01.00,0:00:05.00,Danmaku,"));
        assert!(ass.contains("Dialogue: 2,0:00:02.00,0:00:06.00,Danmaku,"));
        // 底部 y 在屏幕内
        let bottom_line = ass
            .lines()
            .find(|l| l.contains("底部"))
            .expect("底部弹幕行");
        assert!(bottom_line.contains("\\an2\\pos"));
    }

    #[test]
    fn window_clipping() {
        let cfg = LayoutConfig::hd1080();
        let placed = vec![
            placed_scroll(0, 8000, "跨越窗口", 0xFFFFFF),
            placed_scroll(6000, 14000, "窗口之后", 0xFFFFFF),
            placed_scroll(0, 2000, "窗口之前", 0xFFFFFF),
        ];
        let ass = generate_ass_windowed(&placed, &cfg, 3000, 5000);
        // 只保留与窗口相交的一条，且时间被裁剪到窗口
        assert_eq!(ass.matches("Dialogue:").count(), 1);
        assert!(ass.contains("Dialogue: 1,0:00:03.00,0:00:05.00,Danmaku,"));
        assert!(ass.contains("跨越窗口"));
        assert!(!ass.contains("窗口之后"));
        assert!(!ass.contains("窗口之前"));
        // 空窗口
        let empty = generate_ass_windowed(&placed, &cfg, 20_000, 30_000);
        assert_eq!(empty.matches("Dialogue:").count(), 0);
    }

    #[test]
    fn generate_4k_equals_1080p() {
        let comments: Vec<Comment> = (0..20)
            .map(|i| {
                let mode = match i % 3 {
                    0 => DanmakuMode::Scroll,
                    1 => DanmakuMode::Top,
                    _ => DanmakuMode::Bottom,
                };
                Comment::new(i * 500, mode, format!("弹幕{i}"))
            })
            .collect();
        let hd = export_ass(&comments, &LayoutConfig::hd1080());
        let uhd = export_ass(&comments, &LayoutConfig::uhd4k());
        assert_eq!(hd, uhd);
    }

    #[test]
    fn alpha_and_font_options() {
        let cfg = LayoutConfig::hd1080();
        let placed = vec![placed_scroll(0, 4000, "半透明", 0x00FF00)];
        let mut opts = AssRenderOptions::default();
        opts.alpha = 128;
        opts.font_name = "Custom Font".into();
        opts.outline = 2.0;
        let ass = generate_ass_with_options(&placed, &cfg, &opts);
        assert!(ass.contains("\\alpha&H80&"));
        assert!(ass.contains("Style: Danmaku,Custom Font,25,"));
        assert!(ass.contains(",1,2,0,7,"));
        // 绿色 0x00FF00 → &H00FF00&
        assert!(ass.contains("\\c&H00FF00&"));
    }

    #[test]
    fn escape_braces_and_newlines() {
        let cfg = LayoutConfig::hd1080();
        let placed = vec![placed_scroll(0, 4000, "花括号{tag}\n换行", 0xFFFFFF)];
        let ass = generate_ass(&placed, &cfg);
        assert!(ass.contains("花括号｛tag｝\\N换行"));
        assert!(!ass.contains("花括号{tag}"));
    }

    #[test]
    fn empty_placed_produces_valid_header() {
        let ass = generate_ass(&[], &LayoutConfig::hd1080());
        assert!(ass.contains("[Events]"));
        assert!(!ass.contains("Dialogue:"));
    }

    #[test]
    fn timeline_roundtrip_from_layout() {
        let comments = vec![
            Comment::new(1000, DanmakuMode::Scroll, "a"),
            Comment::new(2000, DanmakuMode::Top, "b"),
        ];
        let cfg = LayoutConfig::hd1080();
        let placed = layout(&comments, &cfg);
        let ass = generate_ass(&placed, &cfg);
        let mut times = Vec::new();
        for line in ass.lines().filter(|l| l.starts_with("Dialogue:")) {
            let fields: Vec<&str> = line.splitn(10, ',').collect();
            let start = crate::time::parse_ass_timestamp(fields[1]).unwrap();
            let end = crate::time::parse_ass_timestamp(fields[2]).unwrap();
            times.push((start, end));
        }
        times.sort();
        assert_eq!(times[0].0, 1000);
        assert_eq!(times[1].0, 2000);
        assert!(times[1].1 - times[1].0 >= 4000);
    }

    #[test]
    fn fixed_line_y_within_play_res() {
        let cfg = LayoutConfig::hd1080();
        let geom = crate::layout::geometry(&cfg);
        let bottom = PlacedComment {
            comment: Comment::new(0, DanmakuMode::Bottom, "底部"),
            lane: 0,
            y_ratio: geom.bottom_lane_top(0),
            width_ratio: 0.1,
            start_ms: 0,
            end_ms: 4000,
        };
        let ass = generate_ass(&[bottom], &cfg);
        let line = ass.lines().find(|l| l.contains("\\an2\\pos")).unwrap();
        let y: f32 = line
            .split("\\an2\\pos(960,")
            .nth(1)
            .unwrap()
            .split(')')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(y > 0.0 && y <= 1080.0, "bottom y={y}");
    }
}
