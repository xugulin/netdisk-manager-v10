//! 主题系统：10 套配色（与 V2「主题管理器」对齐），映射到 egui Visuals。
//!
//! 每套主题给出一份完整语义色板（背景/卡片/文字/主色/状态色…），
//! GUI 里所有自定义绘制都从这里取色，保证切换主题整体一致。

use egui::{Color32, CornerRadius, Stroke};

/// 一套主题的语义色板。
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// 内部名
    pub name: &'static str,
    /// 显示名（带图标前缀，egui 内置 emoji 字体可显示一部分）
    pub display: &'static str,
    pub dark: bool,
    /// 页面背景
    pub bg: Color32,
    /// 卡片/面板背景
    pub card: Color32,
    /// 输入框背景
    pub input: Color32,
    /// 主文字
    pub text: Color32,
    /// 次要文字
    pub text_weak: Color32,
    /// 边框
    pub border: Color32,
    /// 选中背景
    pub selection: Color32,
    /// 选中文字
    pub selection_text: Color32,
    /// 悬停背景
    pub hover: Color32,
    /// 表头背景
    pub header_bg: Color32,
    /// 主色/强调色
    pub accent: Color32,
    /// 主色悬停
    #[allow(dead_code)]
    pub accent_hover: Color32,
    /// 导航栏背景
    pub nav_bg: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub error: Color32,
}

