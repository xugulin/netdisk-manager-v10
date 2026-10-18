//! 播放器视频区绘制：16:9 居中黑边、Mock 演示画面、OSD、字幕、弹幕覆盖层。
//!
//! 真实视频帧接入点：
//! - Mode A（进程内 GL render API）：`wp-media-mpv::RenderBridge` 把 mpv 渲染到 FBO，
//!   我们拿到 `egui::TextureId` 后用 `egui::Image`/`Painter::image` 贴到 `video_rect`；
//! - Mode B（原生子窗口）：视频区只画占位/边框，实际画面由 mpv 子窗口覆盖，
//!   此时 `paint_video` 返回的 `video_rect` 供宿主（eframe 窗口）同步子窗口位置用。
//!
//! 当前（M1，MockEngine 阶段）用渐变测试图 + 色条 + 中心准星占位，视觉可截图回归。

use crate::pages::player::{EngineBackend, PlayerUi};
use crate::theme::Theme;
use egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Stroke, Vec2};
use wp_danmaku::{DanmakuMode, OverlayStyle, VideoRect};
use wp_media::PlayerState;

/// 视频区/弹幕覆盖层使用的演示弹幕（真实数据由 wp-danmaku T22/T23 接入后替换）。
pub struct MockDanmaku {
    pub text: &'static str,
    /// 0xRRGGBB。
    pub color: u32,
    /// 出现时间（演示时间轴，秒）。
    pub start: f32,
    /// 0=滚动 1=顶部固定 2=底部固定。
    pub mode: u8,
}

/// 演示弹幕数据：10 条覆盖滚动/顶部/底部三种模式 + 颜色/车道差异，供视觉验收。
pub const MOCK_DANMAKU: &[MockDanmaku] = &[
    MockDanmaku {
        text: "来了来了！",
        color: 0xFFFFFF,
        start: 0.2,
        mode: 0,
    },
    MockDanmaku {
        text: "播放器 UI 好丝滑",
        color: 0x66CCFF,
        start: 0.7,
        mode: 0,
    },
    MockDanmaku {
        text: "字幕面板支持主/副字幕",
        color: 0x99FF99,
        start: 1.2,
        mode: 0,
    },
    MockDanmaku {
        text: "弹幕 4K 下不丢帧（T24 目标）",
        color: 0xFFDD66,
        start: 1.8,
        mode: 0,
    },
    MockDanmaku {
        text: "前面高能！",
        color: 0xFF8844,
        start: 2.3,
        mode: 0,
    },
    MockDanmaku {
        text: "【顶部固定弹幕 · 演示】",
        color: 0xFFFFFF,
        start: 0.0,
        mode: 1,
    },
    MockDanmaku {
        text: "MpvEngine 接入后自动切真实渲染",
        color: 0xCC99FF,
        start: 2.9,
        mode: 0,
    },
    MockDanmaku {
        text: "【底部固定弹幕 · 演示】",
        color: 0xFF99CC,
        start: 0.0,
        mode: 2,
    },
    MockDanmaku {
        text: "wp-danmaku 车道布局引擎待接入",
        color: 0x66FFCC,
        start: 3.5,
        mode: 0,
    },
    MockDanmaku {
        text: "截图验收 ✅",
        color: 0xFFFFFF,
        start: 4.1,
        mode: 0,
    },
];

/// 演示弹幕 → `wp_danmaku::Comment`（交给 T23 布局引擎）。
pub fn demo_comments() -> Vec<wp_danmaku::Comment> {
    MOCK_DANMAKU
        .iter()
        .map(|item| wp_danmaku::Comment {
            time_ms: (item.start as f64 * 1000.0) as i64,
            mode: match item.mode {
                1 => DanmakuMode::Top,
                2 => DanmakuMode::Bottom,
                _ => DanmakuMode::Scroll,
            },
            color: item.color,
            // 1080p 基准字号（wp-danmaku 语义）；窗口内按视频矩形高度等比缩放
            font_size: 32.0,
            text: item.text.to_string(),
            user: None,
        })
        .collect()
}

