//! 日志页：级别过滤 + 关键字搜索 + 自动滚动 + 导出。

use crate::app::{App, ToastKind};
use crate::theme::{self, Theme};
use crate::widgets;
use egui::{Color32, RichText, Ui};
use wp_core::logging::LogLevel;

pub fn show(app: &mut App, ui: &mut Ui) {
    let t = app.theme;
    let mut acts: Vec<Act> = Vec::new();

    ui.horizontal(|ui| {
        ui.label(RichText::new("级别").size(12.0).color(t.text_weak));
        let mut lvl = app.log_level_filter;
        egui::ComboBox::from_id_salt("log_level")
            .selected_text(lvl.label())
            .width(80.0)
            .show_ui(ui, |ui| {
                for l in [
                    LogLevel::Debug,
                    LogLevel::Info,
                    LogLevel::Warn,
                    LogLevel::Error,
                ] {
                    if ui.selectable_label(lvl == l, l.label()).clicked() {
                        lvl = l;
                    }
                }
            });
        if lvl != app.log_level_filter {
            app.log_level_filter = lvl;
        }
        let mut search = app.log_search.clone();
        ui.add(
            egui::TextEdit::singleline(&mut search)
                .desired_width(220.0)
                .hint_text("搜索日志…"),
        );
        if search != app.log_search {
            app.log_search = search;
        }
        let mut auto = app.log_auto_scroll;
        if ui.checkbox(&mut auto, "自动滚动").changed() {
            app.log_auto_scroll = auto;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("🗑 清空").clicked() {
                acts.push(Act::Clear);
            }
            if ui.button("💾 导出").clicked() {
                acts.push(Act::Export);
            }
            if ui.button("📂 日志目录").clicked() {
                acts.push(Act::OpenDir);
            }
            if ui.button("📋 复制全部").clicked() {
                acts.push(Act::CopyAll);
            }
        });
    });
    ui.add_space(4.0);
    if let Some(file) = wp_core::logging::current_file() {
        ui.label(
            RichText::new(format!("日志文件：{}", file.display()))
                .size(11.0)
                .color(t.text_weak),
        );
    }
    ui.add_space(4.0);

    // 过滤
    let level = app.log_level_filter;
    let kw = app.log_search.trim().to_lowercase();
    let lines: Vec<&wp_core::logging::LogLine> = app
        .log_lines
        .iter()
        .filter(|l| l.level >= level)
        .filter(|l| {
            kw.is_empty()
                || l.message.to_lowercase().contains(&kw)
                || l.target.to_lowercase().contains(&kw)
        })
        .collect();

    let mut scroll = egui::ScrollArea::vertical()
        .id_salt("log_scroll")
        .auto_shrink([false, false]);
    if app.log_auto_scroll {
        scroll = scroll.stick_to_bottom(true);
    }
    scroll.show(ui, |ui| {
        if lines.is_empty() {
            widgets::empty_state(ui, &t, "暂无日志", "操作网盘或传输时会产生日志");
            return;
        }
        for line in &lines {
            log_line(ui, &t, line);
        }
    });

    for a in acts {
        apply(app, a);
    }
}

fn log_line(ui: &mut Ui, t: &Theme, line: &wp_core::logging::LogLine) {
    let color = match line.level {
        LogLevel::Debug => t.text_weak,
        LogLevel::Info => t.text,
        LogLevel::Warn => t.warning,
        LogLevel::Error => t.error,
    };
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(
            RichText::new(format!("[{}]", wp_core::util::format_clock(line.time)))
                .size(11.0)
                .monospace()
                .color(t.text_weak),
        );
        ui.label(
            RichText::new(format!("[{}]", line.level.label()))
                .size(11.0)
                .monospace()
                .color(color),
        );
        ui.label(
            RichText::new(format!("[{}]", line.target))
                .size(11.0)
                .monospace()
                .color(t.accent),
        );
        ui.label(RichText::new(&line.message).size(12.0).color(color));
    });
}

enum Act {
    Clear,
    Export,
    OpenDir,
    CopyAll,
}

fn apply(app: &mut App, act: Act) {
    match act {
        Act::Clear => {
            app.log_lines.clear();
            wp_core::logging::clear();
            app.toast(ToastKind::Info, "日志已清空（文件保留）");
        }
        Act::Export => {
            let path = app.paths.log_dir().join(format!(
                "导出-{}.txt",
                wp_core::util::format_stamp(wp_core::util::now_ts())
            ));
            match wp_core::logging::export(&path) {
                Ok(()) => app.toast(ToastKind::Success, format!("已导出：{}", path.display())),
                Err(e) => app.toast(ToastKind::Error, format!("导出失败：{e}")),
            }
        }
        Act::OpenDir => {
            let dir = app.paths.log_dir();
            if let Err(e) = wp_core::paths::open_path(&dir) {
                app.toast(ToastKind::Error, format!("打开日志目录失败：{e}"));
            }
        }
        Act::CopyAll => {
            let text: String = app
                .log_lines
                .iter()
                .map(|l| {
                    format!(
                        "[{}] [{}] [{}] {}",
                        wp_core::util::format_clock(l.time),
                        l.level.label(),
                        l.target,
                        l.message
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(ctx) = &app.ctx {
                ctx.copy_text(text);
            }
            app.toast(
                ToastKind::Success,
                format!("已复制 {} 行日志到剪贴板", app.log_lines.len()),
            );
        }
    }
    let _ = theme::radius(0);
    let _ = Color32::TRANSPARENT;
}
