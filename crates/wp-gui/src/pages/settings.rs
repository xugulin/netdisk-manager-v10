//! 设置页：网盘账号管理 / 传输 / 下载 / 界面 / 敏感词 / 数据目录。

use crate::app::{App, ToastKind};
use crate::theme;
use crate::widgets;
use egui::{RichText, Ui};
use wp_core::config::{DriveConfig, GuardAction};
use wp_core::model::DriveKind;

pub fn show(app: &mut App, ui: &mut Ui) {
    let t = app.theme;
    let mut acts: Vec<Act> = Vec::new();
    let status_map: std::collections::HashMap<String, String> = app
        .drives
        .iter()
        .map(|s| (s.id().to_string(), s.status.label()))
        .collect();
    let drive_rows: Vec<(DriveConfig, bool, String)> = app
        .cfg
        .drives
        .iter()
        .map(|d| {
            let runtime = status_map
                .get(&d.id)
                .cloned()
                .unwrap_or_else(|| "未加载".into());
            (
                d.clone(),
                app.drives.iter().any(|s| s.id() == d.id),
                runtime,
            )
        })
        .collect();

    egui::ScrollArea::vertical()
        .id_salt("settings_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // ── 网盘账号
            widgets::section_title(ui, &t, "网盘账号");
            ui.add_space(4.0);
            widgets::card(ui, &t, |ui| {
                egui::Grid::new("drive_grid")
                    .num_columns(6)
                    .spacing([12.0, 8.0])
                    .striped(false)
                    .show(ui, |ui| {
                        for h in ["启用", "标识", "类型", "名称", "运行状态", "操作"]
                        {
                            ui.label(RichText::new(h).size(12.0).color(t.text_weak));
                        }
                        ui.end_row();
                        for (d, loaded, runtime) in &drive_rows {
                            let mut enabled = d.enabled;
                            if ui.checkbox(&mut enabled, "").changed() {
                                acts.push(Act::ToggleDrive(d.id.clone(), enabled));
                            }
                            ui.label(RichText::new(&d.id).size(12.5).monospace().color(t.text));
                            ui.label(RichText::new(d.kind.label()).size(12.0).color(t.text_weak));
                            let mut name = d.name.clone();
                            let resp = ui.add_sized(
                                [150.0, 24.0],
                                egui::TextEdit::singleline(&mut name).hint_text("显示名"),
                            );
                            if resp.lost_focus() && name != d.name {
                                acts.push(Act::RenameDrive(d.id.clone(), name));
                            }
                            ui.label(
                                RichText::new(if *loaded {
                                    runtime.clone()
                                } else {
                                    format!("{runtime}（需启用）")
                                })
                                .size(12.0)
                                .color(
                                    if runtime.contains("已登录") {
                                        t.success
                                    } else if runtime.contains("未登录") {
                                        t.warning
                                    } else {
                                        t.text_weak
                                    },
                                ),
                            );
                            ui.horizontal(|ui| {
                                if !d.kind.is_cloud() {
                                    ui.label(RichText::new("—").size(12.0).color(t.text_weak));
                                } else if ui.small_button("登录").clicked() {
                                    acts.push(Act::Login(d.id.clone()));
                                }
                                if d.kind.is_cloud() && ui.small_button("登出").clicked() {
                                    acts.push(Act::Logout(d.id.clone()));
                                }
                                if *loaded && ui.small_button("刷新").clicked() {
                                    acts.push(Act::Refresh(d.id.clone()));
                                }
                                if ui.small_button("删除").clicked() {
                                    acts.push(Act::RemoveDrive(d.id.clone()));
                                }
                            });
                            ui.end_row();
                        }
                    });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let mut kind = app.settings_draft_kind;
                    egui::ComboBox::from_id_salt("new_kind")
                        .selected_text(kind.label())
                        .width(100.0)
                        .show_ui(ui, |ui| {
                            for k in DriveKind::ALL {
                                if k.slug() == "local" {
                                    continue;
                                }
                                if ui.selectable_label(kind == k, k.label()).clicked() {
                                    kind = k;
                                }
                            }
                        });
                    if kind != app.settings_draft_kind {
                        app.settings_draft_kind = kind;
                    }
                    if widgets::secondary_button(ui, &t, "＋ 新增网盘实例").clicked() {
                        acts.push(Act::AddDrive(kind));
                    }
                    widgets::hint(ui, &t, "同一家网盘可添加多个实例（各自独立凭证）");
                });
            });

            ui.add_space(10.0);
            // ── 传输
            widgets::section_title(ui, &t, "传输");
            ui.add_space(4.0);
            widgets::card(ui, &t, |ui| {
                let d = &mut app.settings_draft;
                egui::Grid::new("transfer_grid")
                    .num_columns(3)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("小文件阈值").size(12.5).color(t.text));
                        ui.add(
                            egui::DragValue::new(&mut d.small_threshold_mb)
                                .range(1..=1024)
                                .suffix(" MB"),
                        );
                        widgets::hint(ui, &t, "小于该大小的文件走高并发");
                        ui.end_row();
                        ui.label(RichText::new("小文件并发").size(12.5).color(t.text));
                        ui.add(egui::DragValue::new(&mut d.small_concurrency).range(1..=64));
                        widgets::hint(ui, &t, "默认 16");
                        ui.end_row();
                        ui.label(RichText::new("大文件并发").size(12.5).color(t.text));
                        ui.add(egui::DragValue::new(&mut d.large_concurrency).range(1..=32));
                        widgets::hint(ui, &t, "默认 3，过高易触发风控");
                        ui.end_row();
                        ui.label(RichText::new("失败重试次数").size(12.5).color(t.text));
                        ui.add(egui::DragValue::new(&mut d.retry).range(0..=10));
                        widgets::hint(ui, &t, "指数退避");
                        ui.end_row();
                        ui.label(RichText::new("自适应并发").size(12.5).color(t.text));
                        ui.checkbox(&mut d.adaptive, "");
                        widgets::hint(ui, &t, "失败时降并发、稳定时回升");
                        ui.end_row();
                        ui.label(RichText::new("全局限速").size(12.5).color(t.text));
                        ui.add(
                            egui::DragValue::new(&mut d.speed_limit_mbps)
                                .range(0..=10240)
                                .suffix(" MB/s"),
                        );
                        widgets::hint(ui, &t, "0 = 不限速");
                        ui.end_row();
                    });
            });

            ui.add_space(10.0);
            // ── 下载
            widgets::section_title(ui, &t, "下载");
            ui.add_space(4.0);
            widgets::card(ui, &t, |ui| {
                let d = &mut app.settings_draft;
                egui::Grid::new("download_grid")
                    .num_columns(3)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("分段并发数").size(12.5).color(t.text));
                        ui.add(egui::DragValue::new(&mut d.segments).range(1..=16));
                        widgets::hint(ui, &t, "大文件多连接分段下载");
                        ui.end_row();
                        ui.label(RichText::new("分段阈值").size(12.5).color(t.text));
                        ui.add(
                            egui::DragValue::new(&mut d.segment_threshold_mb)
                                .range(1..=4096)
                                .suffix(" MB"),
                        );
                        widgets::hint(ui, &t, "超过此大小才分段");
                        ui.end_row();
                        ui.label(RichText::new("断点续传").size(12.5).color(t.text));
                        ui.checkbox(&mut d.resume, "");
                        widgets::hint(ui, &t, "中断后保留 .part 文件，下次续传");
                        ui.end_row();
                    });
            });

            ui.add_space(10.0);
            // ── 界面
            widgets::section_title(ui, &t, "界面");
            ui.add_space(4.0);
            widgets::card(ui, &t, |ui| {
                let d = &mut app.settings_draft;
                egui::Grid::new("ui_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("主题").size(12.5).color(t.text));
                        egui::ComboBox::from_id_salt("theme_setting")
                            .selected_text(
                                theme::THEMES
                                    .iter()
                                    .find(|x| x.name == d.theme)
                                    .map(|x| x.display)
                                    .unwrap_or("默认"),
                            )
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                for th in theme::THEMES.iter() {
                                    if ui
                                        .selectable_label(d.theme == th.name, th.display)
                                        .clicked()
                                    {
                                        d.theme = th.name.to_string();
                                    }
                                }
                            });
                        ui.end_row();
                        ui.label(RichText::new("中文字体文件").size(12.5).color(t.text));
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut d.font_path)
                                    .desired_width(320.0)
                                    .hint_text("留空 = 自动探测系统字体（修改后重启生效）"),
                            );
                        });
                        ui.end_row();
                        ui.label(RichText::new("下载目录").size(12.5).color(t.text));
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut d.download_dir)
                                    .desired_width(320.0)
                                    .hint_text("留空 = 数据目录/下载"),
                            );
                            if ui.button("选择…").clicked() {
                                acts.push(Act::ChooseDownloadDir);
                            }
                        });
                        ui.end_row();
                        ui.label(RichText::new("保留失败临时文件").size(12.5).color(t.text));
                        ui.checkbox(&mut d.keep_failed_temp, "跨盘传输失败时保留中转文件");
                        ui.end_row();
                    });
            });

            ui.add_space(10.0);
            // ── 敏感词
            widgets::section_title(ui, &t, "敏感词守卫");
            ui.add_space(4.0);
            widgets::card(ui, &t, |ui| {
                let d = &mut app.settings_draft;
                ui.horizontal(|ui| {
                    ui.checkbox(&mut d.guard_enabled, "上传前启用敏感词守卫");
                    ui.add_space(16.0);
                    ui.label(RichText::new("命中策略").size(12.5).color(t.text));
                    egui::ComboBox::from_id_salt("guard_action")
                        .selected_text(guard_action_label(d.guard_action))
                        .width(140.0)
                        .show_ui(ui, |ui| {
                            for a in [GuardAction::Warn, GuardAction::Block, GuardAction::Rename] {
                                if ui
                                    .selectable_label(d.guard_action == a, guard_action_label(a))
                                    .clicked()
                                {
                                    d.guard_action = a;
                                }
                            }
                        });
                });
                ui.add_space(4.0);
                let (total, per_instance) = {
                    let g = app
                        .guard
                        .lock()
                        .map(|g| (g.db.global.len(), g.db.instances.len()))
                        .unwrap_or((0, 0));
                    g
                };
                widgets::hint(
                    ui,
                    &t,
                    &format!(
                        "当前词库：全局 {} 词 · {} 个网盘实例有专属词条（在「敏感词」页管理）",
                        total, per_instance
                    ),
                );
            });

            ui.add_space(10.0);
            // ── 数据与路径
            widgets::section_title(ui, &t, "数据与路径");
            ui.add_space(4.0);
            widgets::card(ui, &t, |ui| {
                widgets::kv_row(ui, &t, "数据目录", &app.paths.root.display().to_string());
                widgets::kv_row(ui, &t, "配置文件", &app.cfg_path.display().to_string());
                widgets::kv_row(
                    ui,
                    &t,
                    "任务库",
                    &app.paths.tasks_file().display().to_string(),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("📂 打开数据目录").clicked() {
                        acts.push(Act::OpenDataDir);
                    }
                    if ui.button("⤓ 打开下载目录").clicked() {
                        acts.push(Act::OpenDownloadDir);
                    }
                    if ui.button("📋 复制配置").clicked() {
                        acts.push(Act::CopyConfig);
                    }
                });
            });

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if widgets::primary_button(ui, &t, "💾 保存设置").clicked() {
                    acts.push(Act::Save);
                }
                if widgets::secondary_button(ui, &t, "恢复默认").clicked() {
                    acts.push(Act::Reset);
                }
                widgets::hint(ui, &t, "保存后立即生效；主题实时预览");
            });
            ui.add_space(30.0);
        });

    for a in acts {
        apply(app, a);
    }
}

