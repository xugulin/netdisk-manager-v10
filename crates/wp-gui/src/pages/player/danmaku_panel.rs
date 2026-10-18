//! 弹幕面板（T26 UI）：开关 / 透明度 / 速度 / 密度 / 显示区域 / 屏蔽词 / 时间轴密度占位。
//!
//! 渲染数据当前用 [`super::overlay::MOCK_DANMAKU`] 做视觉验证；
//! 真实数据接入点：`wp_danmaku::layout(comments, &LayoutConfig) -> Vec<PlacedComment>`
//! （T23 完成后替换 overlay::paint_danmaku 的取数逻辑），本面板的配置项
//! （opacity/speed/density/area/font_scale/屏蔽词）与 `wp_danmaku::LayoutConfig` 字段一一对应。

use super::{PlayerAction, PlayerUi};
use crate::theme::Theme;
use crate::widgets;
use egui::{Color32, Pos2, RichText, Stroke, Ui, Vec2};
use wp_media::PlayerCommand;

/// 弹幕面板状态（跨媒体保留用户偏好）。
pub struct DanmakuUi {
    /// 总开关（D 快捷键）。
    pub enabled: bool,
    /// 不透明度 0.05~1.0。
    pub opacity: f32,
    /// 滚动速度倍率 0.5~2.0（>1 更快）。
    pub speed: f32,
    /// 同屏上限（密度限制）。
    pub density: usize,
    /// 字号缩放 0.6~2.0。
    pub font_scale: f32,
    /// 显示区域顶部留白（0~0.45）。
    pub area_top: f32,
    /// 显示区域底部留白（0~0.7）。
    pub area_bottom: f32,
    /// 屏蔽词（每行一个，也支持逗号分隔）。
    pub blocked: String,
    /// 是否显示时间轴密度条。
    pub show_timeline: bool,
    /// 时间轴偏移（毫秒；弹幕相对视频整体提前/延后，真实接入后用于排布）。
    pub time_offset_ms: i32,
}

impl Default for DanmakuUi {
    fn default() -> Self {
        Self {
            enabled: true,
            opacity: 0.85,
            speed: 1.0,
            density: 160,
            font_scale: 1.0,
            area_top: 0.03,
            area_bottom: 0.25,
            blocked: String::new(),
            show_timeline: true,
            time_offset_ms: 0,
        }
    }
}

impl DanmakuUi {
    /// 解析屏蔽词：换行 / 中英文逗号 / 分号 / 空格分隔，全部转小写去重。
    pub fn blocked_words(&self) -> Vec<String> {
        let mut words: Vec<String> = self
            .blocked
            .split(['\n', '\r', ',', '，', ';', '；'])
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        words.sort();
        words.dedup();
        words
    }

    /// 是否屏蔽某条弹幕文本。
    pub fn is_blocked(&self, text: &str) -> bool {
        let lower = text.to_lowercase();
        self.blocked_words()
            .iter()
            .any(|word| !word.is_empty() && lower.contains(word.as_str()))
    }
}

/// 时间轴密度柱：调用 wp-danmaku T23 `density_curve` 对评论分桶统计，
/// 再归一化到 `n` 根柱子（供热力图占位/后续搜索跳转用）。
pub fn density_bars(comments: &[wp_danmaku::Comment], cycle_ms: i64, n: usize) -> Vec<f32> {
    if n == 0 || comments.is_empty() {
        return vec![0.0; n];
    }
    let curve = wp_danmaku::density_curve(comments, 100);
    let max = curve
        .iter()
        .map(|(_, count)| *count)
        .max()
        .unwrap_or(0)
        .max(1);
    let mut bars = vec![0.0_f32; n];
    for (time_ms, count) in curve {
        let idx = (time_ms.rem_euclid(cycle_ms.max(1)) * n as i64 / cycle_ms.max(1)) as usize;
        if idx < n {
            bars[idx] = bars[idx].max(count as f32 / max as f32);
        }
    }
    bars
}