/// 演示时间轴循环长度（秒）：让弹幕/字幕在演示时持续可见。
const DEMO_CYCLE: f64 = 12.0;

/// 演示字幕 cue：（开始秒，结束秒，文本），循环播放。
const DEMO_CUES: &[(f64, f64, &str)] = &[
    (
        0.0,
        3.6,
        "Mock 演示字幕 · 字幕面板可调延迟 / 字号 / 位置 / 样式",
    ),
    (
        4.0,
        7.6,
        "弹幕覆盖层：egui Painter 绘制（T24 预览）· 支持顶部/底部/滚动",
    ),
    (
        8.0,
        11.6,
        "AI 面板：Provider 启停 / 优先级 / 隐私提示（T21 骨架）",
    ),
];

/// 按屏幕可用区域做 16:9 居中 letterbox，返回视频内容矩形。
pub fn fit_16_9(area: Rect) -> Rect {
    const AR: f32 = 16.0 / 9.0;
    if area.width() <= 0.0 || area.height() <= 0.0 {
        return area;
    }
    let (w, h) = if area.width() / area.height() > AR {
        (area.height() * AR, area.height())
    } else {
        (area.width(), area.width() / AR)
    };
    Rect::from_center_size(area.center(), Vec2::new(w, h))
}

/// 时间格式化：`MM:SS` / `H:MM:SS`（超过 1 小时）。
pub fn format_time(secs: f64) -> String {
    let total = if secs.is_finite() && secs > 0.0 {
        secs.floor() as u64
    } else {
        0
    };
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// 演示字幕当前 cue（Mock 引擎用；真实引擎走 `PlayerEvent::SubtitleCue`）。
pub fn demo_cue(position_secs: f64) -> Option<&'static str> {
    let t = position_secs.rem_euclid(DEMO_CYCLE);
    DEMO_CUES
        .iter()
        .find(|(a, b, _)| t >= *a && t < *b)
        .map(|(_, _, text)| *text)
}

/// 演示时间轴（弹幕/字幕循环用）。
pub fn demo_time(position_secs: f64, backend: EngineBackend) -> f64 {
    match backend {
        EngineBackend::Mock => position_secs.rem_euclid(DEMO_CYCLE),
        EngineBackend::Mpv => position_secs,
    }
}

/// 视频区总绘制入口。
pub fn paint_video(ui: &egui::Ui, area: Rect, player: &PlayerUi, t: &Theme) {
    let painter = ui.painter();
    // 黑边底色
    painter.rect_filled(area, 0, Color32::from_rgb(6, 7, 10));
    let inner = fit_16_9(area);

    match player.backend {
        EngineBackend::Mock => paint_mock_frame(painter, inner, player, t),
        EngineBackend::Mpv => {
            // 真实引擎帧由 RenderBridge 在回调里贴到 inner 矩形；此处仅兜底底色 + 边框。
            painter.rect_filled(inner, 0, Color32::from_rgb(10, 11, 14));
            painter.rect_stroke(
                inner,
                0,
                Stroke::new(1.0, t.border),
                egui::StrokeKind::Inside,
            );
        }
    }

    paint_overlay(ui, inner, player, t);
}

/// 视频内容区之上的 egui 覆盖层：OSD / 字幕 / 弹幕 / 边框。
///
/// Mock 路径由 [`paint_video`] 调用；真实帧路径（MpvRenderContext 渲染完）
/// 由播放页在 `render.paint_rect(ui, inner)` 之后调用，实现 UI/AI/弹幕叠加。
pub fn paint_overlay(ui: &egui::Ui, inner: Rect, player: &PlayerUi, t: &Theme) {
    let painter = ui.painter();
    paint_osd(painter, inner, player, t);

    // 字幕/弹幕覆盖层裁剪到视频内容区（不侵入黑边/右侧面板）
    let overlay = painter.with_clip_rect(inner);
    if player.subtitle.enabled {
        paint_subtitle(&overlay, inner, player, t);
    }
    if player.danmaku.enabled {
        paint_danmaku(&overlay, inner, player, t);
    }

    // 视频区边框（让 16:9 区域在纯色背景中可辨识）
    painter.rect_stroke(
        inner,
        0,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 18)),
        egui::StrokeKind::Inside,
    );
}

