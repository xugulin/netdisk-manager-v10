//! egui 覆盖层几何 helper（纯逻辑，**不依赖 egui**）。
//!
//! 把 [`PlacedComment`] + 当前播放时间换算成「屏幕坐标文字」（[`DanmakuGlyph`]），
//! GUI 侧只需用 `Painter::text` 按 glyph 逐条绘制即可；本模块不含任何绘制/字体代码，
//! 因此可纯单测（4K/1080p 一致性、窗口缩放、边界、命中测试）。
//!
//! # 坐标口径
//! - [`VideoRect`]：视频画面在 UI 里的矩形（egui 逻辑点）。GUI 用
//!   [`VideoRect::fit_video`] 做「contain + 黑边居中」适配后传入。
//! - glyph 的 `x/y` 是 **UI 绝对坐标**（可直接给 `Painter`），`y` 是文本顶部。
//! - 字号：`Comment::font_size` 以 1080p 为基准（默认 25），所以
//!   `font_px = font_size * font_scale * rect.h / 1080`。
//!   （Lead 描述里的 `rect.h/2160` 是 4K 源视频的中间量：
//!   `25 * (2160/1080) * (rect.h/2160) = 25 * rect.h/1080`，结果相同。）
//! - 滚动弹幕：`x` 按时间线性插值（与 [`crate::render_ass`] 的 `\move` 同一公式，
//!   Mode A / Mode B 位置一致）；固定弹幕：水平居中、`y` 静止。
//!
//! # 绘制顺序
//! [`crate::layout::PlacedTimeline::active_geometry`] 返回顺序即绘制顺序：
//! 先滚动（底层）后顶部/底部（上层）。

use crate::layout::PlacedTimeline;
use crate::model::{DanmakuMode, PlacedComment};
use serde::{Deserialize, Serialize};

/// 1080p 基准高度（`Comment::font_size` 的语义基准）。
const BASE_HEIGHT: f32 = 1080.0;

/// 视频画面在 UI 中的矩形（egui 逻辑点，`x/y` 为左上角）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct VideoRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl VideoRect {
    /// 构造。
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// 从尺寸构造（左上角在原点）。
    pub fn from_size(w: f32, h: f32) -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            w,
            h,
        }
    }

    /// 右边界 / 下边界 / 中心。
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn center_x(&self) -> f32 {
        self.x + self.w * 0.5
    }

    pub fn center_y(&self) -> f32 {
        self.y + self.h * 0.5
    }

    /// 相对坐标（0~1）→ UI x。
    pub fn x_at(&self, ratio: f32) -> f32 {
        self.x + ratio * self.w
    }

    /// 相对坐标（0~1）→ UI y。
    pub fn y_at(&self, ratio: f32) -> f32 {
        self.y + ratio * self.h
    }

    /// 字号缩放系数：1080p 基准 → UI 点。
    pub fn font_scale(&self) -> f32 {
        self.h / BASE_HEIGHT
    }

    /// 是否包含某点。
    pub fn contains(&self, point: (f32, f32)) -> bool {
        point.0 >= self.x
            && point.0 <= self.right()
            && point.1 >= self.y
            && point.1 <= self.bottom()
    }

    /// 把视频按宽高比放进 `area`（contain + 黑边居中），返回实际画面矩形。
    ///
    /// 例：16:9 视频放进正方形区域 → 上下黑边；`video_width/video_height` 为视频像素分辨率。
    pub fn fit_video(area: VideoRect, video_width: u32, video_height: u32) -> VideoRect {
        let video_w = if video_width == 0 { 16 } else { video_width } as f32;
        let video_h = if video_height == 0 { 9 } else { video_height } as f32;
        let area_w = area.w.max(1.0);
        let area_h = area.h.max(1.0);
        let video_aspect = video_w / video_h;
        let area_aspect = area_w / area_h;
        if area_aspect > video_aspect {
            // 区域更宽 → 高度撑满，左右黑边
            let h = area_h;
            let w = h * video_aspect;
            VideoRect::new(area.x + (area_w - w) * 0.5, area.y, w, h)
        } else {
            // 区域更高 → 宽度撑满，上下黑边
            let w = area_w;
            let h = w / video_aspect;
            VideoRect::new(area.x, area.y + (area_h - h) * 0.5, w, h)
        }
    }
}

