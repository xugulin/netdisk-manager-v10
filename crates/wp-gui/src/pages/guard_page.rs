//! 敏感词页：按网盘实例管理词库、查看命中改名记录与守卫统计。

use crate::app::{App, ToastKind};
use crate::theme::{self, Theme};
use crate::widgets;
use egui::{RichText, Ui};
use wp_core::config::GuardAction;

pub fn show(app: &mut App, ui: &mut Ui) {
    let t = app.theme;
    let mut acts: Vec<Act> = Vec::new();

    // 当前实例
    let current = app
        .active_slot()
        .map(|s| s.id().to_string())
        .unwrap_or_default();
    let instances: Vec<(String, String)> = app
        .drives
        .iter()
        .map(|s| (s.id().to_string(), s.display().to_string()))
        .collect();
    let (global_words, instance_words, stats, records, enabled, action) = {
        let g = match app.guard.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        (
            g.db.global.clone(),
            g.db.instances.get(&current).cloned().unwrap_or_default(),
            g.stats,
            g.db.rename_records
                .iter()
                .rev()
                .take(20)
                .cloned()
                .collect::<Vec<_>>(),
            g.enabled,
            g.action,
        )
    };

    ui.horizontal(|ui| {
        widgets::section_title(ui, &t, "敏感词守卫");
        ui.add_space(16.0);
        ui.label(RichText::new("当前网盘").size(12.0).color(t.text_weak));
        let mut sel = current.clone();
        egui::ComboBox::from_id_salt("guard_drive")
            .selected_text(
                instances
                    .iter()
                    .find(|(id, _)| id == &sel)
                    .map(|(_, n)| n.clone())
                    .unwrap_or_else(|| "（无）".into()),
            )
            .width(180.0)
            .show_ui(ui, |ui| {
                for (id, name) in &instances {
                    if ui.selectable_label(&sel == id, name).clicked() {
                        sel = id.clone();
                    }
                }
            });
        if sel != current {
            if let Some(idx) = app.slot_idx(&sel) {
                app.active = idx;
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(format!(
                    "状态：{} · 策略：{}",
                    if enabled { "已启用" } else { "已关闭" },
                    match action {
                        GuardAction::Warn => "仅警告",
                        GuardAction::Block => "拦截",
                        GuardAction::Rename => "自动改名",
                    }
                ))
                .size(12.0)
                .color(if enabled { t.success } else { t.text_weak }),
            );
        });
    });
    ui.add_space(4.0);
    widgets::hint(
        ui,
        &t,
        "上传前检查文件名：命中词库后按「设置 → 敏感词守卫」里的策略处理（警告/拦截/自动改名）。词库区分全局与按网盘实例。",
    );
    ui.add_space(8.0);

    // 统计
    ui.horizontal(|ui| {
        let w = ((ui.available_width() - 32.0) / 5.0).max(110.0);
        stat(ui, &t, w, "检查次数", stats.checked, t.accent);
        stat(ui, &t, w, "通过", stats.passed, t.success);
        stat(ui, &t, w, "警告", stats.warned, t.warning);
        stat(ui, &t, w, "拦截", stats.blocked, t.error);
        stat(ui, &t, w, "自动改名", stats.renamed, t.accent);
    });
    ui.add_space(10.0);

    ui.columns(2, |cols| {
        // 左：敏感词
        widgets::card(&mut cols[0], &t, |ui| {
            widgets::section_title(ui, &t, "全局敏感词");
            ui.add_space(4.0);
            let mut footer: Option<Act> = None;
            egui::ScrollArea::vertical()
                .id_salt("global_words")
                .max_height(150.0)
                .show(ui, |ui| {
                    if global_words.is_empty() {
                        widgets::hint(ui, &t, "（空）");
                    }
                    for w in &global_words {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(w).size(13.0).color(t.text));
                            if ui.small_button("✕").clicked() {
                                footer = Some(Act::RemoveGlobal(w.clone()));
                            }
                        });
                    }
                });
            let mut input = app.guard_new_word.clone();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut input)
                    .desired_width(200.0)
                    .hint_text("新增全局敏感词"),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                footer = Some(Act::AddWord {
                    instance: String::new(),
                    word: input.trim().to_string(),
                });
            }
            app.guard_new_word = input;
            if ui.button("＋ 添加").clicked() {
                footer = Some(Act::AddWord {
                    instance: String::new(),
                    word: app.guard_new_word.trim().to_string(),
                });
            }
            if let Some(a) = footer {
                acts.push(a);
            }

            ui.add_space(10.0);
            widgets::section_title(ui, &t, &format!("实例专属词：{current}"));
            ui.add_space(4.0);
            let mut footer2: Option<Act> = None;
            egui::ScrollArea::vertical()
                .id_salt("instance_words")
                .max_height(180.0)
                .show(ui, |ui| {
                    if instance_words.is_empty() {
                        widgets::hint(ui, &t, "（空）—— 全局词对当前网盘同样生效");
                    }
                    for w in &instance_words {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(w).size(13.0).color(t.text));
                            if ui.small_button("✕").clicked() {
                                footer2 = Some(Act::RemoveInstance(current.clone(), w.clone()));
                            }
                        });
                    }
                });
            let mut input2 = app.guard_new_instance_word.clone();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut input2)
                    .desired_width(200.0)
                    .hint_text("新增当前实例的敏感词"),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                footer2 = Some(Act::AddWord {
                    instance: current.clone(),
                    word: input2.trim().to_string(),
                });
            }
            app.guard_new_instance_word = input2;
            if ui.button("＋ 添加").clicked() {
                footer2 = Some(Act::AddWord {
                    instance: current.clone(),
                    word: app.guard_new_instance_word.trim().to_string(),
                });
            }
            if let Some(a) = footer2 {
                acts.push(a);
            }
            ui.add_space(4.0);
            widgets::hint(
                ui,
                &t,
                &format!(
                    "词库文件：{}（也可手工编辑）",
                    app.paths.sensitive_file().display()
                ),
            );
        });

        // 右：改名记录
        widgets::card(&mut cols[1], &t, |ui| {
            widgets::section_title(ui, &t, "最近命中改名记录");
            ui.add_space(4.0);
            if records.is_empty() {
                widgets::empty_state(ui, &t, "暂无记录", "命中敏感词并自动改名后会记录在这里");
            } else {
                egui::ScrollArea::vertical()
                    .id_salt("rename_records")
                    .max_height(380.0)
                    .show(ui, |ui| {
                        for r in &records {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new(wp_core::util::format_time(r.time))
                                        .size(11.0)
                                        .color(t.text_weak),
                                );
                                ui.label(RichText::new(&r.instance).size(11.0).color(t.accent));
                                ui.label(RichText::new(&r.hit_word).size(11.0).color(t.error));
                            });
                            ui.label(
                                RichText::new(format!("{} → {}", r.original, r.renamed))
                                    .size(12.0)
                                    .color(t.text),
                            );
                            ui.separator();
                        }
                    });
            }
        });
    });

    for a in acts {
        apply(app, a);
    }
    let _ = theme::radius(0);
}