/// Mock 演示画面：渐变底色 + 顶部色条 + 中心准星 + 说明文字 + 四角标记。
fn paint_mock_frame(painter: &Painter, rect: Rect, player: &PlayerUi, t: &Theme) {
    // 纵向渐变（用细横条叠加模拟）
    let bands = 16;
    for i in 0..bands {
        let y0 = rect.top() + rect.height() * i as f32 / bands as f32;
        let y1 = rect.top() + rect.height() * (i + 1) as f32 / bands as f32;
        let f = i as f32 / (bands - 1) as f32;
        let c = Color32::from_rgb(
            (16.0 + 18.0 * f) as u8,
            (18.0 + 22.0 * f) as u8,
            (26.0 + 30.0 * f) as u8,
        );
        painter.rect_filled(
            Rect::from_min_max(Pos2::new(rect.left(), y0), Pos2::new(rect.right(), y1)),
            0,
            c,
        );
    }

    // 顶部 55% 高度放 SMPTE 风格低饱和色条（HDR 测试图观感）
    let bars: [Color32; 8] = [
        Color32::from_rgb(46, 52, 74),
        Color32::from_rgb(58, 66, 92),
        Color32::from_rgb(44, 72, 82),
        Color32::from_rgb(40, 82, 74),
        Color32::from_rgb(72, 58, 48),
        Color32::from_rgb(84, 52, 52),
        Color32::from_rgb(64, 50, 82),
        Color32::from_rgb(38, 46, 66),
    ];
    let bar_h = rect.height() * 0.52;
    let bw = rect.width() / bars.len() as f32;
    for (i, c) in bars.iter().enumerate() {
        let x0 = rect.left() + bw * i as f32;
        painter.rect_filled(
            Rect::from_min_size(Pos2::new(x0, rect.top()), Vec2::new(bw, bar_h)),
            0,
            Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 170),
        );
    }

    // 中心准星 + 圆环
    let center = Pos2::new(rect.center().x, rect.top() + rect.height() * 0.44);
    let r = rect.height() * 0.15;
    painter.circle_stroke(
        center,
        r,
        Stroke::new(2.0, Color32::from_rgba_unmultiplied(189, 147, 249, 200)),
    );
    painter.circle_stroke(
        center,
        r * 0.62,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 90)),
    );
    painter.line_segment(
        [
            Pos2::new(center.x - r * 1.25, center.y),
            Pos2::new(center.x + r * 1.25, center.y),
        ],
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 70)),
    );
    painter.line_segment(
        [
            Pos2::new(center.x, center.y - r * 1.25),
            Pos2::new(center.x, center.y + r * 1.25),
        ],
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 70)),
    );

    // 中心文字
    let big = (rect.height() * 0.052).clamp(14.0, 40.0);
    painter.text(
        center + Vec2::new(0.0, -big * 0.4),
        Align2::CENTER_CENTER,
        "MOCK ENGINE",
        FontId::proportional(big),
        Color32::from_rgb(235, 235, 245),
    );
    let (vw, vh) = player.snapshot.video_size.unwrap_or((3840, 2160));
    painter.text(
        center + Vec2::new(0.0, big * 0.7),
        Align2::CENTER_CENTER,
        format!("{vw} × {vh} · HEVC 10bit · HDR→SDR"),
        FontId::proportional((big * 0.44).max(10.0)),
        Color32::from_rgba_unmultiplied(200, 205, 220, 220),
    );

    // 下部说明
    painter.text(
        Pos2::new(rect.center().x, rect.bottom() - rect.height() * 0.16),
        Align2::CENTER_CENTER,
        "演示画面 · 真实帧经 RenderBridge TextureId / paint callback 接入",
        FontId::proportional((big * 0.38).max(9.0)),
        Color32::from_rgba_unmultiplied(150, 156, 175, 200),
    );

    // 四角标记（影院取景框观感）
    let corner = (rect.height() * 0.03).clamp(10.0, 26.0);
    let stroke = Stroke::new(2.0, Color32::from_rgba_unmultiplied(189, 147, 249, 120));
    for (cx, cy, dx, dy) in [
        (rect.left(), rect.top(), 1.0, 1.0),
        (rect.right(), rect.top(), -1.0, 1.0),
        (rect.left(), rect.bottom(), 1.0, -1.0),
        (rect.right(), rect.bottom(), -1.0, -1.0),
    ] {
        let p = Pos2::new(cx, cy);
        painter.line_segment([p, p + Vec2::new(dx * corner, 0.0)], stroke);
        painter.line_segment([p, p + Vec2::new(0.0, dy * corner)], stroke);
    }

    // 右下角水印
    painter.text(
        Pos2::new(rect.right() - 10.0, rect.bottom() - 8.0),
        Align2::RIGHT_BOTTOM,
        format!("wp-media · MockEngine · {}", t.display),
        FontId::proportional(9.0),
        Color32::from_rgba_unmultiplied(255, 255, 255, 60),
    );
}

