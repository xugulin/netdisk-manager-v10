//! 传输页：统计卡片 + 批量操作 + 任务表格（进度/速率/状态/逐任务控制）。

use crate::app::{App, ConfirmAction, Dialog, ToastKind};
use crate::theme::{self, Theme};
use crate::widgets;
use egui::{Color32, RichText, Sense, Stroke, Ui, Vec2};
use wp_core::model::{TaskKind, TaskState, TransferTask};

pub fn show(app: &mut App, ui: &mut Ui) {
    let t = app.theme;
    let tasks = app.transfer_snapshot.clone();
    let stats = app.transfer_stats;
    let mut actions: Vec<Act> = Vec::new();

    // ── 统计卡片
    ui.horizontal(|ui| {
        let card_w = ((ui.available_width() - 4.0 * 8.0) / 5.0).max(120.0);
        stat_card(
            ui,
            &t,
            card_w,
            "进行中",
            &stats.active.to_string(),
            t.accent,
        );
        stat_card(ui, &t, card_w, "排队", &stats.queued.to_string(), t.warning);
        stat_card(ui, &t, card_w, "已完成", &stats.done.to_string(), t.success);
        stat_card(ui, &t, card_w, "失败", &stats.failed.to_string(), t.error);
        stat_card(
            ui,
            &t,
            card_w,
            "总速度",
            &wp_core::util::format_speed(stats.speed),
            t.text,
        );
    });
    ui.add_space(8.0);

    // ── 工具栏
    ui.horizontal(|ui| {
        if widgets::tool_button(ui, &t, "全部暂停", stats.active > 0).clicked() {
            actions.push(Act::PauseAll);
        }
        if widgets::tool_button(
            ui,
            &t,
            "▶ 全部继续",
            stats.queued
                + tasks
                    .iter()
                    .filter(|t| t.state == TaskState::Paused)
                    .count()
                > 0,
        )
        .clicked()
        {
            actions.push(Act::ResumeAll);
        }
        if widgets::tool_button(ui, &t, "全部取消", stats.active + stats.queued > 0).clicked() {
            actions.push(Act::CancelAll);
        }
        if widgets::tool_button(ui, &t, "⟳ 重试失败", stats.failed > 0).clicked() {
            actions.push(Act::RetryFailed);
        }
        if widgets::tool_button(ui, &t, "清除已完成", stats.done > 0).clicked() {
            actions.push(Act::ClearFinished);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::tool_button(ui, &t, "📂 打开下载目录", true).clicked() {
                actions.push(Act::OpenDownloadDir);
            }
            ui.label(
                RichText::new(format!("下载目录：{}", app.download_dir().display()))
                    .size(11.0)
                    .color(t.text_weak),
            );
        });
    });
    ui.add_space(6.0);

    if tasks.is_empty() {
        widgets::empty_state(
            ui,
            &t,
            "还没有传输任务",
            "去「网盘」页选文件下载，或点「⬆ 上传」加入任务",
        );
        for a in actions {
            apply(app, a);
        }
        return;
    }

    // ── 表头
    let total_w = ui.available_width();
    let header_h = theme::size::HEADER_H;
    let (hrect, _) = ui.allocate_exact_size(Vec2::new(total_w, header_h), Sense::hover());
    ui.painter().rect_filled(hrect, 0, t.header_bg);
    let font = egui::FontId::proportional(12.5);
    let cy = hrect.center().y;
    let x0 = hrect.left() + 10.0;
    ui.painter().text(
        egui::pos2(x0, cy),
        egui::Align2::LEFT_CENTER,
        "任务",
        font.clone(),
        t.text_weak,
    );
    ui.painter().text(
        egui::pos2(x0 + 400.0, cy),
        egui::Align2::LEFT_CENTER,
        "进度",
        font.clone(),
        t.text_weak,
    );
    ui.painter().text(
        egui::pos2(x0 + 660.0, cy),
        egui::Align2::LEFT_CENTER,
        "速度",
        font.clone(),
        t.text_weak,
    );
    ui.painter().text(
        egui::pos2(x0 + 760.0, cy),
        egui::Align2::LEFT_CENTER,
        "状态",
        font.clone(),
        t.text_weak,
    );

    // ── 任务行
    let now = wp_core::util::now_ts();
    let row_h = 40.0;
    egui::ScrollArea::vertical()
        .id_salt("task_list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for task in &tasks {
                row(ui, &t, task, row_h, total_w, now, &mut actions);
            }
        });

    for a in actions {
        apply(app, a);
    }
}