/// 渲染弹幕面板。
pub fn show(player: &mut PlayerUi, t: &Theme, ui: &mut Ui, actions: &mut Vec<PlayerAction>) {
    let mut cmds: Vec<PlayerCommand> = Vec::new();
    widgets::section_title(ui, t, "弹幕");
    ui.add_space(2.0);

    if ui
        .checkbox(&mut player.danmaku.enabled, "弹幕显示（D）")
        .on_hover_text("键盘 D 快速开关；关闭后视频区不再绘制弹幕覆盖层")
        .changed()
    {
        // 弹幕轨在 Mode B 走副字幕 ASS 注入，这里先仅本地开关；真实内核接入后发对应命令。
        let _ = &mut cmds;
    }
    widgets::hint(
        ui,
        t,
        "当前渲染：egui 覆盖层（Mode A）；Mode B（mpv 副字幕轨）由 T25 生成 ASS 后接入。",
    );

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("不透明度").size(12.0).color(t.text_weak));
        ui.add(
            egui::Slider::new(&mut player.danmaku.opacity, 0.05..=1.0)
                .show_value(true)
                .fixed_decimals(2),
        );
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("速度").size(12.0).color(t.text_weak));
        ui.add(
            egui::Slider::new(&mut player.danmaku.speed, 0.5..=2.0)
                .show_value(true)
                .fixed_decimals(2)
                .suffix("x"),
        );
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("密度上限").size(12.0).color(t.text_weak));
        let mut density = player.danmaku.density as i32;
        if ui
            .add(egui::Slider::new(&mut density, 20..=400).suffix(" 条/屏"))
            .changed()
        {
            player.danmaku.density = density.max(1) as usize;
        }
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("字号缩放").size(12.0).color(t.text_weak));
        ui.add(
            egui::Slider::new(&mut player.danmaku.font_scale, 0.6..=2.0)
                .show_value(true)
                .fixed_decimals(2)
                .suffix("x"),
        );
    });

    // ── 显示区域 ──
    ui.add_space(6.0);
    ui.label(RichText::new("显示区域").size(13.0).strong().color(t.text));
    ui.horizontal(|ui| {
        ui.label(RichText::new("顶部留白").size(12.0).color(t.text_weak));
        ui.add(egui::Slider::new(&mut player.danmaku.area_top, 0.0..=0.45).fixed_decimals(2));
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("底部留白").size(12.0).color(t.text_weak));
        ui.add(egui::Slider::new(&mut player.danmaku.area_bottom, 0.0..=0.70).fixed_decimals(2));
    });
    if player.danmaku.area_top + player.danmaku.area_bottom > 0.9 {
        // 兜底：留白过大时保证至少 10% 画面可用于弹幕
        player.danmaku.area_bottom = (0.9 - player.danmaku.area_top).max(0.0);
    }

    // ── 时间轴偏移 ──
    ui.horizontal(|ui| {
        ui.label(RichText::new("时间轴偏移").size(12.0).color(t.text_weak));
        ui.add(
            egui::DragValue::new(&mut player.danmaku.time_offset_ms)
                .range(-10_000..=10_000)
                .speed(10.0)
                .suffix(" ms"),
        );
        if ui.small_button("归零").clicked() {
            player.danmaku.time_offset_ms = 0;
        }
    });

    // ── 屏蔽词 ──
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("屏蔽词").size(13.0).strong().color(t.text));
        let count = player.danmaku.blocked_words().len();
        let blocked = player
            .demo_comments
            .iter()
            .filter(|comment| player.danmaku.is_blocked(&comment.text))
            .count();
        widgets::hint(
            ui,
            t,
            &format!("已生效 {count} 个（包含匹配不区分大小写）· 当前命中 {blocked} 条"),
        );
    });
    ui.add(
        egui::TextEdit::multiline(&mut player.danmaku.blocked)
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .hint_text("每行一个，或用逗号分隔。例如：剧透\n广告\n前面高能"),
    );

    // ── 时间轴密度占位图 ──
    ui.add_space(6.0);
    ui.checkbox(&mut player.danmaku.show_timeline, "显示时间轴密度");
    if player.danmaku.show_timeline {
        paint_timeline(ui, t, player, &cmds);
    }
    widgets::hint(
        ui,
        t,
        "密度曲线由 wp-danmaku 布局引擎实时统计；本地弹幕库/搜索跳转/在线源在 T26 完整实现。",
    );

    for cmd in cmds {
        actions.push(PlayerAction::Command(cmd));
    }
}

