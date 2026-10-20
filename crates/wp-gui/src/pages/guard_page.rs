//! 敏感词页：词库按「网盘类型」共享（同一网盘的所有账号实例通用），本机不启用敏感词。
//!
//! 旧版本按「实例 id」存词（baidu_2 与 baidu 各一套），用户反馈同网盘不同实例应通用；
//! 启动时 `GuardManager::migrate` 已把旧词库归并到类型，并清除本机/演示词库。

use crate::app::{App, ToastKind};
use crate::theme::{self, Theme};
use crate::widgets;
use egui::{RichText, Ui};
use wp_core::config::GuardAction;
use wp_core::model::DriveKind;

/// 可配置敏感词的网盘类型（本机/演示不启用）。
fn kind_options() -> Vec<(String, String, DriveKind)> {
    vec![
        ("baidu".into(), "百度网盘".into(), DriveKind::Baidu),
        ("quark".into(), "夸克网盘".into(), DriveKind::Quark),
        ("guangya".into(), "光鸭云盘".into(), DriveKind::Guangya),
    ]
}

pub fn show(app: &mut App, ui: &mut Ui) {
    let t = app.theme;
    let mut acts: Vec<Act> = Vec::new();
    let kinds = kind_options();
    if !kinds.iter().any(|(k, _, _)| k == &app.guard_sel_kind) {
        app.guard_sel_kind = kinds[0].0.clone();
    }
    let current = app.guard_sel_kind.clone();
    let current_name = kinds
        .iter()
        .find(|(k, _, _)| k == &current)
        .map(|(_, n, _)| n.clone())
        .unwrap_or_else(|| current.clone());

    let (global_words, kind_words, stats, records, enabled, action) = {
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

    // 标题 + 网盘类型选择
    ui.horizontal(|ui| {
        widgets::section_title(ui, &t, "敏感词守卫");
        ui.add_space(16.0);
        ui.label(RichText::new("当前网盘").size(12.0).color(t.text_weak));
        let mut sel = current.clone();
        egui::ComboBox::from_id_salt("guard_kind")
            .selected_text(
                kinds
                    .iter()
                    .find(|(k, _, _)| k == &sel)
                    .map(|(_, n, _)| n.clone())
                    .unwrap_or_else(|| "（无）".into()),
            )
            .width(160.0)
            .show_ui(ui, |ui| {
                for (k, name, _) in &kinds {
                    if ui.selectable_label(&sel == k, name).clicked() {
                        sel = k.clone();
                    }
                }
            });
        if sel != current {
            app.guard_sel_kind = sel;
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
        "上传前检查文件名：命中词库后按设置里的策略处理（警告/拦截/自动改名）。词库按网盘类型共享——同一网盘的所有账号实例（如「百度网盘」与「百度网盘（2）」）共用；本机上传不检查敏感词。",
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
        // 左：词库
        widgets::card(&mut cols[0], &t, |ui| {
            widgets::section_title(ui, &t, "全局敏感词");
            widgets::hint(ui, &t, "所有网盘生效");
            ui.add_space(4.0);
            let mut footer: Option<Act> = None;
            egui::ScrollArea::vertical()
                .id_salt("global_words")
                .max_height(120.0)
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
            ui.horizontal(|ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut input)
                        .desired_width(200.0)
                        .hint_text("新增全局敏感词"),
                );
                let submit = (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    || ui.button("＋ 添加").clicked();
                if submit {
                    footer = Some(Act::AddGlobal(input.trim().to_string()));
                }
            });
            app.guard_new_word = input;
            if let Some(a) = footer {
                acts.push(a);
            }
        });

        widgets::card(&mut cols[1], &t, |ui| {
            widgets::section_title(ui, &t, &format!("{current_name} · 专属敏感词"));
            widgets::hint(
                ui,
                &t,
                &format!("同一网盘的所有账号实例共用（当前：{current_name}）"),
            );
            ui.add_space(4.0);
            let mut footer: Option<Act> = None;
            egui::ScrollArea::vertical()
                .id_salt("kind_words")
                .max_height(200.0)
                .show(ui, |ui| {
                    if kind_words.is_empty() {
                        widgets::hint(ui, &t, "（空）");
                    }
                    for w in &kind_words {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(w).size(13.0).color(t.text));
                            if ui.small_button("✕").clicked() {
                                footer = Some(Act::RemoveKind(current.clone(), w.clone()));
                            }
                        });
                    }
                });
            let mut input2 = app.guard_new_instance_word.clone();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut input2)
                    .desired_width(200.0)
                    .hint_text("新增该网盘的敏感词"),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                footer = Some(Act::AddKind(current.clone(), input2.trim().to_string()));
            }
            app.guard_new_instance_word = input2;
            if ui.button("＋ 添加").clicked() {
                footer = Some(Act::AddKind(
                    current.clone(),
                    app.guard_new_instance_word.trim().to_string(),
                ));
            }
            if let Some(a) = footer {
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
    });

    // 改名记录
    ui.add_space(10.0);
    widgets::card(ui, &t, |ui| {
        widgets::section_title(ui, &t, "最近命中改名记录");
        ui.add_space(4.0);
        if records.is_empty() {
            widgets::empty_state(ui, &t, "暂无记录", "命中敏感词并自动改名后会记录在这里");
        } else {
            egui::ScrollArea::vertical()
                .id_salt("rename_records")
                .max_height(280.0)
                .show(ui, |ui| {
                    for r in &records {
                        let kind_name = kinds
                            .iter()
                            .find(|(k, _, _)| k == &r.instance)
                            .map(|(_, n, _)| n.clone())
                            .unwrap_or_else(|| r.instance.clone());
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(wp_core::util::format_time(r.time))
                                    .size(11.0)
                                    .color(t.text_weak),
                            );
                            ui.label(RichText::new(kind_name).size(11.0).color(t.accent));
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
    AddGlobal(String),
    RemoveGlobal(String),
    AddKind(String, String),
    RemoveKind(String, String),
}

fn apply(app: &mut App, act: Act) {
    let mut g = match app.guard.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    match act {
        Act::AddGlobal(word) => {
            if word.trim().is_empty() {
                drop(g);
                app.toast(ToastKind::Warn, "敏感词不能为空");
                return;
            }
            if !g.db.global.iter().any(|x| x == &word) {
                g.db.global.push(word.clone());
            }
            let _ = g.save();
            drop(g);
            app.guard_new_word.clear();
            app.toast(ToastKind::Success, format!("已添加全局敏感词：{word}"));
        }
        Act::RemoveGlobal(word) => {
            g.db.global.retain(|x| x != &word);
            let _ = g.save();
            drop(g);
            app.toast(ToastKind::Info, format!("已删除敏感词：{word}"));
        }
        Act::AddKind(kind, word) => {
            if word.trim().is_empty() {
                drop(g);
                app.toast(ToastKind::Warn, "敏感词不能为空");
                return;
            }
            g.db.add_word(&kind, word.clone());
            let _ = g.save();
            drop(g);
            app.guard_new_instance_word.clear();
            app.toast(ToastKind::Success, format!("已添加敏感词：{word}"));
        }
        Act::RemoveKind(kind, word) => {
            g.db.remove_word(&kind, &word);
            let _ = g.save();
            drop(g);
            app.toast(ToastKind::Info, format!("已删除敏感词：{word}"));
        }
    }
}
