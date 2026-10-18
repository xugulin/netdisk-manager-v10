//! 弹幕数据模型与公共类型（对外公开类型从 crate 根 re-export）。
//!
//! 冻结类型（`DanmakuMode` / `Comment` / `DanmakuSource` / `DanmakuDoc` /
//! `LayoutConfig` / `PlacedComment`）保持任务书定义的字段语义不变；
//! 本模块在此基础上追加策略/报告/渲染选项等新类型。

use serde::{Deserialize, Serialize};

/// 弹幕显示模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DanmakuMode {
    /// 滚动（右→左）。
    #[default]
    Scroll,
    /// 顶部固定。
    Top,
    /// 底部固定。
    Bottom,
}

impl DanmakuMode {
    /// B 站 XML `p` 字段的 mode 整数 → 模式。
    ///
    /// 1/2/3 = 滚动，4 = 底部，5 = 顶部，6（逆向滚动）降级为滚动，其余（高级/脚本弹幕）返回 None。
    pub fn from_bilibili_code(code: i64) -> Option<DanmakuMode> {
        match code {
            1 | 2 | 3 | 6 => Some(DanmakuMode::Scroll),
            4 => Some(DanmakuMode::Bottom),
            5 => Some(DanmakuMode::Top),
            _ => None,
        }
    }

    /// 模式 → B 站 XML mode 整数（逆向滚动导出为普通滚动）。
    pub fn to_bilibili_code(self) -> i64 {
        match self {
            DanmakuMode::Scroll => 1,
            DanmakuMode::Top => 5,
            DanmakuMode::Bottom => 4,
        }
    }

    /// 中文标签（面板展示用）。
    pub fn label(self) -> &'static str {
        match self {
            DanmakuMode::Scroll => "滚动",
            DanmakuMode::Top => "顶部",
            DanmakuMode::Bottom => "底部",
        }
    }

    /// 是否为固定弹幕（顶部/底部）。
    pub fn is_fixed(self) -> bool {
        matches!(self, DanmakuMode::Top | DanmakuMode::Bottom)
    }
}

/// 单条弹幕。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    /// 出现时间（毫秒，相对视频起点）。
    pub time_ms: i64,
    pub mode: DanmakuMode,
    /// RGB 颜色（0xRRGGBB）。
    pub color: u32,
    /// 字号（以 1080p 为基准的相对字号，默认 25）。
    pub font_size: f32,
    pub text: String,
    /// 发送者（可选）。
    pub user: Option<String>,
}

impl Default for Comment {
    fn default() -> Self {
        Self {
            time_ms: 0,
            mode: DanmakuMode::Scroll,
            color: 0xFFFFFF,
            font_size: 25.0,
            text: String::new(),
            user: None,
        }
    }
}

impl Comment {
    /// 快捷构造。
    pub fn new(time_ms: i64, mode: DanmakuMode, text: impl Into<String>) -> Self {
        Self {
            time_ms,
            mode,
            text: text.into(),
            ..Comment::default()
        }
    }

    /// 设置颜色（`0xRRGGBB`）。
    pub fn with_color(mut self, color: u32) -> Self {
        self.color = color;
        self
    }

    /// 规范化：时间不为负、字号在合理范围、文本去首尾空白。
    pub fn normalized(&self) -> Comment {
        let font_size = if self.font_size.is_finite() {
            self.font_size.clamp(1.0, 200.0)
        } else {
            25.0
        };
        Comment {
            time_ms: self.time_ms.max(0),
            mode: self.mode,
            color: self.color & 0xFFFFFF,
            font_size,
            text: self.text.trim().to_string(),
            user: self.user.clone(),
        }
    }
}

/// 弹幕来源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DanmakuSource {
    #[default]
    LocalFile,
    BilibiliXml,
    Online(String),
    Generated,
}

/// 弹幕文档。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DanmakuDoc {
    pub source: DanmakuSource,
    pub comments: Vec<Comment>,
}

impl DanmakuDoc {
    /// 构造。
    pub fn new(source: DanmakuSource, comments: Vec<Comment>) -> Self {
        Self { source, comments }
    }

