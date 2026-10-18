//! 弹幕子系统（纯逻辑）：模型/解析/车道布局/ASS 生成。
//!
//! 渲染在 GUI 侧：Mode A 用 egui 覆盖层（`wp-gui`），Mode B 用本 crate 生成的 ASS
//! 作为副字幕轨交给 mpv 渲染。任务 T22–T27。

use serde::{Deserialize, Serialize};
use wp_core::{Error, Result};

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

/// 解析 B 站 XML 弹幕（T22 实现）。
pub fn parse_bilibili_xml(_xml: &str) -> Result<DanmakuDoc> {
    Err(Error::unsupported("B站 XML 解析尚未实现（T22 任务）"))
}

/// 解析 JSON 弹幕（T22 实现）。
pub fn parse_json(_json: &str) -> Result<DanmakuDoc> {
    Err(Error::unsupported("JSON 弹幕解析尚未实现（T22 任务）"))
}

/// 车道布局（T23 实现）。
pub fn layout(_comments: &[Comment], _cfg: &LayoutConfig) -> Vec<PlacedComment> {
    Vec::new()
}

/// 生成 ASS 弹幕轨（T25 实现）。
pub fn generate_ass(_placed: &[PlacedComment], _cfg: &LayoutConfig) -> String {
    String::new()
}

/// 导出 B 站 XML（T22 实现）。
pub fn export_bilibili_xml(_doc: &DanmakuDoc) -> String {
    String::new()
}
