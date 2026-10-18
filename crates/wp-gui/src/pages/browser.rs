//! 网盘页：文件浏览（路径栏 + 操作工具栏 + 文件表格 + 状态行）。
//!
//! 渲染与操作分离：本帧只读 App（用轻量快照），把用户动作收集成 [`Action`]，
//! 渲染结束后统一执行，避免 UI 闭包里持有 `&mut App` 与只读借用的冲突。

use crate::app::{
    AccountStatus, App, BrowseState, DetailsDialog, Dialog, DirPickerAction, ToastKind, ToolsTab,
};
use crate::theme::{self, Theme};
use crate::widgets;
use egui::{Color32, RichText, Sense, Stroke, Ui, Vec2};
use std::collections::BTreeSet;
use std::sync::Arc;
use wp_core::model::RemoteEntry;
use wp_core::util::SortBy;
use wp_core::util::{base_name, breadcrumb, parent_path};

/// 浏览页轻量快照（Arc 共享条目，克隆代价低）。
struct Snap {
    drive_id: String,
    path: String,
    entries: Arc<Vec<RemoteEntry>>,
    selected: BTreeSet<String>,
    loading: bool,
    error: Option<String>,
    filter: String,
    sort: SortBy,
    desc: bool,
    back_len: usize,
    fwd_len: usize,
}

fn snap(app: &App, drive_id: &str) -> Option<Snap> {
    let s: &BrowseState = app.browse_ref(drive_id)?;
    Some(Snap {
        drive_id: drive_id.to_string(),
        path: s.path.clone(),
        entries: s.entries.clone(),
        selected: s.selected.clone(),
        loading: s.loading,
        error: s.error.clone(),
        filter: s.filter.clone(),
        sort: s.sort,
        desc: s.desc,
        back_len: s.back.len(),
        fwd_len: s.forward.len(),
    })
}

impl Snap {
    fn visible(&self) -> Vec<usize> {
        let kw = self.filter.trim().to_lowercase();
        let mut idx: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| kw.is_empty() || e.name.to_lowercase().contains(&kw))
            .map(|(i, _)| i)
            .collect();
        let entries = &self.entries;
        let (sort, desc) = (self.sort, self.desc);
        idx.sort_by(|&a, &b| {
            let (ea, eb) = (&entries[a], &entries[b]);
            match (ea.is_dir, eb.is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => {
                    let o = match sort {
                        SortBy::Name => ea.name.to_lowercase().cmp(&eb.name.to_lowercase()),
                        SortBy::Size => ea.size.cmp(&eb.size),
                        SortBy::Modified => ea.modified.unwrap_or(0).cmp(&eb.modified.unwrap_or(0)),
                        SortBy::Kind => ea
                            .name
                            .rsplit('.')
                            .next()
                            .unwrap_or("")
                            .cmp(eb.name.rsplit('.').next().unwrap_or("")),
                    };
                    if desc {
                        o.reverse()
                    } else {
                        o
                    }
                }
            }
        });
        idx
    }
}

