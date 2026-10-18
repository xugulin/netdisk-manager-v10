//! 工具页：回收站 / 云任务（云添加）/ 上传记录 / 分享管理+转存 / 分类浏览。
//!
//! 全部按当前网盘的 `DriveCaps` 能力动态显示；数据异步加载（Event 回传），
//! 渲染遵循「只读快照 → 收集动作 → 统一应用」的模式。

use crate::app::{App, Page, ToastKind, ToolsTab};
use crate::theme::{self, Theme};
use crate::widgets;
use egui::{Color32, RichText, Sense, Ui, Vec2};
use wp_core::model::{CloudTask, RecycleEntry, RemoteEntry, ShareInfo, UploadRecord};
use wp_drive::DriveCaps;

pub fn show(app: &mut App, ui: &mut Ui) {
    let t = app.theme;
    let drive_id = app.active_drive_id();
    if drive_id.is_empty() {
        widgets::empty_state(
            ui,
            &t,
            "还没有可用的网盘",
            "请在「设置 → 网盘账号」里启用网盘",
        );
        return;
    }
    let caps = app.active_slot().and_then(|s| s.caps()).unwrap_or_default();

    // ── 顶部：标签 + 刷新
    let mut acts: Vec<Act> = Vec::new();
    let cur = app.tools.tab.unwrap_or(ToolsTab::Recycle);
    if !tab_supported(&caps, cur) {
        // 当前标签不支持 → 自动切到第一个支持的
        if let Some(first) = ALL_TABS.iter().copied().find(|x| tab_supported(&caps, *x)) {
            app.tools.tab = Some(first);
        }
    }
    let cur = app.tools.tab.unwrap_or(ToolsTab::Recycle);
    ui.horizontal(|ui| {
        for tab in ALL_TABS {
            if !tab_supported(&caps, tab) {
                continue;
            }
            let selected = tab == cur;
            let text = RichText::new(tab.label()).size(13.5).color(if selected {
                t.accent
            } else {
                t.text
            });
            let fill = if selected {
                t.hover
            } else {
                Color32::TRANSPARENT
            };
            if ui
                .add(
                    egui::Button::new(text)
                        .fill(fill)
                        .corner_radius(theme::radius(6)),
                )
                .clicked()
                && !selected
            {
                app.tools.tab = Some(tab);
                acts.push(Act::Reload(tab));
            }
            ui.add_space(2.0);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::tool_button(ui, &t, "⟳ 刷新", true).clicked() {
                acts.push(Act::Reload(cur));
            }
        });
    });
    ui.separator();

    // 首次进入 / 切网盘 / 切标签 → 自动加载
    let key = format!(
        "{}:{:?}:{}",
        drive_id,
        app.tools.tab.unwrap_or(ToolsTab::Recycle),
        app.tools.category
    );
    if app.tools.loaded_key != key {
        app.tools.loaded_key = key;
        acts.push(Act::Reload(cur));
    }

    match cur {
        ToolsTab::Recycle => recycle_tab(app, ui, &t, &caps, &mut acts),
        ToolsTab::Cloud => cloud_tab(app, ui, &t, &caps, &mut acts),
        ToolsTab::Records => records_tab(app, ui, &t, &caps, &mut acts),
        ToolsTab::Shares => shares_tab(app, ui, &t, &caps, &mut acts),
        ToolsTab::Categories => categories_tab(app, ui, &t, &caps, &mut acts),
    }

    for a in acts {
        apply(app, &drive_id, a);
    }
}

const ALL_TABS: [ToolsTab; 5] = [
    ToolsTab::Recycle,
    ToolsTab::Cloud,
    ToolsTab::Records,
    ToolsTab::Shares,
    ToolsTab::Categories,
];

fn tab_supported(caps: &DriveCaps, tab: ToolsTab) -> bool {
    match tab {
        ToolsTab::Recycle => caps.recycle_list,
        ToolsTab::Cloud => caps.cloud_add || caps.cloud_tasks,
        ToolsTab::Records => caps.upload_records,
        ToolsTab::Shares => caps.share || caps.share_info || caps.share_save,
        ToolsTab::Categories => caps.categories,
    }
}

fn unsupported(ui: &mut Ui, t: &Theme, what: &str) {
    widgets::empty_state(
        ui,
        t,
        &format!("当前网盘不支持{what}"),
        "这是网盘服务端没有提供对应接口，不是客户端限制；可切到其它网盘查看",
    );
}