/// 一条待绘制的弹幕文字（egui 覆盖层的最小绘制单元）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DanmakuGlyph {
    pub text: String,
    /// UI 绝对坐标（文本左上角，点）。
    pub x: f32,
    pub y: f32,
    /// 实际字号（UI 点）。
    pub font_px: f32,
    /// RGB（egui `Color32::from_rgb` 三元组）。
    pub color: (u8, u8, u8),
    /// 不透明度：255=不透明（GUI 可再乘全局弹幕透明度）。
    pub alpha: u8,
}

/// `DanmakuGlyph` 的别名（Lead/接口描述里用过 `EguiGlyph` 这个名字）。
pub type EguiGlyph = DanmakuGlyph;

impl DanmakuGlyph {
    /// 文本估算宽度（UI 点）。用布局时的相对宽度乘以视频矩形宽度，
    /// 与 EGUI 实际字形宽度可能有几个像素差异（用于命中/调试，不用于排版）。
    pub fn width_px(&self, rect: VideoRect) -> f32 {
        // 宽度信息在 PlacedComment 里，glyph 自身不含；这里给出「按字号估算」的宽度：
        // CJK/全角 1em、ASCII/半角 0.5em（与布局引擎同一口径）。
        let em: f32 = self
            .text
            .chars()
            .map(|c| if (c as u32) < 0x2E80 { 0.5 } else { 1.0 })
            .sum::<f32>()
            .max(1.0);
        (em * self.font_px).min(rect.w.max(1.0))
    }
}

/// 覆盖层可选样式（默认值即 GUI 常规行为）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OverlayStyle {
    /// 字号缩放（对应 `LayoutConfig::font_scale`，默认 1.0）。
    pub font_scale: f32,
    /// 全局不透明度（0~255，默认 255）。
    pub alpha: u8,
}

impl Default for OverlayStyle {
    fn default() -> Self {
        Self {
            font_scale: 1.0,
            alpha: 255,
        }
    }
}

impl OverlayStyle {
    /// 指定字号缩放。
    pub fn with_font_scale(mut self, font_scale: f32) -> Self {
        if font_scale.is_finite() && font_scale > 0.0 {
            self.font_scale = font_scale;
        }
        self
    }

    /// 指定不透明度。
    pub fn with_alpha(mut self, alpha: u8) -> Self {
        self.alpha = alpha;
        self
    }
}

impl PlacedTimeline {
    /// 当前时刻要绘制的全部弹幕文字（默认样式；顺序即绘制顺序）。
    pub fn active_geometry(&self, at_ms: i64, rect: VideoRect) -> Vec<DanmakuGlyph> {
        self.active_geometry_with(at_ms, rect, &OverlayStyle::default())
    }

    /// 当前时刻要绘制的全部弹幕文字（指定字号缩放/透明度）。
    pub fn active_geometry_with(
        &self,
        at_ms: i64,
        rect: VideoRect,
        style: &OverlayStyle,
    ) -> Vec<DanmakuGlyph> {
        let mut indices = self.active_indices(at_ms);
        // 绘制顺序：滚动（layer 1）在下，顶部/底部（layer 2）在上
        indices.sort_by_key(|&i| {
            let placed = &self.placed()[i];
            (placed.comment.mode.is_fixed(), placed.start_ms)
        });
        indices
            .into_iter()
            .filter_map(|i| self.glyph_for(&self.placed()[i], at_ms, rect, style))
            .collect()
    }