    /// 条数。
    pub fn len(&self) -> usize {
        self.comments.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.comments.is_empty()
    }

    /// 汇总来源的一份文档（合并本地/在线弹幕时用）。
    pub fn from_comments(comments: Vec<Comment>) -> Self {
        Self {
            source: DanmakuSource::LocalFile,
            comments,
        }
    }

    /// 按时间排序（稳定；同时间保持原顺序）。
    pub fn sort_by_time(&mut self) {
        self.comments.sort_by_key(|c| c.time_ms.max(0));
    }

    /// 规范化全部弹幕并排序。
    pub fn normalize(&mut self) {
        self.comments = self.comments.iter().map(|c| c.normalized()).collect();
        self.sort_by_time();
    }
}

/// 布局配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutConfig {
    pub video_width: u32,
    pub video_height: u32,
    /// 滚动弹幕穿越屏幕时长（毫秒）。
    pub scroll_duration_ms: i64,
    /// 顶部/底部固定弹幕停留时长（毫秒）。
    pub fixed_duration_ms: i64,
    /// 同屏最大弹幕数。
    pub density_limit: usize,
    /// 显示区域（0~1，留出上下边）。
    pub area_top: f32,
    pub area_bottom: f32,
    /// 字号缩放。
    pub font_scale: f32,
    /// 速度倍率（>1 更快）。
    pub speed: f32,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            video_width: 3840,
            video_height: 2160,
            scroll_duration_ms: 8000,
            fixed_duration_ms: 4000,
            density_limit: 160,
            area_top: 0.03,
            area_bottom: 0.25,
            font_scale: 1.0,
            speed: 1.0,
        }
    }
}

impl LayoutConfig {
    /// 常用分辨率预设（1080p / 4K）。
    pub fn for_resolution(width: u32, height: u32) -> Self {
        Self {
            video_width: width,
            video_height: height,
            ..LayoutConfig::default()
        }
    }

    /// 1080p 布局配置。
    pub fn hd1080() -> Self {
        Self::for_resolution(1920, 1080)
    }

    /// 4K 布局配置。
    pub fn uhd4k() -> Self {
        Self::for_resolution(3840, 2160)
    }

    /// 折叠非法值（宽高为 0、时长非正、倍率非正等），返回可安全计算的副本。
    pub fn sanitized(&self) -> LayoutConfig {
        let mut cfg = self.clone();
        if cfg.video_width == 0 {
            cfg.video_width = 1920;
        }
        if cfg.video_height == 0 {
            cfg.video_height = 1080;
        }
        if cfg.scroll_duration_ms <= 0 {
            cfg.scroll_duration_ms = 8000;
        }
        if cfg.fixed_duration_ms <= 0 {
            cfg.fixed_duration_ms = 4000;
        }
        cfg.area_top = if cfg.area_top.is_finite() {
            cfg.area_top.clamp(0.0, 0.9)
        } else {
            0.0
        };
        cfg.area_bottom = if cfg.area_bottom.is_finite() {
            cfg.area_bottom.clamp(0.0, 0.9)
        } else {
            0.25
        };
        if cfg.area_top + cfg.area_bottom > 0.95 {
            cfg.area_top = 0.0;
            cfg.area_bottom = 0.05;
        }
        if !cfg.font_scale.is_finite() || cfg.font_scale <= 0.0 {
            cfg.font_scale = 1.0;
        }
        if !cfg.speed.is_finite() || cfg.speed <= 0.0 {
            cfg.speed = 1.0;
        }
        cfg
    }
}

/// 布局后的弹幕（供 egui 渲染或 ASS 生成）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacedComment {
    pub comment: Comment,
    /// 车道序号（0 起）。
    pub lane: usize,
    /// y 坐标（相对视频高度 0~1，文本顶部）。
    pub y_ratio: f32,
    /// 文本宽度（相对视频宽度 0~1，估算值）。
    pub width_ratio: f32,
    /// 渲染开始/结束（毫秒，已含固定/滚动时长）。
    pub start_ms: i64,
    pub end_ms: i64,
}