// ═══════════════════════ 回收站 ═══════════════════════

fn recycle_tab(app: &mut App, ui: &mut Ui, t: &Theme, caps: &DriveCaps, acts: &mut Vec<Act>) {
    if !caps.recycle_list {
        unsupported(ui, t, "回收站");
        return;
    }
    let selected: Vec<String> = app.tools.recycle_selected.iter().cloned().collect();
    ui.horizontal(|ui| {
        if widgets::tool_button(
            ui,
            t,
            "↩ 恢复所选",
            caps.recycle_restore && !selected.is_empty(),
        )
        .clicked()
        {
            acts.push(Act::RecycleRestore(selected.clone()));
        }
        if widgets::tool_button(
            ui,
            t,
            "删除所选（彻底）",
            caps.recycle_delete && !selected.is_empty(),
        )
        .clicked()
        {
            acts.push(Act::RecycleDelete(selected.clone()));
        }
        if widgets::danger_button(ui, t, "清空回收站").clicked() && caps.recycle_clear {
            acts.push(Act::RecycleClear);
        }
        if widgets::tool_button(ui, t, "全选", !app.tools.recycle.data.is_empty()).clicked() {
            acts.push(Act::RecycleSelectAll);
        }
        if widgets::tool_button(ui, t, "取消选择", !selected.is_empty()).clicked() {
            acts.push(Act::RecycleClearSel);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            widgets::hint(
                ui,
                t,
                &format!(
                    "共 {} 项{}",
                    app.tools.recycle.data.len(),
                    if selected.is_empty() {
                        String::new()
                    } else {
                        format!("，已选 {}", selected.len())
                    }
                ),
            );
        });
    });
    ui.add_space(4.0);
    if app.tools.recycle.loading {
        widgets::loading(ui, t, "正在加载回收站…");
        return;
    }
    if !app.tools.recycle.error.is_empty() {
        widgets::error_banner(ui, t, &app.tools.recycle.error);
        return;
    }
    if app.tools.recycle.data.is_empty() {
        widgets::empty_state(ui, t, "回收站是空的", "被删除的文件会先进入回收站");
        return;
    }
    let mut toggle: Option<String> = None;
    let rows: Vec<RecycleEntry> = app.tools.recycle.data.clone();
    table_header(
        ui,
        t,
        &["", "名称", "大小", "删除时间", "原路径"],
        &[0.28, 0.40, 0.12, 0.16, 0.42],
    );
    egui::ScrollArea::vertical()
        .id_salt("recycle_list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for e in &rows {
                let checked = app.tools.recycle_selected.contains(&e.id);
                ui.horizontal(|ui| {
                    let mut c = checked;
                    if ui.checkbox(&mut c, "").changed() {
                        toggle = Some(e.id.clone());
                    }
                    ui.add_sized(
                        [250.0, 20.0],
                        egui::Label::new(
                            RichText::new(format!(
                                "{} {}",
                                widgets::file_icon(e.is_dir, &e.name),
                                e.name
                            ))
                            .size(13.0)
                            .color(t.text),
                        )
                        .truncate(),
                    );
                    ui.add_sized(
                        [90.0, 20.0],
                        egui::Label::new(
                            RichText::new(if e.size > 0 {
                                wp_core::util::format_size(e.size)
                            } else {
                                "—".into()
                            })
                            .size(12.0)
                            .color(t.text_weak),
                        ),
                    );
                    ui.add_sized(
                        [130.0, 20.0],
                        egui::Label::new(
                            RichText::new(
                                e.deleted_at
                                    .map(wp_core::util::format_time)
                                    .unwrap_or_else(|| "—".into()),
                            )
                            .size(12.0)
                            .color(t.text_weak),
                        ),
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(if e.original_path.is_empty() {
                                e.path.clone()
                            } else {
                                e.original_path.clone()
                            })
                            .size(11.5)
                            .color(t.text_weak),
                        )
                        .truncate(),
                    )
                    .on_hover_text(&e.path);
                });
            }
        });
    if let Some(id) = toggle {
        if app.tools.recycle_selected.contains(&id) {
            app.tools.recycle_selected.remove(&id);
        } else {
            app.tools.recycle_selected.insert(id);
        }
    }
}