/// 视频区 OSD：左上状态标签 / 右上时间码 / 左下媒体名。
fn paint_osd(painter: &Painter, rect: Rect, player: &PlayerUi, t: &Theme) {
    let pad = 10.0;
    // 左上：演示/真实 引擎徽标 + 播放状态
    let (state_text, state_color) = state_style(player.snapshot.state, t);
    let tag = match player.backend {
        EngineBackend::Mock => "演示媒体",
        EngineBackend::Mpv => "libmpv",
    };
    painter.text(
        Pos2::new(rect.left() + pad, rect.top() + pad),
        Align2::LEFT_TOP,
        format!("● {tag} · {state_text}"),
        FontId::proportional(11.5),
        state_color,
    );

    // 右上：时间码
    let time = format!(
        "{} / {}",
        format_time(player.position),
        format_time(player.duration)
    );
    painter.text(
        Pos2::new(rect.right() - pad, rect.top() + pad),
        Align2::RIGHT_TOP,
        time,
        FontId::proportional(11.5),
        Color32::from_rgba_unmultiplied(230, 230, 240, 210),
    );

    // 左下：媒体名（截断，避免侵入字幕区）
    let label = if player.source_label.chars().count() > 48 {
        let s: String = player.source_label.chars().take(47).collect();
        format!("{s}…")
    } else {
        player.source_label.clone()
    };
    painter.text(
        Pos2::new(rect.left() + pad, rect.bottom() - rect.height() * 0.30),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(11.0),
        Color32::from_rgba_unmultiplied(190, 195, 210, 160),
    );
}

/// 状态 → (中文文案, 颜色)。
pub fn state_style(state: PlayerState, t: &Theme) -> (&'static str, Color32) {
    match state {
        PlayerState::Idle => ("空闲", t.text_weak),
        PlayerState::Opening => ("打开中…", t.warning),
        PlayerState::Buffering => ("缓冲中…", t.warning),
        PlayerState::Playing => ("播放中", t.success),
        PlayerState::Paused => ("已暂停", t.text_weak),
        PlayerState::Seeking => ("跳转中…", t.warning),
        PlayerState::Ended => ("播放结束", t.text_weak),
        PlayerState::Error => ("错误", t.error),
    }
}