pub fn show(app: &mut App, ui: &mut Ui) {
    let t = app.theme;
    let drive_id = app.active_drive_id();
    if drive_id.is_empty() {
        widgets::empty_state(
            ui,
            &t,
            "还没有可用的网盘",
            "请在「设置 → 网盘账号」里启用或新增网盘实例",
        );
        return;
    }
    let Some(slot_idx) = app.slot_idx(&drive_id) else {
        return;
    };
    let (drive_ok, status, init_error, caps) = {
        let slot = &app.drives[slot_idx];
        (
            slot.drive.is_some(),
            slot.status.clone(),
            slot.init_error.clone(),
            slot.caps(),
        )
    };
    if !drive_ok {
        widgets::empty_state(
            ui,
            &t,
            "该网盘未初始化成功",
            &init_error.unwrap_or_else(|| "请检查配置/数据目录权限".into()),
        );
        return;
    }
    let caps = caps.unwrap_or_default();
    let Some(s) = snap(app, &drive_id) else {
        return;
    };
    let mut actions: Vec<Action> = Vec::new();

    // ── 路径栏 ───────────────────────────────────────────────
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                s.back_len > 0,
                egui::Button::new(RichText::new("←").size(15.0)).fill(t.card),
            )
            .on_hover_text("后退（Backspace）")
            .clicked()
        {
            actions.push(Action::Back);
        }
        if ui
            .add_enabled(
                s.fwd_len > 0,
                egui::Button::new(RichText::new("→").size(15.0)).fill(t.card),
            )
            .clicked()
        {
            actions.push(Action::Forward);
        }
        let at_root = s.path == "/";
        if ui
            .add_enabled(
                !at_root,
                egui::Button::new(RichText::new("⬆").size(14.0)).fill(t.card),
            )
            .on_hover_text("上级目录")
            .clicked()
        {
            actions.push(Action::Navigate(parent_path(&s.path)));
        }
        if ui
            .add(egui::Button::new(RichText::new("⟳").size(14.0)).fill(t.card))
            .on_hover_text("刷新（F5）")
            .clicked()
        {
            actions.push(Action::Refresh);
        }
        ui.add_space(4.0);

        // 面包屑
        let crumbs = breadcrumb(&s.path);
        egui::ScrollArea::horizontal()
            .id_salt("crumbs")
            .auto_shrink([false, true])
            .max_height(26.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (i, (name, path)) in crumbs.iter().enumerate() {
                        if i > 0 {
                            ui.label(RichText::new("›").size(12.0).color(t.text_weak));
                        }
                        let is_last = i + 1 == crumbs.len();
                        let text = if is_last {
                            RichText::new(name).size(13.0).strong().color(t.text)
                        } else {
                            RichText::new(name).size(13.0).color(t.accent)
                        };
                        if ui
                            .add(egui::Button::new(text).fill(Color32::TRANSPARENT))
                            .clicked()
                            && !is_last
                        {
                            actions.push(Action::Navigate(path.clone()));
                        }
                    }
                });
            });

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // 排序
            let mut sort = (s.sort, s.desc);
            egui::ComboBox::from_id_salt("sort_combo")
                .selected_text(sort_label(sort.0, sort.1))
                .width(118.0)
                .show_ui(ui, |ui| {
                    for (by, desc) in [
                        (SortBy::Name, false),
                        (SortBy::Name, true),
                        (SortBy::Size, false),
                        (SortBy::Size, true),
                        (SortBy::Modified, false),
                        (SortBy::Modified, true),
                    ] {
                        if ui
                            .selectable_label(sort == (by, desc), sort_label(by, desc))
                            .clicked()
                        {
                            sort = (by, desc);
                        }
                    }
                });
            if sort != (s.sort, s.desc) {
                actions.push(Action::Sort(sort.0, sort.1));
            }
            // 本地筛选
            let mut filter = s.filter.clone();
            ui.add(
                egui::TextEdit::singleline(&mut filter)
                    .desired_width(170.0)
                    .hint_text("在当前目录筛选…"),
            );
            if filter != s.filter {
                actions.push(Action::Filter(filter));
            }
        });
    });
    ui.add_space(6.0);

    // ── 操作工具栏 ───────────────────────────────────────────
    let selected_paths: Vec<String> = s.selected.iter().cloned().collect();
    let selected_entries: Vec<RemoteEntry> = s
        .entries
        .iter()
        .filter(|e| s.selected.contains(&e.path))
        .cloned()
        .collect();
    let has_selection = !selected_paths.is_empty();
    let sel_has_file = selected_entries.iter().any(|e| !e.is_dir);
    let single = selected_paths.len() == 1;
    ui.horizontal(|ui| {
        if widgets::primary_button(ui, &t, "⬆ 上传").clicked() {
            actions.push(Action::OpenUploadPicker);
        }
        if widgets::tool_button(ui, &t, "＋ 新建文件夹", caps.mkdir).clicked() {
            actions.push(Action::NewFolder);
        }
        if widgets::tool_button(ui, &t, "⬇ 下载", caps.download && sel_has_file).clicked() {
            actions.push(Action::Download(selected_paths.clone()));
        }
        if widgets::tool_button(ui, &t, "重命名", caps.rename && single).clicked() {
            actions.push(Action::Rename(selected_paths[0].clone()));
        }
        if widgets::tool_button(ui, &t, "删除", has_selection).clicked() {
            actions.push(Action::Delete(selected_paths.clone()));
        }
        ui.menu_button(RichText::new("⋯ 更多").size(13.0).color(t.text), |ui| {
            if ui.button("📋 复制播放直链").clicked() {
                if let Some(p) = selected_paths.first() {
                    actions.push(Action::DirectLink(p.clone()));
                }
                ui.close();
            }
            if ui.button("🔄 跨网盘传输…").clicked() {
                if !selected_paths.is_empty() {
                    actions.push(Action::CrossCopy(selected_paths.clone()));
                }
                ui.close();
            }
            if ui.button("🔗 分享…").clicked() {
                if !selected_paths.is_empty() {
                    actions.push(Action::Share(selected_paths.clone()));
                }
                ui.close();
            }
            if ui.button("📦 移动到…").clicked() {
                if !selected_paths.is_empty() {
                    actions.push(Action::MoveTo(selected_paths.clone()));
                }
                ui.close();
            }
            if ui.button("📄 复制到…").clicked() {
                if !selected_paths.is_empty() {
                    actions.push(Action::CopyTo(selected_paths.clone()));
                }
                ui.close();
            }
            if ui.button("ℹ 详情").clicked() {
                if selected_entries.len() == 1 {
                    actions.push(Action::Details(selected_entries[0].clone()));
                }
                ui.close();
            }
            ui.separator();
            let tools_items: &[(&str, ToolsTab)] = &[
                ("🗑 回收站", ToolsTab::Recycle),
                ("☁ 云任务 / 云添加", ToolsTab::Cloud),
                ("🕘 上传记录", ToolsTab::Records),
                ("🔗 分享管理 / 转存", ToolsTab::Shares),
                ("🗂 分类浏览", ToolsTab::Categories),
            ];
            for (label, tab) in tools_items {
                if ui.button(*label).clicked() {
                    actions.push(Action::OpenTools(*tab));
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("全选").clicked() {
                actions.push(Action::SelectAll);
                ui.close();
            }
            if ui.button("取消选择").clicked() {
                actions.push(Action::ClearSelection);
                ui.close();
            }
        });

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(acc) = app.slot_by_id(&drive_id).and_then(|sl| sl.account.as_ref()) {
                if let Some(q) = acc.quota {
                    ui.label(RichText::new(q.summary()).size(11.0).color(t.text_weak));
                }
                let name = if acc.vip.is_empty() {
                    acc.display_name.clone()
                } else {
                    format!("{} · {}", acc.display_name, acc.vip)
                };
                ui.label(
                    RichText::new(crate::widgets::ellipsis(&name, 24))
                        .size(12.0)
                        .color(t.text_weak),
                );
            }
            let (status_text, color) = match &status {
                AccountStatus::LoggedIn => ("🟢 已登录", t.success),
                AccountStatus::LoggedOut => ("🟡 未登录", t.warning),
                AccountStatus::Checking => ("⏳ 检查中", t.warning),
                AccountStatus::Failed(e) => (e.as_str(), t.error),
                AccountStatus::Unknown => ("⚪ 未检查", t.text_weak),
            };
            ui.label(
                RichText::new(crate::widgets::ellipsis(status_text, 50))
                    .size(12.0)
                    .color(color),
            );
        });
    });
    ui.add_space(4.0);

    // ── 文件表格 ─────────────────────────────────────────────
    if s.loading && s.entries.is_empty() {
        widgets::loading(ui, &t, "正在列目录…");
    } else if let Some(err) = &s.error {
        let mut msg = err.clone();
        widgets::error_banner(ui, &t, &crate::widgets::ellipsis(&mut msg, 300));
        ui.add_space(6.0);
        if widgets::secondary_button(ui, &t, "重试").clicked() {
            actions.push(Action::Refresh);
        }
    } else {
        file_table(app, ui, &t, &s, &drive_id, &mut actions);
    }

    // 应用动作
    for a in actions {
        apply(app, a);
    }
}