// ═══════════════════════ 云任务 ═══════════════════════

fn cloud_tab(app: &mut App, ui: &mut Ui, t: &Theme, caps: &DriveCaps, acts: &mut Vec<Act>) {
    if caps.cloud_add {
        widgets::card(ui, t, |ui| {
            widgets::section_title(ui, t, "新建云任务（离线下载 / BT）");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("链接").size(12.5).color(t.text));
                ui.add(
                    egui::TextEdit::singleline(&mut app.tools.cloud_url)
                        .desired_width(520.0)
                        .hint_text("https://... 或 magnet:?xt=urn:btih:..."),
                );
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("保存到").size(12.5).color(t.text));
                ui.add(
                    egui::TextEdit::singleline(&mut app.tools.cloud_dest)
                        .desired_width(320.0)
                        .hint_text("/（根目录）"),
                );
                let dest = app.tools.cloud_dest.clone();
                let dest = if dest.trim().is_empty() {
                    "/".to_string()
                } else {
                    dest
                };
                if ui
                    .add_enabled(
                        !app.tools.cloud_busy && !app.tools.cloud_url.trim().is_empty(),
                        egui::Button::new(RichText::new("＋ 添加任务"))
                            .fill(t.accent)
                            .corner_radius(theme::radius(6)),
                    )
                    .clicked()
                {
                    acts.push(Act::CloudAdd(app.tools.cloud_url.clone(), dest));
                }
                if app.tools.cloud_busy {
                    ui.spinner();
                }
            });
        });
        ui.add_space(8.0);
    }
    if !caps.cloud_tasks {
        if caps.cloud_add {
            return;
        }
        unsupported(ui, t, "云任务");
        return;
    }
    if app.tools.cloud_tasks.loading {
        widgets::loading(ui, t, "正在加载云任务…");
        return;
    }
    if !app.tools.cloud_tasks.error.is_empty() {
        widgets::error_banner(ui, t, &app.tools.cloud_tasks.error);
        return;
    }
    let rows: Vec<CloudTask> = app.tools.cloud_tasks.data.clone();
    if rows.is_empty() {
        widgets::empty_state(ui, t, "没有云任务", "在上面填链接即可创建离线下载任务");
        return;
    }
    table_header(
        ui,
        t,
        &["名称", "状态", "进度", "保存路径", "链接"],
        &[0.30, 0.10, 0.30, 0.20, 0.10],
    );
    egui::ScrollArea::vertical()
        .id_salt("cloud_tasks")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for task in &rows {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [240.0, 20.0],
                        egui::Label::new(RichText::new(&task.name).size(12.5).color(t.text))
                            .truncate(),
                    );
                    let status_color = match task.status.as_str() {
                        "2" => t.success,
                        "3" => t.error,
                        "1" => t.accent,
                        _ => t.warning,
                    };
                    ui.add_sized(
                        [80.0, 20.0],
                        egui::Label::new(
                            RichText::new(&task.status_text)
                                .size(12.0)
                                .color(status_color),
                        ),
                    );
                    ui.add(
                        egui::ProgressBar::new(task.progress.clamp(0.0, 1.0))
                            .desired_width(220.0)
                            .fill(status_color)
                            .corner_radius(theme::radius(3)),
                    );
                    ui.add_sized(
                        [160.0, 20.0],
                        egui::Label::new(
                            RichText::new(wp_core::util::ellipsis(&task.save_path, 24))
                                .size(11.0)
                                .color(t.text_weak),
                        ),
                    );
                    ui.add_sized(
                        [120.0, 20.0],
                        egui::Label::new(
                            RichText::new(wp_core::util::ellipsis(&task.url, 20))
                                .size(10.5)
                                .color(t.text_weak),
                        ),
                    )
                    .on_hover_text(&task.url);
                });
                if !task.error.is_empty() {
                    ui.label(
                        RichText::new(format!("  ⚠ {}", task.error))
                            .size(11.0)
                            .color(t.error),
                    );
                }
            }
        });
}

// ═══════════════════════ 上传记录 ═══════════════════════

