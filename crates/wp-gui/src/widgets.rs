//! 通用小部件与绘制辅助：卡片、徽标、主按钮、进度条、空态、键值行、提示条。

use crate::theme::{radius, Theme};
use egui::{Color32, Response, RichText, Stroke, Ui};

/// 卡片容器。
pub fn card<R>(ui: &mut Ui, theme: &Theme, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::NONE
        .fill(theme.card)
        .stroke(Stroke::new(1.0, theme.border))
        .corner_radius(radius(8))
        .inner_margin(egui::Margin::same(12))
        .show(ui, add)
        .inner
}

/// 小标题。
pub fn section_title(ui: &mut Ui, theme: &Theme, text: &str) {
    ui.label(RichText::new(text).size(15.0).strong().color(theme.text));
}

/// 说明文字。
pub fn hint(ui: &mut Ui, theme: &Theme, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(theme.text_weak));
}

/// 主按钮（强调色填充）。
pub fn primary_button(ui: &mut Ui, theme: &Theme, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(if theme.dark {
            Color32::from_rgb(20, 20, 24)
        } else {
            Color32::WHITE
        }))
        .fill(theme.accent)
        .corner_radius(radius(6)),
    )
}

/// 次要按钮（卡片底色 + 边框）。
pub fn secondary_button(ui: &mut Ui, theme: &Theme, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(theme.text))
            .fill(theme.card)
            .stroke(Stroke::new(1.0, theme.border))
            .corner_radius(radius(6)),
    )
}

/// 危险操作按钮（错误色文字）。
pub fn danger_button(ui: &mut Ui, theme: &Theme, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(theme.error))
            .fill(theme.card)
            .stroke(Stroke::new(1.0, theme.border))
            .corner_radius(radius(6)),
    )
}

/// 工具栏小按钮。
pub fn tool_button(ui: &mut Ui, theme: &Theme, text: &str, enabled: bool) -> Response {
    let mut btn = egui::Button::new(RichText::new(text).size(13.0).color(if enabled {
        theme.text
    } else {
        theme.text_weak
    }))
    .fill(theme.card)
    .corner_radius(radius(6));
    if enabled {
        btn = btn.stroke(Stroke::new(1.0, theme.border));
    }
    if enabled {
        ui.add_enabled(true, btn)
    } else {
        ui.add_enabled(false, btn)
            .on_hover_text("当前网盘不支持该操作")
    }
}

/// 空态提示。
pub fn empty_state(ui: &mut Ui, theme: &Theme, title: &str, hint_text: &str) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(title).size(16.0).color(theme.text_weak));
        ui.add_space(6.0);
        ui.label(RichText::new(hint_text).size(12.0).color(theme.text_weak));
    });
    ui.add_space(40.0);
}

/// 加载中提示。
pub fn loading(ui: &mut Ui, theme: &Theme, text: &str) {
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(RichText::new(text).size(13.0).color(theme.text_weak));
    });
}

/// 键值行。
pub fn kv_row(ui: &mut Ui, theme: &Theme, key: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(key).size(13.0).color(theme.text_weak));
        ui.add_space(6.0);
        ui.label(RichText::new(value).size(13.0).color(theme.text));
    });
}

/// 错误横幅。
pub fn error_banner(ui: &mut Ui, theme: &Theme, text: &str) {
    egui::Frame::NONE
        .fill(Color32::from_rgba_unmultiplied(
            theme.error.r(),
            theme.error.g(),
            theme.error.b(),
            28,
        ))
        .stroke(Stroke::new(1.0, theme.error))
        .corner_radius(radius(6))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.label(
                RichText::new(format!("⚠ {text}"))
                    .size(13.0)
                    .color(theme.error),
            );
        });
}

/// 成功横幅。
pub fn success_banner(ui: &mut Ui, theme: &Theme, text: &str) {
    egui::Frame::NONE
        .fill(Color32::from_rgba_unmultiplied(
            theme.success.r(),
            theme.success.g(),
            theme.success.b(),
            24,
        ))
        .corner_radius(radius(6))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.label(
                RichText::new(format!("✓ {text}"))
                    .size(13.0)
                    .color(theme.success),
            );
        });
}

/// 警告横幅（非阻塞提示）。
pub fn warn_banner(ui: &mut Ui, theme: &Theme, text: &str) {
    egui::Frame::NONE
        .fill(Color32::from_rgba_unmultiplied(
            theme.warning.r(),
            theme.warning.g(),
            theme.warning.b(),
            24,
        ))
        .stroke(Stroke::new(1.0, theme.warning))
        .corner_radius(radius(6))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.label(
                RichText::new(format!("⚠ {text}"))
                    .size(13.0)
                    .color(theme.warning),
            );
        });
}

/// 把文本截断到 n 个字符（表格列用）。
pub fn ellipsis(text: &str, n: usize) -> String {
    let count = text.chars().count();
    if count <= n {
        text.to_string()
    } else {
        let s: String = text.chars().take(n.saturating_sub(1)).collect();
        format!("{s}…")
    }
}

/// 文件类型图标（用 Unicode 符号，egui 默认字体可显示常见部分）。
pub fn file_icon(is_dir: bool, name: &str) -> &'static str {
    if is_dir {
        return "📁";
    }
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "flv" | "ts" => "🎬",
        "mp3" | "flac" | "wav" | "aac" | "m4a" | "ogg" => "🎵",
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "svg" => "🖼",
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" => "📦",
        "pdf" => "📕",
        "doc" | "docx" => "📘",
        "xls" | "xlsx" | "csv" => "📗",
        "ppt" | "pptx" => "📙",
        "txt" | "md" | "log" | "json" | "toml" | "yaml" | "yml" => "📄",
        "exe" | "msi" | "deb" | "rpm" | "apk" => "⚙",
        _ => "📄",
    }
}