fn sort_label(by: SortBy, desc: bool) -> &'static str {
    match (by, desc) {
        (SortBy::Name, false) => "名称 ↑",
        (SortBy::Name, true) => "名称 ↓",
        (SortBy::Size, false) => "大小 ↑",
        (SortBy::Size, true) => "大小 ↓",
        (SortBy::Modified, false) => "修改时间 ↑",
        (SortBy::Modified, true) => "修改时间 ↓",
        (SortBy::Kind, _) => "类型",
    }
}

fn file_table(
    app: &App,
    ui: &mut Ui,
    t: &Theme,
    s: &Snap,
    drive_id: &str,
    actions: &mut Vec<Action>,
) {
    let visible = s.visible();
    let total_w = ui.available_width();
    let size_w = 92.0;
    let time_w = 138.0;
    let check_w = 30.0;
    let name_w = (total_w - size_w - time_w - check_w - 8.0).max(120.0);

    // 表头
    let header_h = theme::size::HEADER_H;
    let (hrect, _) = ui.allocate_exact_size(Vec2::new(total_w, header_h), Sense::hover());
    ui.painter().rect_filled(hrect, 0, t.header_bg);
    let font_small = egui::FontId::proportional(12.5);
    let cy = hrect.center().y;
    let mut x = hrect.left() + 8.0;
    ui.painter().text(
        egui::pos2(x + check_w / 2.0 - 4.0, cy),
        egui::Align2::CENTER_CENTER,
        "☑",
        font_small.clone(),
        t.text_weak,
    );
    x += check_w;
    ui.painter().text(
        egui::pos2(x, cy),
        egui::Align2::LEFT_CENTER,
        "名称",
        font_small.clone(),
        t.text_weak,
    );
    ui.painter().text(
        egui::pos2(x + name_w - 4.0, cy),
        egui::Align2::RIGHT_CENTER,
        "大小",
        font_small.clone(),
        t.text_weak,
    );
    ui.painter().text(
        egui::pos2(x + name_w + size_w, cy),
        egui::Align2::LEFT_CENTER,
        "修改时间",
        font_small.clone(),
        t.text_weak,
    );

    let row_h = theme::size::ROW_H;
    egui::ScrollArea::vertical()
        .id_salt("file_list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if visible.is_empty() {
                ui.add_space(30.0);
                let (title, hint) = if s.entries.is_empty() {
                    ("这个目录是空的", "点上方「⬆ 上传」或「🆕 新建文件夹」开始")
                } else {
                    ("没有匹配筛选条件的条目", "清空右上角筛选框试试")
                };
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(title).size(14.0).color(t.text_weak));
                    ui.add_space(4.0);
                    ui.label(RichText::new(hint).size(12.0).color(t.text_weak));
                });
                return;
            }
            for (row_no, &idx) in visible.iter().enumerate() {
                let Some(e) = s.entries.get(idx) else {
                    continue;
                };
                let is_sel = s.selected.contains(&e.path);
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::click());
                let hovered = response.hovered();
                if is_sel {
                    ui.painter().rect_filled(
                        rect,
                        2,
                        Color32::from_rgba_unmultiplied(
                            t.selection.r(),
                            t.selection.g(),
                            t.selection.b(),
                            110,
                        ),
                    );
                } else if hovered {
                    ui.painter().rect_filled(rect, 2, t.hover);
                } else if row_no % 2 == 1 {
                    ui.painter().rect_filled(
                        rect,
                        0,
                        Color32::from_rgba_unmultiplied(128, 128, 128, 8),
                    );
                }

                let mut x = rect.left() + 8.0;
                // 复选框
                let cb = egui::Rect::from_center_size(
                    egui::pos2(x + check_w / 2.0 - 4.0, rect.center().y),
                    Vec2::splat(15.0),
                );
                ui.painter().rect_stroke(
                    cb,
                    theme::radius(3),
                    Stroke::new(1.0, if is_sel { t.accent } else { t.border }),
                    egui::StrokeKind::Inside,
                );
                if is_sel {
                    ui.painter().rect_filled(cb.shrink(3.5), 1, t.accent);
                }
                x += check_w;

                // 图标 / 缩略图
                let thumb_key = crate::thumbs::ThumbCache::key(&s.drive_id, e);
                let thumb_tex = app.thumbs.texture(&thumb_key);
                if let Some(tex) = thumb_tex {
                    let trect = egui::Rect::from_center_size(
                        egui::pos2(x + 9.0, rect.center().y),
                        Vec2::splat(18.0),
                    );
                    ui.painter().image(
                        tex.id(),
                        trect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                } else {
                    let icon = widgets::file_icon(e.is_dir, &e.name);
                    ui.painter().text(
                        egui::pos2(x, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        icon,
                        egui::FontId::proportional(14.0),
                        t.text,
                    );
                    // 视口内且未缓存 → 请求缩略图（每帧限并发，由缓存控制）
                    if crate::thumbs::ThumbCache::is_candidate(e)
                        && !app.thumbs.has(&thumb_key)
                        && rect.intersects(ui.clip_rect())
                    {
                        actions.push(Action::LoadThumb(e.clone()));
                    }
                }
                let max_chars = ((name_w - 24.0) / 7.6).max(4.0) as usize;
                ui.painter().text(
                    egui::pos2(x + 22.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    widgets::ellipsis(&e.name, max_chars),
                    egui::FontId::proportional(13.5),
                    if e.is_dir { t.accent } else { t.text },
                );
                // 大小
                ui.painter().text(
                    egui::pos2(x + name_w - 4.0, rect.center().y),
                    egui::Align2::RIGHT_CENTER,
                    e.size_text(),
                    egui::FontId::proportional(12.5),
                    t.text_weak,
                );
                // 修改时间
                ui.painter().text(
                    egui::pos2(x + name_w + size_w, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    e.modified_text(),
                    egui::FontId::proportional(12.5),
                    t.text_weak,
                );

                // 交互
                if response.clicked() {
                    let pos = response.interact_pointer_pos();
                    let in_checkbox = pos
                        .map(|p| p.x < rect.left() + 8.0 + check_w)
                        .unwrap_or(false);
                    if in_checkbox {
                        actions.push(Action::ToggleSelect(e.path.clone()));
                    } else {
                        let toggle = ui.input(|i| i.modifiers.ctrl || i.modifiers.command);
                        actions.push(Action::SelectEntry(e.path.clone(), toggle));
                    }
                }
                if response.double_clicked() {
                    actions.push(Action::Open(e.clone()));
                }
                let mut ctx_action: Option<Action> = None;
                response.context_menu(|ui| {
                    ctx_action = context_menu(ui, e);
                });
                if let Some(a) = ctx_action {
                    actions.push(a);
                }
                response.on_hover_text(&e.path);
            }
        });
    let _ = (drive_id, app);
}