fn stat_card(ui: &mut Ui, t: &Theme, w: f32, label: &str, value: &str, color: Color32) {
    egui::Frame::NONE
        .fill(t.card)
        .stroke(Stroke::new(1.0, t.border))
        .corner_radius(theme::radius(8))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(w - 22.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(label).size(11.0).color(t.text_weak));
                ui.label(RichText::new(value).size(20.0).strong().color(color));
            });
        });
}

fn row(
    ui: &mut Ui,
    t: &Theme,
    task: &TransferTask,
    h: f32,
    total_w: f32,
    now: i64,
    actions: &mut Vec<Act>,
) {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(total_w, h), Sense::hover());
    if response.hovered() {
        ui.painter().rect_filled(rect, 2, t.hover);
    }
    let cy = rect.center().y;
    let x0 = rect.left() + 10.0;

    // 类型图标 + 文件名
    let (icon, kind_label) = match task.kind {
        TaskKind::Upload => ("⬆", "上传"),
        TaskKind::Download => ("⬇", "下载"),
        TaskKind::CrossCopy => ("⇄", "跨盘"),
    };
    ui.painter().text(
        egui::pos2(x0, cy),
        egui::Align2::LEFT_CENTER,
        icon,
        egui::FontId::proportional(16.0),
        t.accent,
    );
    ui.painter().text(
        egui::pos2(x0 + 22.0, cy - 7.0),
        egui::Align2::LEFT_CENTER,
        widgets::ellipsis(&task.file_name, 46),
        egui::FontId::proportional(13.0),
        t.text,
    );
    // 多账号场景下必须显示「源实例 → 目标实例」，否则看不出来是哪两个账号在传。
    let route = match task.kind {
        TaskKind::Upload => format!("本机 → {}", task.dst_drive),
        TaskKind::Download => format!("{} → 本机", task.src_drive),
        TaskKind::CrossCopy => format!("{} → {}", task.src_drive, task.dst_drive),
    };
    let mut sub = format!(
        "{} · {} · {}",
        kind_label,
        route,
        if task.src_path.is_empty() {
            &task.dst_dir
        } else {
            &task.src_path
        }
    );
    // 备注（如「空目录：0 个文件…」）直接显示在副标题里，避免用户以为传输丢了内容。
    if !task.note.is_empty() {
        sub = format!("{sub} · {}", task.note);
    }
    ui.painter().text(
        egui::pos2(x0 + 22.0, cy + 9.0),
        egui::Align2::LEFT_CENTER,
        widgets::ellipsis(&sub, 76),
        egui::FontId::proportional(10.5),
        t.text_weak,
    );

    // 进度条
    let bar_rect =
        egui::Rect::from_min_size(egui::pos2(x0 + 400.0, cy - 6.0), Vec2::new(220.0, 12.0));
    ui.painter().rect_filled(bar_rect, 4, t.border);
    let p = task.progress();
    if p > 0.0 {
        let fill = if task.state == TaskState::Done {
            t.success
        } else {
            t.accent
        };
        let mut fr = bar_rect;
        fr.set_width(bar_rect.width() * p);
        ui.painter().rect_filled(fr, 4, fill);
    }
    ui.painter().text(
        egui::pos2(x0 + 400.0, cy + 11.0),
        egui::Align2::LEFT_CENTER,
        task.progress_text(),
        egui::FontId::proportional(10.5),
        t.text_weak,
    );
    if !task.note.is_empty() {
        let note_rect =
            egui::Rect::from_min_size(egui::pos2(x0 + 400.0, cy - 6.0), Vec2::new(220.0, 12.0));
        ui.allocate_rect(note_rect, Sense::hover())
            .on_hover_text(&task.note);
    }

    // 速度
    ui.painter().text(
        egui::pos2(x0 + 660.0, cy),
        egui::Align2::LEFT_CENTER,
        task.speed_text(),
        egui::FontId::proportional(12.0),
        t.text_weak,
    );
    // 状态
    let (state_text, state_color) = match task.state {
        TaskState::Queued => ("排队中", t.warning),
        TaskState::Running => ("进行中", t.accent),
        TaskState::Paused => ("已暂停", t.warning),
        TaskState::Done => ("已完成", t.success),
        TaskState::Failed => ("失败", t.error),
        TaskState::Canceled => ("已取消", t.text_weak),
    };
    ui.painter().text(
        egui::pos2(x0 + 760.0, cy),
        egui::Align2::LEFT_CENTER,
        state_text,
        egui::FontId::proportional(12.0),
        state_color,
    );
    if task.state == TaskState::Failed && !task.error.is_empty() {
        let err = widgets::ellipsis(&task.error, 36);
        let resp = ui
            .allocate_rect(
                egui::Rect::from_min_size(egui::pos2(x0 + 760.0, cy - 8.0), Vec2::new(180.0, 16.0)),
                Sense::hover(),
            )
            .on_hover_text(&task.error);
        let _ = resp;
        let _ = err;
    }

    // 行尾操作按钮（右对齐）
    let id = task.id.clone();
    ui.scope_builder(
        egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(
            egui::pos2(rect.right() - 210.0, rect.top() + 6.0),
            Vec2::new(210.0, h - 12.0),
        )),
        |ui| {
            ui.horizontal(|ui| match task.state {
                TaskState::Running => {
                    if ui.small_button("暂停").clicked() {
                        actions.push(Act::Pause(id.clone()));
                    }
                    if ui.small_button("取消").clicked() {
                        actions.push(Act::Cancel(id.clone()));
                    }
                }
                TaskState::Queued => {
                    if ui.small_button("暂停").clicked() {
                        actions.push(Act::Pause(id.clone()));
                    }
                    if ui.small_button("取消").clicked() {
                        actions.push(Act::Cancel(id.clone()));
                    }
                }
                TaskState::Paused => {
                    if ui.small_button("继续").clicked() {
                        actions.push(Act::Resume(id.clone()));
                    }
                    if ui.small_button("取消").clicked() {
                        actions.push(Act::Cancel(id.clone()));
                    }
                }
                TaskState::Failed | TaskState::Canceled => {
                    if ui.small_button("重试").clicked() {
                        actions.push(Act::Retry(id.clone()));
                    }
                    if ui.small_button("移除").clicked() {
                        actions.push(Act::Remove(id.clone()));
                    }
                }
                TaskState::Done => {
                    if ui.small_button("移除").clicked() {
                        actions.push(Act::Remove(id.clone()));
                    }
                    if !task.local_path.is_empty() && ui.small_button("打开").clicked() {
                        actions.push(Act::OpenLocal(task.local_path.clone()));
                    }
                }
            });
        },
    );
    let _ = now;
}