/// 弹幕时间轴密度占位图：60 个分桶柱 + 当前播放位置游标。
fn paint_timeline(ui: &mut Ui, t: &Theme, player: &PlayerUi, _cmds: &[PlayerCommand]) {
    let width = ui.available_width().max(120.0);
    let height = 56.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, crate::theme::radius(6), t.input);
    painter.rect_stroke(
        rect,
        crate::theme::radius(6),
        Stroke::new(1.0, t.border),
        egui::StrokeKind::Inside,
    );

    // 真实密度曲线（wp-danmaku T23）：演示弹幕按 12s 循环统计
    let bars = density_bars(&player.demo_comments, 12_000, 60);
    let inner = rect.shrink2(Vec2::new(4.0, 5.0));
    let bw = inner.width() / bars.len() as f32;
    for (i, v) in bars.iter().enumerate() {
        let h = (inner.height() * 0.92 * v).max(1.0);
        let x = inner.left() + bw * i as f32;
        let bar = egui::Rect::from_min_max(
            Pos2::new(x, inner.bottom() - h),
            Pos2::new(x + (bw - 1.0).max(1.0), inner.bottom()),
        );
        let color = if *v > 0.6 { t.warning } else { t.accent };
        painter.rect_filled(
            bar,
            1,
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 170),
        );
    }

    // 播放位置游标
    let ratio = if player.duration > 0.0 {
        (player.position / player.duration).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    let x = inner.left() + inner.width() * ratio;
    painter.line_segment(
        [Pos2::new(x, inner.top()), Pos2::new(x, inner.bottom())],
        Stroke::new(1.5, t.success),
    );
    painter.text(
        Pos2::new(rect.left() + 4.0, rect.top() + 2.0),
        egui::Align2::LEFT_TOP,
        format!(
            "密度（wp-danmaku）· 同屏 {} 条 · 播放 {:.0}%",
            player.active_danmaku_count(),
            ratio * 100.0
        ),
        egui::FontId::proportional(9.5),
        t.text_weak,
    );
    if resp.hovered() {
        resp.on_hover_text("时间轴密度热力图：真实实现按 wp-danmaku 评论分桶统计（T26）");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_words_parse_multiple_separators() {
        let ui = DanmakuUi {
            blocked: "剧透, 广告\n前面高能；spam, SPAM".into(),
            ..Default::default()
        };
        let words = ui.blocked_words();
        assert_eq!(words.len(), 4, "{words:?}");
        assert!(words.contains(&"剧透".to_string()));
        assert!(words.contains(&"spam".to_string()));
    }

    #[test]
    fn is_blocked_case_insensitive_contains() {
        let ui = DanmakuUi {
            blocked: "广告\nSPAM".into(),
            ..Default::default()
        };
        assert!(ui.is_blocked("这是一条广告弹幕"));
        assert!(ui.is_blocked("spam spam"));
        assert!(!ui.is_blocked("正常弹幕"));
        assert!(!DanmakuUi::default().is_blocked("任意内容"));
    }

    #[test]
    fn density_bars_from_danmaku_curve() {
        let comments: Vec<wp_danmaku::Comment> = vec![
            wp_danmaku::Comment::new(500, wp_danmaku::DanmakuMode::Scroll, "a"),
            wp_danmaku::Comment::new(1200, wp_danmaku::DanmakuMode::Scroll, "b"),
            wp_danmaku::Comment::new(1250, wp_danmaku::DanmakuMode::Top, "c"),
            wp_danmaku::Comment::new(9000, wp_danmaku::DanmakuMode::Scroll, "d"),
        ];
        let bars = density_bars(&comments, 12_000, 60);
        assert_eq!(bars.len(), 60);
        assert!(bars.iter().all(|v| (0.0..=1.0).contains(v)));
        // 500ms 的弹幕落在第 2 根柱（500*60/12000 = 2）
        let idx = (500_i64 * 60 / 12_000) as usize;
        assert!(bars[idx] > 0.0, "500ms 附近密度为 0：{bars:?}");
        // 空输入安全
        assert_eq!(density_bars(&[], 12_000, 60), vec![0.0; 60]);
    }

    #[test]
    fn blocked_words_dedup_and_normalize() {
        let ui = DanmakuUi {
            blocked: "  A ,a,，A\n".into(),
            ..Default::default()
        };
        assert_eq!(ui.blocked_words(), vec!["a".to_string()]);
    }
}