/// 字幕覆盖层（Mock 用演示 cue；真实引擎用 SubtitleCue 事件回填的文本）。
fn paint_subtitle(painter: &Painter, rect: Rect, player: &PlayerUi, t: &Theme) {
    let sub = &player.subtitle;
    let text = match &sub.current_cue {
        Some(text) if player.backend == EngineBackend::Mpv => text.clone(),
        _ => match demo_cue(player.position) {
            Some(text) => text.to_string(),
            None => return,
        },
    };
    let base = (rect.height() * 0.030 * sub.font_scale()).clamp(10.0, 44.0);
    let y = rect.bottom() - rect.height() * sub.margin_ratio.clamp(0.0, 0.4);
    let pos = Pos2::new(rect.center().x, y);

    let color = sub.color();
    // 描边：先画深色偏移副本
    let outline = SubtitleOutline::from(sub.outline);
    for (dx, dy) in outline.offsets {
        painter.text(
            pos + Vec2::new(dx, dy),
            Align2::CENTER_BOTTOM,
            &text,
            FontId::proportional(base),
            Color32::from_black_alpha(220),
        );
    }
    painter.text(
        pos,
        Align2::CENTER_BOTTOM,
        &text,
        FontId::proportional(base),
        color,
    );

    // 副字幕（双语/AI 译文占位）：主字幕上方一行
    if sub.secondary_visible {
        let sec = match &sub.secondary_cue {
            Some(text) => text.clone(),
            None => "AI translated subtitle preview · 演示副字幕（双语布局占位）".to_string(),
        };
        painter.text(
            pos + Vec2::new(0.0, -(base * 1.35)),
            Align2::CENTER_BOTTOM,
            sec,
            FontId::proportional(base * 0.82),
            t.accent,
        );
    }
}

/// 字幕描边偏移表（8 方向；描边粗细 0~4 映射为 1~2px）。
struct SubtitleOutline {
    offsets: Vec<(f32, f32)>,
}

impl SubtitleOutline {
    fn from(strength: f32) -> Self {
        let s = (strength.clamp(0.0, 4.0) * 0.5).max(0.8);
        Self {
            offsets: vec![
                (s, 0.0),
                (-s, 0.0),
                (0.0, s),
                (0.0, -s),
                (s, s),
                (-s, -s),
                (s, -s),
                (-s, s),
            ],
        }
    }
}

/// 弹幕覆盖层：由 `PlayerUi::danmaku_timeline`（wp-danmaku T23 布局引擎）驱动。
///
/// 每帧 `active_geometry_with(at_ms, video_rect, style)` 二分查询当前活跃弹幕，
/// 直接得到 UI 绝对坐标/字号/颜色，与 Mode B 的 ASS `\move` 公式同源（双模式位置一致）。
/// 屏蔽词在布局前经 `DanmakuFilter` 过滤；透明度/字号经 `OverlayStyle` 生效。
fn paint_danmaku(painter: &Painter, rect: Rect, player: &PlayerUi, t: &Theme) {
    let cfg = &player.danmaku;
    if player.danmaku_timeline.is_empty() {
        return;
    }
    let vr = VideoRect::new(rect.left(), rect.top(), rect.width(), rect.height());
    let at_ms = (demo_time(player.position, player.backend) * 1000.0) as i64;
    let style = OverlayStyle::default()
        .with_font_scale(cfg.font_scale.clamp(0.5, 2.5))
        .with_alpha((cfg.opacity.clamp(0.05, 1.0) * 235.0) as u8);
    let glyphs = player
        .danmaku_timeline
        .active_geometry_with(at_ms, vr, &style);
    for glyph in &glyphs {
        let rgb =
            ((glyph.color.0 as u32) << 16) | ((glyph.color.1 as u32) << 8) | glyph.color.2 as u32;
        paint_danmaku_text(
            painter,
            Pos2::new(glyph.x, glyph.y),
            Align2::LEFT_TOP,
            &glyph.text,
            glyph.font_px,
            rgb,
            glyph.alpha,
        );
    }

    // 演示弹幕角标（说明当前用的是真实布局引擎 + 演示数据）
    painter.text(
        Pos2::new(rect.right() - 10.0, rect.top() + rect.height() * 0.30),
        Align2::RIGHT_TOP,
        format!(
            "弹幕覆盖层 · wp-danmaku 布局 · 同屏 {} 条 · {:.0}% 不透明度",
            glyphs.len(),
            cfg.opacity * 100.0
        ),
        FontId::proportional(9.5),
        Color32::from_rgba_unmultiplied(255, 255, 255, 80),
    );
    let _ = t;
}