fn context_menu(ui: &mut Ui, e: &RemoteEntry) -> Option<Action> {
    let mut action = None;
    if e.is_dir {
        if ui.button("📂 打开").clicked() {
            action = Some(Action::Open(e.clone()));
            ui.close();
        }
    } else {
        // 播放器入口：视频/音频文件右键可直接播放（T5/T15）
        if crate::pages::player::is_media_file(&e.name) && ui.button("▶ 播放").clicked() {
            action = Some(Action::Play(e.clone()));
            ui.close();
        }
        if ui.button("⬇ 下载").clicked() {
            action = Some(Action::Download(vec![e.path.clone()]));
            ui.close();
        }
        if ui.button("📋 复制直链").clicked() {
            action = Some(Action::DirectLink(e.path.clone()));
            ui.close();
        }
    }
    if ui.button("重命名").clicked() {
        action = Some(Action::Rename(e.path.clone()));
        ui.close();
    }
    if ui.button("删除").clicked() {
        action = Some(Action::Delete(vec![e.path.clone()]));
        ui.close();
    }
    if ui.button("🔗 分享").clicked() {
        action = Some(Action::Share(vec![e.path.clone()]));
        ui.close();
    }
    if ui.button("📦 移动到…").clicked() {
        action = Some(Action::MoveTo(vec![e.path.clone()]));
        ui.close();
    }
    if ui.button("📄 复制到…").clicked() {
        action = Some(Action::CopyTo(vec![e.path.clone()]));
        ui.close();
    }
    if ui.button("🔄 跨网盘传输").clicked() {
        action = Some(Action::CrossCopy(vec![e.path.clone()]));
        ui.close();
    }
    if ui.button("ℹ 详情").clicked() {
        action = Some(Action::Details(e.clone()));
        ui.close();
    }
    action
}