impl PlacedComment {
    /// 时长（毫秒）。
    pub fn duration_ms(&self) -> i64 {
        (self.end_ms - self.start_ms).max(0)
    }

    /// 某时刻是否可见。
    pub fn visible_at(&self, time_ms: i64) -> bool {
        self.start_ms <= time_ms && self.end_ms > time_ms
    }
}

/// 密度超限丢弃策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DensityPolicy {
    /// 丢弃新弹幕（默认；结果稳定可复现）。
    #[default]
    DropNew,
    /// 保高权重：新弹幕权重高于同屏最低权重时替换它，否则丢弃新弹幕。
    KeepHighPriority,
    /// 保随机：按（时间+文本）稳定哈希决定保留新的还是丢新的（约 50%）。
    KeepRandom,
}

/// 布局统计（面板诊断/单测断言用）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LayoutStats {
    /// 输入弹幕数。
    pub input_count: usize,
    /// 成功布局数。
    pub placed_count: usize,
    /// 因无可用车道丢弃数。
    pub dropped_no_lane: usize,
    /// 因密度上限丢弃数。
    pub dropped_density: usize,
    /// 空文本跳过的数量。
    pub skipped_empty: usize,
    /// 车道总数（滚动）。
    pub lane_count: usize,
}

/// 生成 ASS 弹幕轨的选项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssRenderOptions {
    /// 时间窗裁剪（视频绝对毫秒，闭开区间 `[start, end)`）；None = 全部。
    ///
    /// 用于分片 `sub-reload`：只导出窗口内活跃的弹幕。**时间保持视频绝对时间**，
    /// 因为 mpv/libass 是按媒体时间戳比较 ASS 事件的，做 rebase 反而会错位。
    pub window: Option<(i64, i64)>,
    /// 透明度（ASS `\alpha`，0=不透明，255=全透明）。
    pub alpha: u8,
    pub font_name: String,
    /// 描边宽度（PlayRes 像素）。
    pub outline: f32,
    /// 阴影宽度。
    pub shadow: f32,
}

impl Default for AssRenderOptions {
    fn default() -> Self {
        Self {
            window: None,
            alpha: 0,
            font_name: "Noto Sans CJK SC".into(),
            outline: 1.5,
            shadow: 0.0,
        }
    }
}

impl AssRenderOptions {
    /// 只导出指定时间窗的配置。
    pub fn windowed(start_ms: i64, end_ms: i64) -> Self {
        Self {
            window: Some((start_ms, end_ms)),
            ..Self::default()
        }
    }
}

/// 布局引擎内部几何参数（公开给渲染/面板做坐标换算）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LayoutGeometry {
    /// 车道高度（相对视频高度）。
    pub lane_height_ratio: f32,
    /// 车道数。
    pub lane_count: usize,
    /// 显示区域上边界（相对高度）。
    pub region_top: f32,
    /// 显示区域下边界（相对高度）。
    pub region_bottom: f32,
    /// 滚动弹幕实际时长（毫秒，已应用 speed）。
    pub scroll_duration_ms: i64,
    /// 固定弹幕实际时长（毫秒）。
    pub fixed_duration_ms: i64,
}

impl LayoutGeometry {
    /// 车道 `lane` 的文本顶部 y（相对高度，滚动/顶部用）。
    pub fn lane_top(&self, lane: usize) -> f32 {
        self.region_top + lane as f32 * self.lane_height_ratio
    }

    /// 底部弹幕车道 `lane` 的文本顶部 y（相对高度，lane 0 在最底部）。
    pub fn bottom_lane_top(&self, lane: usize) -> f32 {
        self.region_bottom - (lane as f32 + 1.0) * self.lane_height_ratio
    }
}