const fn c(hex: u32) -> Color32 {
    Color32::from_rgb(
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}

/// 全部内置主题（顺序即设置页下拉顺序）。
pub const THEMES: [Theme; 10] = [
    Theme {
        name: "dracula",
        display: "🧛 Dracula",
        dark: true,
        bg: c(0x282a36),
        card: c(0x21222c),
        input: c(0x282a36),
        text: c(0xf8f8f2),
        text_weak: c(0x6272a4),
        border: c(0x44475a),
        selection: c(0x7c6fbd),
        selection_text: c(0xffffff),
        hover: c(0x383a4a),
        header_bg: c(0x21222c),
        accent: c(0xbd93f9),
        accent_hover: c(0xcba9ff),
        nav_bg: c(0x1e1f29),
        success: c(0x50fa7b),
        warning: c(0xf1fa8c),
        error: c(0xff5555),
    },
    Theme {
        name: "onedark",
        display: "🌑 One Dark Pro",
        dark: true,
        bg: c(0x282c34),
        card: c(0x21252b),
        input: c(0x282c34),
        text: c(0xabb2bf),
        text_weak: c(0x5c6370),
        border: c(0x3e4451),
        selection: c(0x4976c9),
        selection_text: c(0xffffff),
        hover: c(0x2c313a),
        header_bg: c(0x21252b),
        accent: c(0x61afef),
        accent_hover: c(0x7fc0ff),
        nav_bg: c(0x1c1f24),
        success: c(0x98c379),
        warning: c(0xe5c07b),
        error: c(0xe06c75),
    },
    Theme {
        name: "nord",
        display: "❄ Nord",
        dark: true,
        bg: c(0x2e3440),
        card: c(0x3b4252),
        input: c(0x2e3440),
        text: c(0xeceff4),
        text_weak: c(0x7b88a1),
        border: c(0x4c566a),
        selection: c(0x5e81ac),
        selection_text: c(0xffffff),
        hover: c(0x3b4252),
        header_bg: c(0x3b4252),
        accent: c(0x88c0d0),
        accent_hover: c(0x9fd8e6),
        nav_bg: c(0x242933),
        success: c(0xa3be8c),
        warning: c(0xebcb8b),
        error: c(0xbf616a),
    },
    Theme {
        name: "monokai",
        display: "🎨 Monokai Pro",
        dark: true,
        bg: c(0x2d2a2e),
        card: c(0x221f22),
        input: c(0x2d2a2e),
        text: c(0xfcfcfa),
        text_weak: c(0x939293),
        border: c(0x403e41),
        selection: c(0x7150a0),
        selection_text: c(0xffffff),
        hover: c(0x37343a),
        header_bg: c(0x221f22),
        accent: c(0xffd866),
        accent_hover: c(0xffe694),
        nav_bg: c(0x19181a),
        success: c(0xa9dc76),
        warning: c(0xffd866),
        error: c(0xff6188),
    },
    Theme {
        name: "tokyo",
        display: "🌃 Tokyo Night",
        dark: true,
        bg: c(0x1a1b26),
        card: c(0x16161e),
        input: c(0x1a1b26),
        text: c(0xc0caf5),
        text_weak: c(0x565f89),
        border: c(0x292e42),
        selection: c(0x3d59a1),
        selection_text: c(0xffffff),
        hover: c(0x292e42),
        header_bg: c(0x16161e),
        accent: c(0x7aa2f7),
        accent_hover: c(0x9ab8ff),
        nav_bg: c(0x0e0e15),
        success: c(0x9ece6a),
        warning: c(0xe0af68),
        error: c(0xf7768e),
    },
    Theme {
        name: "gruvbox",
        display: "🌰 Gruvbox Dark",
        dark: true,
        bg: c(0x282828),
        card: c(0x1d2021),
        input: c(0x282828),
        text: c(0xebdbb2),
        text_weak: c(0x928374),
        border: c(0x3c3836),
        selection: c(0x8b7355),
        selection_text: c(0xffffff),
        hover: c(0x32302f),
        header_bg: c(0x1d2021),
        accent: c(0xd79921),
        accent_hover: c(0xe8b13b),
        nav_bg: c(0x1d2021),
        success: c(0xb8bb26),
        warning: c(0xfabd2f),
        error: c(0xfb4934),
    },
    Theme {
        name: "catppuccin",
        display: "🐱 Catppuccin Mocha",
        dark: true,
        bg: c(0x1e1e2e),
        card: c(0x181825),
        input: c(0x1e1e2e),
        text: c(0xcdd6f4),
        text_weak: c(0x6c7086),
        border: c(0x313244),
        selection: c(0x6f7bc4),
        selection_text: c(0xffffff),
        hover: c(0x313244),
        header_bg: c(0x181825),
        accent: c(0xcba6f7),
        accent_hover: c(0xddb9ff),
        nav_bg: c(0x11111b),
        success: c(0xa6e3a1),
        warning: c(0xf9e2af),
        error: c(0xf38ba8),
    },
    Theme {
        name: "rosepine",
        display: "🌹 Rosé Pine",
        dark: true,
        bg: c(0x191724),
        card: c(0x1f1d2e),
        input: c(0x191724),
        text: c(0xe0def4),
        text_weak: c(0x6e6a86),
        border: c(0x26233a),
        selection: c(0x6b5f8a),
        selection_text: c(0xffffff),
        hover: c(0x26233a),
        header_bg: c(0x1f1d2e),
        accent: c(0xc4a7e7),
        accent_hover: c(0xd8bff2),
        nav_bg: c(0x16141f),
        success: c(0x9ccfd8),
        warning: c(0xf6c177),
        error: c(0xeb6f92),
    },
    Theme {
        name: "solarized",
        display: "☀ Solarized Light",
        dark: false,
        bg: c(0xfdf6e3),
        card: c(0xeee8d5),
        input: c(0xffffff),
        text: c(0x073642),
        text_weak: c(0x93a1a1),
        border: c(0xd8d2c0),
        selection: c(0x7cbfb5),
        selection_text: c(0x073642),
        hover: c(0xeee8d5),
        header_bg: c(0xeee8d5),
        accent: c(0x268bd2),
        accent_hover: c(0x3498db),
        nav_bg: c(0x073642),
        success: c(0x859900),
        warning: c(0xb58900),
        error: c(0xdc322f),
    },
    Theme {
        name: "github",
        display: "🐙 GitHub Light",
        dark: false,
        bg: c(0xffffff),
        card: c(0xf6f8fa),
        input: c(0xffffff),
        text: c(0x24292f),
        text_weak: c(0x6e7781),
        border: c(0xd0d7de),
        selection: c(0x79c0ff),
        selection_text: c(0xffffff),
        hover: c(0xf3f4f6),
        header_bg: c(0xf6f8fa),
        accent: c(0x0969da),
        accent_hover: c(0x1a7fda),
        nav_bg: c(0x24292f),
        success: c(0x1a7f37),
        warning: c(0x9a6700),
        error: c(0xcf222e),
    },
];

/// 按内部名查主题（找不到返回默认 Dracula）。
pub fn by_name(name: &str) -> Theme {
    THEMES
        .iter()
        .find(|t| t.name == name || t.display == name)
        .copied()
        .unwrap_or(THEMES[0])
}

/// 生成 egui 样式并应用到 Context。
pub fn apply(ctx: &egui::Context, t: Theme) {
    let mut visuals = if t.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.panel_fill = t.bg;
    visuals.window_fill = t.bg;
    visuals.window_shadow = egui::epaint::Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(if t.dark { 120 } else { 40 }),
    };
    visuals.popup_shadow = visuals.window_shadow;
    visuals.extreme_bg_color = t.input;
    visuals.faint_bg_color = t.card;
    visuals.code_bg_color = t.card;
    visuals.window_stroke = Stroke::new(1.0, t.border);
    visuals.hyperlink_color = t.accent;
    visuals.warn_fg_color = t.warning;
    visuals.error_fg_color = t.error;
    visuals.selection = egui::style::Selection {
        bg_fill: t.selection,
        stroke: Stroke::new(1.0, t.accent),
    };
    visuals.widgets.noninteractive.bg_fill = t.card;
    visuals.widgets.noninteractive.weak_bg_fill = t.card;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, t.border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, t.text);
    visuals.widgets.inactive.bg_fill = t.card;
    visuals.widgets.inactive.weak_bg_fill = t.card;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, t.border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, t.text);
    visuals.widgets.hovered.bg_fill = t.hover;
    visuals.widgets.hovered.weak_bg_fill = t.hover;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, t.accent);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, t.text);
    visuals.widgets.active.bg_fill = t.selection;
    visuals.widgets.active.weak_bg_fill = t.selection;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, t.accent);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, t.selection_text);
    visuals.widgets.open.bg_fill = t.hover;
    visuals.widgets.open.weak_bg_fill = t.hover;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, t.accent);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, t.text);

    let egui_theme = if t.dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    let mut style = (*ctx.style_of(egui_theme)).clone();
    style.visuals = visuals;
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.menu_margin = egui::Margin::same(6);
    style.spacing.indent = 18.0;
    style.text_styles = [
        (egui::TextStyle::Small, egui::FontId::proportional(12.0)),
        (egui::TextStyle::Body, egui::FontId::proportional(14.0)),
        (egui::TextStyle::Button, egui::FontId::proportional(14.0)),
        (egui::TextStyle::Heading, egui::FontId::proportional(19.0)),
        (egui::TextStyle::Monospace, egui::FontId::monospace(13.0)),
    ]
    .into();
    // 两套主题都用同一份样式：应用自己控制深/浅色，不跟随系统切换
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);
    ctx.set_theme(egui_theme);
}

/// 主题里的圆角（统一 6px，窗口 10px）。
pub fn radius(n: u8) -> CornerRadius {
    CornerRadius::same(n)
}

/// UI 尺寸常量。
pub mod size {
    pub const TOPBAR_H: f32 = 52.0;
    pub const SIDEBAR_W: f32 = 148.0;
    pub const STATUSBAR_H: f32 = 26.0;
    pub const ROW_H: f32 = 30.0;
    pub const HEADER_H: f32 = 30.0;
}