fn records_tab(app: &mut App, ui: &mut Ui, t: &Theme, caps: &DriveCaps, _acts: &mut Vec<Act>) {
    if !caps.upload_records {
        unsupported(ui, t, "上传记录");
        return;
    }
    if app.tools.records.loading {
        widgets::loading(ui, t, "正在加载记录…");
        return;
    }
    if !app.tools.records.error.is_empty() {
        widgets::error_banner(ui, t, &app.tools.records.error);
        return;
    }
    let rows: Vec<UploadRecord> = app.tools.records.data.clone();
    if rows.is_empty() {
        widgets::empty_state(ui, t, "暂无记录", "上传/下载/删除操作会出现在这里");
        return;
    }
    table_header(
        ui,
        t,
        &["时间", "操作", "名称", "大小", "路径"],
        &[0.16, 0.10, 0.30, 0.12, 0.32],
    );
    egui::ScrollArea::vertical()
        .id_salt("records_list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for r in &rows {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [130.0, 20.0],
                        egui::Label::new(
                            RichText::new(
                                r.time
                                    .map(wp_core::util::format_time)
                                    .unwrap_or_else(|| "—".into()),
                            )
                            .size(11.5)
                            .color(t.text_weak),
                        ),
                    );
                    ui.add_sized(
                        [70.0, 20.0],
                        egui::Label::new(RichText::new(&r.action).size(12.0).color(t.accent)),
                    );
                    ui.add_sized(
                        [240.0, 20.0],
                        egui::Label::new(RichText::new(&r.name).size(12.5).color(t.text))
                            .truncate(),
                    );
                    ui.add_sized(
                        [90.0, 20.0],
                        egui::Label::new(
                            RichText::new(if r.size > 0 {
                                wp_core::util::format_size(r.size)
                            } else {
                                "—".into()
                            })
                            .size(11.5)
                            .color(t.text_weak),
                        ),
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(wp_core::util::ellipsis(&r.path, 40))
                                .size(11.0)
                                .color(t.text_weak),
                        )
                        .truncate(),
                    );
                });
            }
        });
}

// ═══════════════════════ 分享管理 / 转存 ═══════════════════════

fn shares_tab(app: &mut App, ui: &mut Ui, t: &Theme, caps: &DriveCaps, acts: &mut Vec<Act>) {
    let mut cancel_id: Option<String> = None;
    if caps.share {
        widgets::card(ui, t, |ui| {
            widgets::section_title(ui, t, "我的分享");
            ui.add_space(4.0);
            if app.tools.shares.loading {
                widgets::loading(ui, t, "加载中…");
            } else if !app.tools.shares.error.is_empty() {
                widgets::error_banner(ui, t, &app.tools.shares.error);
            } else if app.tools.shares.data.is_empty() {
                widgets::hint(
                    ui,
                    t,
                    "暂无分享（可在网盘页勾选文件 →「⋯ 更多 → 分享…」创建）",
                );
            } else {
                let rows: Vec<ShareInfo> = app.tools.shares.data.clone();
                egui::ScrollArea::vertical()
                    .id_salt("share_list")
                    .max_height(220.0)
                    .show(ui, |ui| {
                        for s in &rows {
                            ui.horizontal(|ui| {
                                ui.add_sized(
                                    [420.0, 20.0],
                                    egui::Label::new(
                                        RichText::new(wp_core::util::ellipsis(
                                            if s.title.is_empty() { &s.url } else { &s.title },
                                            52,
                                        ))
                                        .size(12.5)
                                        .color(t.text),
                                    ),
                                );
                                if !s.password.is_empty() {
                                    ui.label(
                                        RichText::new(format!("提取码 {}", s.password))
                                            .size(11.5)
                                            .color(t.warning),
                                    );
                                }
                                ui.add_space(8.0);
                                if ui.small_button("复制链接").clicked() {
                                    if let Some(ctx) = &app.ctx {
                                        ctx.copy_text(s.url.clone());
                                    }
                                }
                                if ui.small_button("取消").clicked() {
                                    cancel_id = Some(s.share_id.clone());
                                }
                            });
                        }
                    });
            }
        });
        ui.add_space(10.0);
    }

    if caps.share_info || caps.share_save {
        widgets::card(ui, t, |ui| {
            widgets::section_title(ui, t, "转存分享链接");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("分享链接").size(12.5).color(t.text));
                ui.add(
                    egui::TextEdit::singleline(&mut app.tools.share_url)
                        .desired_width(420.0)
                        .hint_text("https://pan.baidu.com/s/... 或 https://pan.quark.cn/s/..."),
                );
                ui.label(RichText::new("提取码").size(12.5).color(t.text));
                ui.add(
                    egui::TextEdit::singleline(&mut app.tools.share_pwd)
                        .desired_width(90.0)
                        .hint_text("可选"),
                );
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("保存到").size(12.5).color(t.text));
                ui.add(
                    egui::TextEdit::singleline(&mut app.tools.share_dest)
                        .desired_width(320.0)
                        .hint_text("/（根目录）"),
                );
                let dest = app.tools.share_dest.clone();
                let dest = if dest.trim().is_empty() {
                    "/".to_string()
                } else {
                    dest
                };
                let url = app.tools.share_url.clone();
                let pwd = app.tools.share_pwd.clone();
                if ui
                    .add_enabled(
                        caps.share_info && !app.tools.share_busy && !url.trim().is_empty(),
                        egui::Button::new("校验链接"),
                    )
                    .clicked()
                {
                    acts.push(Act::ShareInfo(url.clone(), pwd.clone()));
                }
                if ui
                    .add_enabled(
                        caps.share_save && !app.tools.share_busy && !url.trim().is_empty(),
                        egui::Button::new(RichText::new("转存到我的网盘").color(if t.dark {
                            Color32::from_rgb(20, 20, 24)
                        } else {
                            Color32::WHITE
                        }))
                        .fill(t.accent)
                        .corner_radius(theme::radius(6)),
                    )
                    .clicked()
                {
                    acts.push(Act::ShareSave(url, pwd, dest));
                }
                if app.tools.share_busy {
                    ui.spinner();
                }
            });
            if let Some(detail) = &app.tools.share_detail {
                ui.add_space(6.0);
                widgets::success_banner(
                    ui,
                    t,
                    &format!(
                        "{}（{} 个文件{}）",
                        detail.title,
                        detail.files.len(),
                        if detail.note.is_empty() {
                            String::new()
                        } else {
                            format!("；{}", detail.note)
                        }
                    ),
                );
                for f in detail.files.iter().take(8) {
                    widgets::hint(
                        ui,
                        t,
                        &format!("· {}（{}）", f.name, wp_core::util::format_size(f.size)),
                    );
                }
            }
        });
    }
    if !caps.share && !caps.share_info && !caps.share_save {
        unsupported(ui, t, "分享");
    }
    if let Some(id) = cancel_id {
        acts.push(Act::CancelShare(id));
    }
}