/// 单条弹幕绘制：黑色描边偏移 + 本体颜色。
fn paint_danmaku_text(
    painter: &Painter,
    pos: Pos2,
    anchor: Align2,
    text: &str,
    size: f32,
    rgb: u32,
    alpha: u8,
) {
    let font = FontId::proportional(size);
    for (dx, dy) in [(1.2, 0.0), (-1.2, 0.0), (0.0, 1.2), (0.0, -1.2)] {
        painter.text(
            pos + Vec2::new(dx, dy),
            anchor,
            text,
            font.clone(),
            Color32::from_black_alpha(alpha),
        );
    }
    painter.text(pos, anchor, text, font, rgba_with_alpha(rgb, alpha));
}

/// 0xRRGGBB + alpha → Color32。
pub fn rgba_with_alpha(rgb: u32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
        alpha,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_time_basic() {
        assert_eq!(format_time(0.0), "00:00");
        assert_eq!(format_time(65.9), "01:05");
        assert_eq!(format_time(3600.0), "1:00:00");
        assert_eq!(format_time(3661.4), "1:01:01");
        assert_eq!(format_time(-5.0), "00:00");
        assert_eq!(format_time(f64::NAN), "00:00");
    }

    #[test]
    fn fit_16_9_always_ratio() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 400.0)); // 宽 > 16:9
        let inner = fit_16_9(area);
        assert!((inner.width() / inner.height() - 16.0 / 9.0).abs() < 1e-3);
        assert_eq!(inner.center(), area.center());
        assert!(inner.width() <= area.width() && inner.height() <= area.height());

        let tall = Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 1000.0)); // 高 > 16:9
        let inner = fit_16_9(tall);
        assert!((inner.width() / inner.height() - 16.0 / 9.0).abs() < 1e-3);
        assert!(inner.width() <= tall.width() && inner.height() <= tall.height());
    }

    #[test]
    fn demo_cues_cover_cycle_without_gap() {
        // 每个 cycle 时间点要么命中 cue，要么位于 cue 间隙；至少大部分时间有字幕
        let mut hits = 0;
        for i in 0..120 {
            let t = i as f64 * 0.1;
            if demo_cue(t).is_some() {
                hits += 1;
            }
        }
        assert!(hits > 100, "演示字幕覆盖率过低：{hits}/120");
        // 超出 cycle 后循环
        assert_eq!(demo_cue(4.2), demo_cue(4.2 + DEMO_CYCLE));
    }

    #[test]
    fn demo_danmaku_visible_near_start() {
        // 演示起播位置 2s 附近应至少有 3 条滚动弹幕在屏
        let now = 2.0_f64;
        let dur = 8.0_f64;
        let visible = MOCK_DANMAKU
            .iter()
            .filter(|d| d.mode == 0)
            .filter(|d| {
                let e = now - d.start as f64;
                (0.0..=dur).contains(&e)
            })
            .count();
        assert!(visible >= 3, "起播 2s 可见滚动弹幕仅 {visible} 条");
    }

    #[test]
    fn rgba_with_alpha_packs() {
        // Color32 内部是预乘 alpha 存储，通道值不等于原 RGB，故与构造函数结果比较。
        let c = rgba_with_alpha(0x3366FF, 128);
        assert_eq!(c, Color32::from_rgba_unmultiplied(0x33, 0x66, 0xFF, 128));
        assert_eq!(c.a(), 128);
        let opaque = rgba_with_alpha(0x3366FF, 255);
        assert_eq!((opaque.r(), opaque.g(), opaque.b()), (0x33, 0x66, 0xFF));
    }
}