    /// 单条弹幕在 `at_ms` 时刻的绘制数据（不在可见区间内返回 None）。
    pub fn glyph_for(
        &self,
        placed: &PlacedComment,
        at_ms: i64,
        rect: VideoRect,
        style: &OverlayStyle,
    ) -> Option<DanmakuGlyph> {
        if at_ms < placed.start_ms || at_ms >= placed.end_ms {
            return None;
        }
        let font_scale = if style.font_scale.is_finite() && style.font_scale > 0.0 {
            style.font_scale
        } else {
            1.0
        };
        let font_px = (placed.comment.font_size * font_scale * rect.font_scale()).max(1.0);
        let width_px = placed.width_ratio * rect.w.max(1.0);
        let (x_ratio, y_ratio) = match placed.comment.mode {
            DanmakuMode::Scroll => {
                let duration = (placed.end_ms - placed.start_ms).max(1) as f32;
                let progress = ((at_ms - placed.start_ms) as f32 / duration).clamp(0.0, 1.0);
                // 与 ASS 的 \move 同公式：左边界从 1.0 线性移到 -width_ratio
                let left_ratio = 1.0 - (1.0 + placed.width_ratio) * progress;
                (left_ratio, placed.y_ratio)
            }
            // 固定弹幕：水平居中、y 静止
            DanmakuMode::Top | DanmakuMode::Bottom => (
                ((rect.w - width_px) * 0.5).max(0.0) / rect.w.max(1.0),
                placed.y_ratio,
            ),
        };
        let color = placed.comment.color & 0xFFFFFF;
        Some(DanmakuGlyph {
            text: placed.comment.text.clone(),
            x: rect.x_at(x_ratio),
            y: rect.y_at(y_ratio),
            font_px,
            color: (
                ((color >> 16) & 0xFF) as u8,
                ((color >> 8) & 0xFF) as u8,
                (color & 0xFF) as u8,
            ),
            alpha: style.alpha,
        })
    }

    /// 命中测试（悬停暂停/复制用）：返回 `placed()` 中的下标。
    ///
    /// 规则：固定弹幕优先于滚动弹幕（固定层在上）；同层取 `start_ms` 更新的（后出现者在上）。
    pub fn hit_test(&self, at_ms: i64, rect: VideoRect, pointer: (f32, f32)) -> Option<usize> {
        self.hit_test_with(at_ms, rect, pointer, &OverlayStyle::default())
    }

    /// 带样式的命中测试。
    pub fn hit_test_with(
        &self,
        at_ms: i64,
        rect: VideoRect,
        pointer: (f32, f32),
        style: &OverlayStyle,
    ) -> Option<usize> {
        let mut best: Option<(usize, bool, i64)> = None; // (index, is_fixed, start_ms)
        for index in self.active_indices(at_ms) {
            let placed = &self.placed()[index];
            let Some(glyph) = self.glyph_for(placed, at_ms, rect, style) else {
                continue;
            };
            let width = placed.width_ratio * rect.w.max(1.0);
            let height = glyph.font_px;
            let inside = pointer.0 >= glyph.x
                && pointer.0 <= glyph.x + width
                && pointer.1 >= glyph.y
                && pointer.1 <= glyph.y + height;
            if !inside {
                continue;
            }
            let is_fixed = placed.comment.mode.is_fixed();
            let candidate = (index, is_fixed, placed.start_ms);
            let better = match best {
                None => true,
                Some((_, best_fixed, best_start)) => {
                    (is_fixed && !best_fixed)
                        || (is_fixed == best_fixed && placed.start_ms >= best_start)
                }
            };
            if better {
                best = Some(candidate);
            }
        }
        best.map(|(index, _, _)| index)
    }