fn stat(ui: &mut Ui, t: &Theme, w: f32, label: &str, value: u64, color: egui::Color32) {
    egui::Frame::NONE
        .fill(t.card)
        .stroke(egui::Stroke::new(1.0, t.border))
        .corner_radius(theme::radius(8))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(w - 22.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(label).size(11.0).color(t.text_weak));
                ui.label(
                    RichText::new(value.to_string())
                        .size(20.0)
                        .strong()
                        .color(color),
                );
            });
        });
}

enum Act {
    AddWord { instance: String, word: String },
    RemoveGlobal(String),
    RemoveInstance(String, String),
}

fn apply(app: &mut App, act: Act) {
    let mut g = match app.guard.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    match act {
        Act::AddWord { instance, word } => {
            if word.trim().is_empty() {
                drop(g);
                app.toast(ToastKind::Warn, "敏感词不能为空");
                return;
            }
            if instance.is_empty() {
                if !g.db.global.iter().any(|x| x == &word) {
                    g.db.global.push(word.clone());
                }
            } else {
                g.db.add_word(&instance, word.clone());
            }
            let _ = g.save();
            drop(g);
            if instance.is_empty() {
                app.guard_new_word.clear();
            } else {
                app.guard_new_instance_word.clear();
            }
            app.toast(ToastKind::Success, format!("已添加敏感词：{word}"));
        }
        Act::RemoveGlobal(word) => {
            g.db.global.retain(|x| x != &word);
            let _ = g.save();
            drop(g);
            app.toast(ToastKind::Info, format!("已删除敏感词：{word}"));
        }
        Act::RemoveInstance(instance, word) => {
            g.db.remove_word(&instance, &word);
            let _ = g.save();
            drop(g);
            app.toast(ToastKind::Info, format!("已删除敏感词：{word}"));
        }
    }
}