// ═══════════════════════ 分类浏览 ═══════════════════════

fn categories_tab(app: &mut App, ui: &mut Ui, t: &Theme, caps: &DriveCaps, acts: &mut Vec<Act>) {
    if !caps.categories {
        unsupported(ui, t, "分类浏览");
        return;
    }
    let cats = [
        ("all", "全部"),
        ("video", "视频"),
        ("audio", "音频"),
        ("image", "图片"),
        ("doc", "文档"),
        ("app", "应用"),
        ("other", "其它"),
    ];
    ui.horizontal(|ui| {
        ui.label(RichText::new("分类").size(12.5).color(t.text));
        let mut sel = app.tools.category.clone();
        egui::ComboBox::from_id_salt("category_combo")
            .selected_text(
                cats.iter()
                    .find(|(k, _)| *k == sel)
                    .map(|(_, v)| *v)
                    .unwrap_or("视频"),
            )
            .width(100.0)
            .show_ui(ui, |ui| {
                for (k, v) in cats {
                    if ui.selectable_label(sel == k, v).clicked() {
                        sel = k.to_string();
                    }
                }
            });
        if sel != app.tools.category {
            app.tools.category = sel.clone();
            acts.push(Act::Reload(ToolsTab::Categories));
        }
    });
    ui.add_space(4.0);
    if app.tools.categories.loading {
        widgets::loading(ui, t, "正在加载分类文件…");
        return;
    }
    if !app.tools.categories.error.is_empty() {
        widgets::error_banner(ui, t, &app.tools.categories.error);
        return;
    }
    let rows: Vec<RemoteEntry> = app.tools.categories.data.clone();
    if rows.is_empty() {
        widgets::empty_state(ui, t, "该分类下没有文件", "换个分类试试");
        return;
    }
    let mut open: Option<String> = None;
    table_header(
        ui,
        t,
        &["名称", "大小", "所在目录", ""],
        &[0.32, 0.12, 0.42, 0.12],
    );
    egui::ScrollArea::vertical()
        .id_salt("category_list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for e in &rows {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [300.0, 20.0],
                        egui::Label::new(
                            RichText::new(format!(
                                "{} {}",
                                widgets::file_icon(e.is_dir, &e.name),
                                e.name
                            ))
                            .size(12.5)
                            .color(t.text),
                        )
                        .truncate(),
                    );
                    ui.add_sized(
                        [90.0, 20.0],
                        egui::Label::new(
                            RichText::new(e.size_text()).size(11.5).color(t.text_weak),
                        ),
                    );
                    ui.add_sized(
                        [360.0, 20.0],
                        egui::Label::new(
                            RichText::new(e.path.clone()).size(11.0).color(t.text_weak),
                        )
                        .truncate(),
                    );
                    if ui.small_button("定位").clicked() {
                        open = Some(e.path.clone());
                    }
                });
            }
        });
    if let Some(path) = open {
        acts.push(Act::OpenPath(path));
    }
}