    /// 命中测试并直接返回弹幕引用。
    pub fn hit_test_comment(
        &self,
        at_ms: i64,
        rect: VideoRect,
        pointer: (f32, f32),
    ) -> Option<&PlacedComment> {
        self.hit_test(at_ms, rect, pointer)
            .map(|index| &self.placed()[index])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::layout;
    use crate::model::{Comment, LayoutConfig};

    fn timeline_from_hd() -> PlacedTimeline {
        let comments = vec![
            Comment::new(0, DanmakuMode::Scroll, "滚动弹幕"),
            Comment::new(1000, DanmakuMode::Top, "顶部固定"),
            Comment::new(2000, DanmakuMode::Bottom, "底部固定"),
        ];
        PlacedTimeline::new(layout(&comments, &LayoutConfig::hd1080()))
    }

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn video_rect_fit_pillarbox_and_letterbox() {
        // 16:9 视频放进 1000x1000 → 上下黑边
        let rect = VideoRect::fit_video(VideoRect::from_size(1000.0, 1000.0), 3840, 2160);
        assert!(approx(rect.w, 1000.0));
        assert!(approx(rect.h, 562.5));
        assert!(approx(rect.y, 218.75));
        // 4:3 视频放进 1000x500 → 左右黑边
        let rect = VideoRect::fit_video(VideoRect::new(10.0, 20.0, 1000.0, 500.0), 1024, 768);
        assert!(approx(rect.h, 500.0));
        assert!(approx(rect.w, 666.6667));
        assert!(approx(rect.x, 10.0 + (1000.0 - 666.6667) / 2.0));
        assert!(approx(rect.y, 20.0));
    }

    #[test]
    fn video_rect_helpers() {
        let rect = VideoRect::new(100.0, 50.0, 1920.0, 1080.0);
        assert!(approx(rect.right(), 2020.0));
        assert!(approx(rect.bottom(), 1130.0));
        assert!(approx(rect.center_x(), 1060.0));
        assert!(approx(rect.x_at(0.5), 1060.0));
        assert!(approx(rect.y_at(0.0), 50.0));
        assert!(approx(rect.font_scale(), 1.0));
        assert!(rect.contains((100.0, 50.0)));
        assert!(!rect.contains((99.0, 50.0)));
    }

    #[test]
    fn font_scales_with_rect_height() {
        let timeline = timeline_from_hd();
        let style = OverlayStyle::default();
        let placed = &timeline.placed()[0];
        let small = timeline
            .glyph_for(placed, 0, VideoRect::from_size(1920.0, 540.0), &style)
            .unwrap();
        let large = timeline
            .glyph_for(placed, 0, VideoRect::from_size(1920.0, 1080.0), &style)
            .unwrap();
        assert!(approx(small.font_px, 25.0 * 0.5));
        assert!(approx(large.font_px, 25.0));
        // 窗口缩放后相对位置一致
        assert!(approx(small.x / 1920.0, large.x / 1920.0));
    }

    #[test]
    fn scroll_x_interpolates_linearly() {
        let placed = PlacedComment {
            comment: Comment::new(0, DanmakuMode::Scroll, "abc"),
            lane: 0,
            y_ratio: 0.1,
            width_ratio: 0.2,
            start_ms: 0,
            end_ms: 8000,
        };
        let timeline = PlacedTimeline::new(vec![placed.clone()]);
        let rect = VideoRect::from_size(1920.0, 1080.0);
        let style = OverlayStyle::default();
        let at = |ms| timeline.glyph_for(&placed, ms, rect, &style).unwrap();
        // t=0：左边界在屏幕右边缘（1.0）
        assert!(approx(at(0).x, 1920.0));
        // t=4000：progress=0.5 → left_ratio = 1 - 1.2*0.5 = 0.4 → 768
        assert!(approx(at(4000).x, 768.0));
        // t=8000：完全离开（end 是开区间，用 7999 检查）
        assert!(approx(at(7999).x, 1920.0 * (1.0 - 1.2 * 7999.0 / 8000.0)));
        assert!(timeline
            .glyph_for(&placed, 8000, rect, &OverlayStyle::default())
            .is_none());
    }

    #[test]
    fn fixed_glyph_is_static_and_centered() {
        let timeline = timeline_from_hd();
        let style = OverlayStyle::default();
        let rect = VideoRect::new(0.0, 0.0, 1920.0, 1080.0);
        let top_index = timeline
            .placed()
            .iter()
            .position(|p| p.comment.mode == DanmakuMode::Top)
            .unwrap();
        let placed = &timeline.placed()[top_index];
        let a = timeline
            .glyph_for(placed, placed.start_ms + 100, rect, &style)
            .unwrap();
        let b = timeline
            .glyph_for(placed, placed.end_ms - 100, rect, &style)
            .unwrap();
        assert!(approx(a.x, b.x));
        assert!(approx(a.y, b.y));
        // 居中：x = (W - text_w) / 2
        let expected = (1920.0 - placed.width_ratio * 1920.0) * 0.5;
        assert!(approx(a.x, expected));
    }

    #[test]
    fn active_geometry_ordering_and_count() {
        let timeline = timeline_from_hd();
        let rect = VideoRect::from_size(1920.0, 1080.0);
        // t=2500：滚动已 8000ms 结束？不会——滚动 0~8000 仍在；顶部 1000~5000；底部 2000~6000
        assert_eq!(timeline.active_count(2500), 3);
        let glyphs = timeline.active_geometry(2500, rect);
        assert_eq!(glyphs.len(), 3);
        // 绘制顺序：滚动在前，固定在后
        assert_eq!(glyphs.last().unwrap().text, "底部固定");
        assert_eq!(glyphs[0].text, "滚动弹幕");
        // 超出所有区间
        assert!(timeline.active_geometry(20_000, rect).is_empty());
        assert_eq!(timeline.active_count(20_000), 0);
    }

    #[test]
    fn boundary_times() {
        let placed = PlacedComment {
            comment: Comment::new(0, DanmakuMode::Top, "边界"),
            lane: 0,
            y_ratio: 0.1,
            width_ratio: 0.1,
            start_ms: 1000,
            end_ms: 5000,
        };
        let timeline = PlacedTimeline::new(vec![placed]);
        let rect = VideoRect::from_size(1920.0, 1080.0);
        assert!(timeline.active_geometry(999, rect).is_empty());
        assert_eq!(timeline.active_geometry(1000, rect).len(), 1);
        assert_eq!(timeline.active_geometry(4999, rect).len(), 1);
        assert!(timeline.active_geometry(5000, rect).is_empty());
    }

    #[test]
    fn color_and_alpha_in_glyph() {
        let mut comment = Comment::new(0, DanmakuMode::Top, "彩色");
        comment.color = 0xFF8800;
        let timeline = PlacedTimeline::new(layout(&[comment], &LayoutConfig::hd1080()));
        let style = OverlayStyle::default().with_alpha(128);
        let glyphs = timeline.active_geometry(0, VideoRect::from_size(1920.0, 1080.0));
        // 上面用 style 默认 alpha，这里单独验证 style
        assert_eq!(glyphs[0].color, (0xFF, 0x88, 0x00));
        let custom = timeline.active_geometry_with(0, VideoRect::from_size(1920.0, 1080.0), &style);
        assert_eq!(custom[0].alpha, 128);
        assert_eq!(glyphs[0].alpha, 255);
    }

    #[test]
    fn overlay_style_font_scale() {
        let placed = PlacedComment {
            comment: Comment::new(0, DanmakuMode::Top, "缩放"),
            lane: 0,
            y_ratio: 0.1,
            width_ratio: 0.1,
            start_ms: 0,
            end_ms: 1000,
        };
        let timeline = PlacedTimeline::new(vec![placed.clone()]);
        let style = OverlayStyle::default().with_font_scale(2.0);
        let glyph = timeline
            .glyph_for(&placed, 0, VideoRect::from_size(1920.0, 1080.0), &style)
            .unwrap();
        assert!(approx(glyph.font_px, 50.0));
        // 非法缩放回退 1.0
        let bad = OverlayStyle::default().with_font_scale(f32::NAN);
        assert!(approx(bad.font_scale, 1.0));
    }

    #[test]
    fn hit_test_inside_and_outside() {
        let timeline = timeline_from_hd();
        let rect = VideoRect::from_size(1920.0, 1080.0);
        // t=500：只有滚动弹幕；取它的 glyph 位置，点在文字中心
        let glyph = timeline.active_geometry(500, rect)[0].clone();
        let width = glyph.width_px(rect);
        let inside = (glyph.x + width * 0.5, glyph.y + glyph.font_px * 0.5);
        let hit = timeline.hit_test(500, rect, inside).unwrap();
        assert_eq!(timeline.placed()[hit].comment.text, "滚动弹幕");
        // 点在文字外
        assert!(timeline
            .hit_test(500, rect, (glyph.x - 50.0, glyph.y - 50.0))
            .is_none());
        // 点在没有弹幕的时间
        assert!(timeline.hit_test(30_000, rect, inside).is_none());
    }

    #[test]
    fn hit_test_prefers_fixed_over_scroll() {
        let rect = VideoRect::from_size(1920.0, 1080.0);
        // 手工构造同一位置、同一时间的滚动 + 顶部
        let scroll = PlacedComment {
            comment: Comment::new(0, DanmakuMode::Scroll, "滚动"),
            lane: 0,
            y_ratio: 0.05,
            width_ratio: 0.5,
            start_ms: 0,
            end_ms: 8000,
        };
        let fixed = PlacedComment {
            comment: Comment::new(0, DanmakuMode::Top, "顶部"),
            lane: 0,
            y_ratio: 0.05,
            width_ratio: 0.5,
            start_ms: 0,
            end_ms: 4000,
        };
        let timeline = PlacedTimeline::new(vec![scroll, fixed]);
        let style = OverlayStyle::default();
        // t=3390：滚动弹幕左边界 ≈ 700（宽 960 → 覆盖 700~1660），
        // 顶部固定弹幕居中（覆盖 480~1440），两者在 700~1440 重叠
        let scroll_glyph = timeline
            .glyph_for(&timeline.placed()[0], 3390, rect, &style)
            .unwrap();
        let pointer = (scroll_glyph.x + 10.0, scroll_glyph.y + 5.0);
        let hit = timeline.hit_test(3390, rect, pointer).unwrap();
        assert_eq!(timeline.placed()[hit].comment.mode, DanmakuMode::Top);
        assert_eq!(
            timeline
                .hit_test_comment(3390, rect, pointer)
                .unwrap()
                .comment
                .text,
            "顶部"
        );
    }

    #[test]
    fn four_k_and_1080p_same_on_screen() {
        let comments: Vec<Comment> = (0..30)
            .map(|i| {
                let mode = match i % 3 {
                    0 => DanmakuMode::Scroll,
                    1 => DanmakuMode::Top,
                    _ => DanmakuMode::Bottom,
                };
                Comment::new(i * 300, mode, format!("弹幕{i}"))
            })
            .collect();
        let hd = PlacedTimeline::new(layout(&comments, &LayoutConfig::hd1080()));
        let uhd = PlacedTimeline::new(layout(&comments, &LayoutConfig::uhd4k()));
        let rect = VideoRect::new(100.0, 50.0, 1280.0, 720.0);
        for at in [0_i64, 1000, 3000, 6000] {
            let a = hd.active_geometry(at, rect);
            let b = uhd.active_geometry(at, rect);
            assert_eq!(a, b, "4K/1080p 在同一窗口矩形下几何不一致 at={at}");
        }
    }

    #[test]
    fn window_scaling_consistency() {
        let timeline = timeline_from_hd();
        let base = VideoRect::from_size(1920.0, 1080.0);
        let half = VideoRect::from_size(960.0, 540.0);
        let a = timeline.active_geometry(1000, base);
        let b = timeline.active_geometry(1000, half);
        assert_eq!(a.len(), b.len());
        for (ga, gb) in a.iter().zip(b.iter()) {
            assert!(approx(ga.x / 1920.0, gb.x / 960.0));
            assert!(approx(ga.y / 1080.0, gb.y / 540.0));
            assert!(approx(ga.font_px * 0.5, gb.font_px));
        }
    }

    #[test]
    fn empty_timeline() {
        let timeline = PlacedTimeline::new(vec![]);
        let rect = VideoRect::from_size(1920.0, 1080.0);
        assert!(timeline.active_geometry(0, rect).is_empty());
        assert_eq!(timeline.active_count(0), 0);
        assert!(timeline.hit_test(0, rect, (10.0, 10.0)).is_none());
    }
}