/// 浏览页动作。
pub enum Action {
    Navigate(String),
    Back,
    Forward,
    Refresh,
    Filter(String),
    Sort(SortBy, bool),
    NewFolder,
    OpenUploadPicker,
    Download(Vec<String>),
    Delete(Vec<String>),
    Rename(String),
    Share(Vec<String>),
    MoveTo(Vec<String>),
    CopyTo(Vec<String>),
    DirectLink(String),
    Details(RemoteEntry),
    CrossCopy(Vec<String>),
    SelectAll,
    ClearSelection,
    SelectEntry(String, bool),
    ToggleSelect(String),
    Open(RemoteEntry),
    /// 用播放器打开该媒体文件（右键「▶ 播放」）
    Play(RemoteEntry),
    OpenTools(ToolsTab),
    LoadThumb(RemoteEntry),
}

fn apply(app: &mut App, action: Action) {
    let drive_id = app.active_drive_id();
    match action {
        Action::Navigate(p) => app.navigate(&p),
        Action::Back => app.go_back(),
        Action::Forward => app.go_forward(),
        Action::Refresh => {
            let path = app
                .browse_ref(&drive_id)
                .map(|s| s.path.clone())
                .unwrap_or_else(|| "/".into());
            app.load_dir_for(&drive_id, &path);
        }
        Action::Filter(f) => app.browse_mut(&drive_id).filter = f,
        Action::Sort(by, desc) => {
            let st = app.browse_mut(&drive_id);
            st.sort = by;
            st.desc = desc;
        }
        Action::NewFolder => {
            let dir = app
                .browse_ref(&drive_id)
                .map(|s| s.path.clone())
                .unwrap_or_else(|| "/".into());
            app.dialog = Some(Dialog::Input(crate::app::InputDialog {
                title: "新建文件夹".into(),
                label: "文件夹名称".into(),
                value: "新建文件夹".into(),
                on_ok: crate::app::InputAction::NewFolder {
                    drive: drive_id,
                    dir,
                },
            }));
        }
        Action::OpenUploadPicker => app.open_file_picker(false),
        Action::Download(paths) => {
            let entries: Vec<RemoteEntry> = app
                .browse_ref(&drive_id)
                .map(|s| {
                    s.entries
                        .iter()
                        .filter(|e| paths.iter().any(|p| p == &e.path))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            app.enqueue_download_entries(&entries);
        }
        Action::Delete(paths) => {
            if !paths.is_empty() {
                app.dialog = Some(Dialog::Confirm(crate::app::ConfirmDialog {
                    title: "删除确认".into(),
                    message: format!(
                        "确认删除选中的 {} 项？\n{}\n\n（目录会连同其中内容一起删除）",
                        paths.len(),
                        paths.iter().take(8).cloned().collect::<Vec<_>>().join("\n")
                    ),
                    danger: true,
                    action: crate::app::ConfirmAction::Delete {
                        drive: drive_id,
                        paths,
                    },
                }));
            }
        }
        Action::Rename(path) => {
            let name = base_name(&path);
            app.dialog = Some(Dialog::Input(crate::app::InputDialog {
                title: "重命名".into(),
                label: "新名称".into(),
                value: name,
                on_ok: crate::app::InputAction::Rename {
                    drive: drive_id,
                    path,
                },
            }));
        }
        Action::Share(paths) => {
            app.dialog = Some(Dialog::Share(crate::app::ShareDialog {
                drive: drive_id,
                paths,
                expire_days: 7,
                password: String::new(),
                busy: false,
                result: None,
                error: String::new(),
            }));
        }
        Action::MoveTo(paths) => {
            if !paths.is_empty() {
                app.open_dir_picker(DirPickerAction::Move { paths });
            }
        }
        Action::CopyTo(paths) => {
            if !paths.is_empty() {
                app.open_dir_picker(DirPickerAction::Copy { paths });
            }
        }
        Action::DirectLink(path) => {
            let Some(slot) = app.slot_by_id(&drive_id) else {
                return;
            };
            let Some(drive) = slot.drive.clone() else {
                return;
            };
            app.spawn(async move {
                crate::app::Event::DirectLink {
                    path: path.clone(),
                    result: drive.direct_link(&path).await,
                }
            });
        }
        Action::Details(e) => app.dialog = Some(Dialog::Details(DetailsDialog { entry: e })),
        Action::CrossCopy(paths) => {
            let dst = app
                .drives
                .iter()
                .find(|s| s.id() != drive_id && s.drive.is_some())
                .map(|s| s.id().to_string())
                .unwrap_or_default();
            if dst.is_empty() {
                app.toast(ToastKind::Warn, "只有一个可用网盘，无法跨盘传输");
                return;
            }
            app.dialog = Some(Dialog::CrossCopy(crate::app::CrossCopyDialog {
                src_drive: drive_id,
                paths,
                dst_drive: dst,
                dst_dir: "/".into(),
            }));
        }
        Action::SelectAll => {
            let st = app.browse_mut(&drive_id);
            let paths: Vec<String> = st.entries.iter().map(|e| e.path.clone()).collect();
            for p in paths {
                st.selected.insert(p);
            }
        }
        Action::ClearSelection => app.browse_mut(&drive_id).selected.clear(),
        Action::SelectEntry(path, toggle) => {
            let st = app.browse_mut(&drive_id);
            if toggle {
                if st.selected.contains(&path) {
                    st.selected.remove(&path);
                } else {
                    st.selected.insert(path);
                }
            } else {
                st.selected.clear();
                st.selected.insert(path);
            }
        }
        Action::ToggleSelect(path) => {
            let st = app.browse_mut(&drive_id);
            if st.selected.contains(&path) {
                st.selected.remove(&path);
            } else {
                st.selected.insert(path);
            }
        }
        Action::Open(e) => app.open_entry(&e),
        Action::Play(e) => app.open_player_remote_entry(&e),
        Action::LoadThumb(e) => app.request_thumbnail(&e),
        Action::OpenTools(tab) => {
            app.page = crate::app::Page::Tools;
            app.tools.tab = Some(tab);
            app.tools.loaded_key.clear();
        }
    }
}