fn guard_action_label(a: GuardAction) -> &'static str {
    match a {
        GuardAction::Warn => "仅警告（继续上传）",
        GuardAction::Block => "拦截（失败）",
        GuardAction::Rename => "自动改名规避",
    }
}

enum Act {
    AddDrive(DriveKind),
    RemoveDrive(String),
    RenameDrive(String, String),
    ToggleDrive(String, bool),
    Login(String),
    Logout(String),
    Refresh(String),
    ChooseDownloadDir,
    Save,
    Reset,
    OpenDataDir,
    OpenDownloadDir,
    CopyConfig,
}

fn apply(app: &mut App, act: Act) {
    match act {
        Act::AddDrive(kind) => {
            // 同一网盘可挂多个账号：新增实例后直接切到它并打开登录对话框
            app.add_account_and_login(kind);
        }
        Act::RemoveDrive(id) => {
            app.cfg.drives.retain(|d| d.id != id);
            app.save_config();
            app.rebuild_drives();
            app.toast(ToastKind::Info, format!("已删除网盘实例：{id}"));
        }
        Act::RenameDrive(id, name) => {
            if let Some(d) = app.cfg.drives.iter_mut().find(|d| d.id == id) {
                d.name = name;
            }
            if let Some(s) = app.drives.iter_mut().find(|s| s.id() == id) {
                s.cfg.name = app
                    .cfg
                    .drives
                    .iter()
                    .find(|d| d.id == id)
                    .map(|d| d.name.clone())
                    .unwrap_or_default();
            }
            app.save_config();
        }
        Act::ToggleDrive(id, enabled) => {
            if let Some(d) = app.cfg.drives.iter_mut().find(|d| d.id == id) {
                d.enabled = enabled;
            }
            app.save_config();
            app.rebuild_drives();
            app.toast(
                ToastKind::Info,
                format!("网盘实例 {id} 已{}", if enabled { "启用" } else { "停用" }),
            );
        }
        Act::Login(id) => {
            if let Some(idx) = app.slot_idx(&id) {
                app.active = idx;
                app.page = crate::app::Page::Browser;
                app.open_login_dialog();
            } else {
                app.toast(ToastKind::Warn, "该实例未启用，无法登录");
            }
        }
        Act::Logout(id) => {
            if let Some(drive) = app.slot_by_id(&id).and_then(|s| s.drive.clone()) {
                let did = id.clone();
                app.spawn(async move {
                    crate::app::Event::LoggedOut {
                        drive: did,
                        result: drive.logout().await,
                    }
                });
            }
        }
        Act::Refresh(id) => app.refresh_account(&id),
        Act::ChooseDownloadDir => app.open_file_picker(true),
        Act::Save => app.apply_settings(),
        Act::Reset => {
            app.settings_draft =
                crate::app::settings_from_config(&wp_core::config::AppConfig::default());
            app.toast(ToastKind::Info, "已恢复默认值（点「保存设置」生效）");
        }
        Act::OpenDataDir => {
            let dir = app.paths.root.clone();
            if let Err(e) = wp_core::paths::open_path(&dir) {
                app.toast(ToastKind::Error, format!("打开失败：{e}"));
            }
        }
        Act::OpenDownloadDir => {
            let dir = app.download_dir();
            if let Err(e) = wp_core::paths::open_path(&dir) {
                app.toast(ToastKind::Error, format!("打开失败：{e}"));
            }
        }
        Act::CopyConfig => {
            if let Ok(text) = serde_json::to_string_pretty(&app.cfg) {
                if let Some(ctx) = &app.ctx {
                    ctx.copy_text(text);
                }
                app.toast(ToastKind::Success, "配置 JSON 已复制到剪贴板");
            }
        }
    }
}