enum Act {
    Pause(String),
    Resume(String),
    Cancel(String),
    Retry(String),
    Remove(String),
    PauseAll,
    ResumeAll,
    CancelAll,
    RetryFailed,
    ClearFinished,
    OpenDownloadDir,
    OpenLocal(String),
}

fn apply(app: &mut App, act: Act) {
    match act {
        Act::Pause(id) => app.transfer.pause(&id),
        Act::Resume(id) => app.transfer.resume(&id),
        Act::Cancel(id) => app.transfer.cancel(&id),
        Act::Retry(id) => app.transfer.retry(&id),
        Act::Remove(id) => app.transfer.remove(&id),
        Act::PauseAll => app.transfer.pause_all(),
        Act::ResumeAll => app.transfer.resume_all(),
        Act::CancelAll => {
            let ids: Vec<String> = app
                .transfer_snapshot
                .iter()
                .filter(|t| t.state.is_active() || t.state == TaskState::Paused)
                .map(|t| t.id.clone())
                .collect();
            if !ids.is_empty() {
                app.dialog = Some(Dialog::Confirm(crate::app::ConfirmDialog {
                    title: "取消全部任务".into(),
                    message: format!(
                        "确认取消 {} 个进行中/排队任务？\n已传输的部分会保留断点（下载）。",
                        ids.len()
                    ),
                    danger: true,
                    action: ConfirmAction::CancelTasks { ids },
                }));
            }
        }
        Act::RetryFailed => {
            for t in app.transfer_snapshot.iter() {
                if t.state == TaskState::Failed {
                    app.transfer.retry(&t.id);
                }
            }
        }
        Act::ClearFinished => app.transfer.clear_finished(),
        Act::OpenDownloadDir => {
            let dir = app.download_dir();
            if let Err(e) = wp_core::paths::open_path(&dir) {
                app.toast(ToastKind::Error, format!("打开下载目录失败：{e}"));
            }
        }
        Act::OpenLocal(path) => {
            let p = std::path::PathBuf::from(path);
            let target = if p.is_file() {
                p.parent().map(|x| x.to_path_buf()).unwrap_or(p)
            } else {
                p
            };
            if let Err(e) = wp_core::paths::open_path(&target) {
                app.toast(ToastKind::Error, format!("打开失败：{e}"));
            }
        }
    }
    app.tick_transfers(true);
    app.transfer.save();
}