/// 弹幕权重（密度超限「保高权重」策略用）：
/// 大字号 / 彩色 / 顶部底部固定 / 文本长度适中 的弹幕权重更高。
pub fn comment_weight(comment: &Comment) -> f32 {
    let mut weight = 1.0;
    weight += ((comment.font_size - 25.0) / 25.0).max(0.0) * 0.5;
    if comment.color != 0xFFFFFF {
        weight += 0.3;
    }
    if comment.mode.is_fixed() {
        weight += 0.1;
    }
    let len = comment.text.chars().count() as f32;
    weight += (len / 20.0).min(1.0) * 0.2;
    weight
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_mapping() {
        assert_eq!(
            DanmakuMode::from_bilibili_code(1),
            Some(DanmakuMode::Scroll)
        );
        assert_eq!(
            DanmakuMode::from_bilibili_code(2),
            Some(DanmakuMode::Scroll)
        );
        assert_eq!(
            DanmakuMode::from_bilibili_code(4),
            Some(DanmakuMode::Bottom)
        );
        assert_eq!(DanmakuMode::from_bilibili_code(5), Some(DanmakuMode::Top));
        // 逆向滚动降级
        assert_eq!(
            DanmakuMode::from_bilibili_code(6),
            Some(DanmakuMode::Scroll)
        );
        // 高级/脚本弹幕不支持
        assert_eq!(DanmakuMode::from_bilibili_code(7), None);
        assert_eq!(DanmakuMode::from_bilibili_code(8), None);
        assert_eq!(DanmakuMode::Scroll.to_bilibili_code(), 1);
        assert_eq!(DanmakuMode::Top.to_bilibili_code(), 5);
        assert_eq!(DanmakuMode::Bottom.to_bilibili_code(), 4);
    }

    #[test]
    fn comment_normalize() {
        let c = Comment {
            time_ms: -100,
            color: 0xFF00FF00,
            font_size: f32::NAN,
            text: "  hi  ".into(),
            ..Comment::default()
        };
        let n = c.normalized();
        assert_eq!(n.time_ms, 0);
        assert_eq!(n.font_size, 25.0);
        assert_eq!(n.color, 0x00FF00);
        assert_eq!(n.text, "hi");
    }

    #[test]
    fn config_sanitized() {
        let cfg = LayoutConfig {
            video_width: 0,
            video_height: 0,
            scroll_duration_ms: 0,
            fixed_duration_ms: -1,
            density_limit: 0,
            area_top: 0.6,
            area_bottom: 0.6,
            font_scale: 0.0,
            speed: -2.0,
        }
        .sanitized();
        assert_eq!(cfg.video_width, 1920);
        assert_eq!(cfg.video_height, 1080);
        assert_eq!(cfg.scroll_duration_ms, 8000);
        assert_eq!(cfg.fixed_duration_ms, 4000);
        assert_eq!(cfg.font_scale, 1.0);
        assert_eq!(cfg.speed, 1.0);
        assert!(cfg.area_top + cfg.area_bottom <= 0.95);
    }

    #[test]
    fn weight_ordering() {
        let plain = Comment::new(0, DanmakuMode::Scroll, "普通");
        let mut big_colored =
            Comment::new(0, DanmakuMode::Scroll, "彩色大字号").with_color(0xFF0000);
        big_colored.font_size = 40.0;
        assert!(comment_weight(&big_colored) > comment_weight(&plain));
    }

    #[test]
    fn doc_normalize_sorts() {
        let mut doc = DanmakuDoc::from_comments(vec![
            Comment::new(2000, DanmakuMode::Scroll, "b"),
            Comment::new(1000, DanmakuMode::Scroll, "a"),
        ]);
        doc.normalize();
        assert_eq!(doc.comments[0].text, "a");
        assert_eq!(doc.len(), 2);
        assert!(!doc.is_empty());
        assert!(DanmakuDoc::default().is_empty());
    }

    #[test]
    fn geometry_positions() {
        let g = LayoutGeometry {
            lane_height_ratio: 0.05,
            lane_count: 4,
            region_top: 0.1,
            region_bottom: 0.8,
            scroll_duration_ms: 8000,
            fixed_duration_ms: 4000,
        };
        assert!((g.lane_top(0) - 0.1).abs() < 1e-6);
        assert!((g.lane_top(2) - 0.2).abs() < 1e-6);
        assert!((g.bottom_lane_top(0) - 0.75).abs() < 1e-6);
        assert!((g.bottom_lane_top(1) - 0.70).abs() < 1e-6);
    }
}