// ═══════════════════════ 公共小部件 ═══════════════════════

fn table_header(ui: &mut Ui, t: &Theme, headers: &[&str], _weights: &[f32]) {
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), theme::size::HEADER_H),
        Sense::hover(),
    );
    ui.painter().rect_filled(rect, 0, t.header_bg);
    let mut x = rect.left() + 8.0;
    let font = egui::FontId::proportional(12.0);
    for h in headers {
        ui.painter().text(
            egui::pos2(x, rect.center().y),
            egui::Align2::LEFT_CENTER,
            *h,
            font.clone(),
            t.text_weak,
        );
        x += 120.0;
    }
}

// ═══════════════════════ 动作 ═══════════════════════

enum Act {
    Reload(ToolsTab),
    RecycleRestore(Vec<String>),
    RecycleDelete(Vec<String>),
    RecycleClear,
    RecycleSelectAll,
    RecycleClearSel,
    CloudAdd(String, String),
    ShareInfo(String, String),
    ShareSave(String, String, String),
    CancelShare(String),
    OpenPath(String),
}

fn apply(app: &mut App, drive_id: &str, act: Act) {
    match act {
        Act::Reload(tab) => match tab {
            ToolsTab::Recycle => app.recycle_load(drive_id),
            ToolsTab::Cloud => app.cloud_tasks_load(drive_id),
            ToolsTab::Records => app.records_load(drive_id),
            ToolsTab::Shares => app.shares_load(drive_id),
            ToolsTab::Categories => {
                let cat = app.tools.category.clone();
                let cat = if cat.is_empty() {
                    "video".to_string()
                } else {
                    cat
                };
                app.categories_load(drive_id, cat);
            }
        },
        Act::RecycleRestore(ids) => app.recycle_restore(drive_id, ids),
        Act::RecycleDelete(ids) => app.recycle_delete(drive_id, ids),
        Act::RecycleClear => app.recycle_clear(drive_id),
        Act::RecycleSelectAll => {
            let ids: Vec<String> = app
                .tools
                .recycle
                .data
                .iter()
                .map(|e| e.id.clone())
                .collect();
            app.tools.recycle_selected = ids.into_iter().collect();
        }
        Act::RecycleClearSel => app.tools.recycle_selected.clear(),
        Act::CloudAdd(url, dest) => {
            app.tools.cloud_url.clear();
            app.cloud_add(drive_id, url, dest);
        }
        Act::ShareInfo(url, pwd) => app.share_info(drive_id, url, pwd),
        Act::ShareSave(url, pwd, dest) => app.share_save(drive_id, url, pwd, dest),
        Act::CancelShare(id) => {
            let Some(slot) = app.slot_by_id(drive_id) else {
                return;
            };
            let Some(drive) = slot.drive.clone() else {
                return;
            };
            app.spawn(async move {
                crate::app::Event::OpDone {
                    message: "取消分享".into(),
                    refresh: false,
                    result: drive.cancel_shares(&[id]).await.map(|r| r.summary()),
                }
            });
        }
        Act::OpenPath(path) => {
            let parent = wp_core::util::parent_path(&path);
            app.navigate_drive(drive_id, &parent, true);
            app.page = Page::Browser;
            app.toast(ToastKind::Info, format!("已定位到：{parent}"));
        }
    }
}
