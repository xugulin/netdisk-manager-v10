//! 全部模态对话框：登录、输入、确认、云端目录选择、分享、详情、跨盘传输。
//!
//! 模式：从 `app.dialog` 取出对话框值 → 渲染并收集动作（`Modal`）→ 应用动作 →
//! 需要继续显示时再写回 `app.dialog`。这样不会在 UI 闭包里与 `&mut App` 冲突。

use crate::app::{
    AddDriveDialog, App, ConfirmAction, CrossCopyDialog, DetailsDialog, Dialog, DirPicker,
    DirPickerAction, InputAction, InputDialog, LoginDialog, LoginTab, ShareDialog, ToastKind,
};
use crate::theme::Theme;
use crate::widgets;
use egui::{RichText, Ui};
use wp_core::model::{DriveKind, QrState, TaskState};

pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(dialog) = app.dialog.take() else {
        return;
    };
    let keep: Option<Dialog> = match dialog {
        Dialog::Login(mut d) => login(app, ctx, &mut d).then(|| Dialog::Login(d)),
        Dialog::AddDrive(mut d) => add_drive(app, ctx, &mut d).then(|| Dialog::AddDrive(d)),
        Dialog::Input(mut d) => input(app, ctx, &mut d).then(|| Dialog::Input(d)),
        Dialog::Confirm(d) => confirm(app, ctx, d),
        Dialog::DirPicker(mut d) => dir_picker(app, ctx, &mut d).then(|| Dialog::DirPicker(d)),
        Dialog::Share(mut d) => share(app, ctx, &mut d).then(|| Dialog::Share(d)),
        Dialog::Details(d) => details(app, ctx, d),
        Dialog::CrossCopy(mut d) => cross_copy(app, ctx, &mut d).then(|| Dialog::CrossCopy(d)),
    };
    if let Some(d) = keep {
        app.dialog = Some(d);
    }
}

fn title_row(ui: &mut Ui, t: &Theme, title: &str, close: &mut bool) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(17.0).strong().color(t.text));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("✕").clicked() {
                *close = true;
            }
        });
    });
    ui.separator();
}

// ═══════════════════════════ 添加网盘 / 本机 ═══════════════════════════

fn drive_kind_desc(kind: DriveKind) -> &'static str {
    match kind {
        DriveKind::Baidu => "扫码 / Cookie 登录；同一家可加多个账号实例（凭证隔离）",
        DriveKind::Quark => "扫码登录；可加多个账号实例",
        DriveKind::Guangya => "扫码 / 短信 / 令牌；可加多个账号实例",
        DriveKind::Local => "浏览与传输本机文件；单实例（已有则直接切换）",
        DriveKind::Fake => "内存演示数据、无需登录；用于界面与传输演练",
    }
}

fn add_drive(app: &mut App, ctx: &egui::Context, _d: &mut AddDriveDialog) -> bool {
    let t = app.theme;
    let mut close = false;
    let mut chosen: Option<DriveKind> = None;
    egui::Modal::new(crate::app::modal_id("add_drive_modal")).show(ctx, |ui| {
        ui.set_width(470.0);
        title_row(ui, &t, "添加网盘 / 本机", &mut close);
        widgets::hint(
            ui,
            &t,
            "先选类型再创建实例：云盘同一家可加多个账号（各自独立凭证）；本机/演示为单实例，已有则直接切换。",
        );
        ui.add_space(8.0);
        for kind in DriveKind::ALL {
            let existing = app.cfg.drives.iter().filter(|x| x.kind == kind).count();
            let brand = egui::Color32::from_rgb(
                kind.brand_color()[0],
                kind.brand_color()[1],
                kind.brand_color()[2],
            );
            ui.horizontal(|ui| {
                ui.label(RichText::new(kind.icon()).size(17.0).strong().color(brand));
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(kind.label()).size(13.5).color(t.text));
                    ui.label(RichText::new(drive_kind_desc(kind)).size(11.0).color(t.text_weak));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let single = matches!(kind, DriveKind::Local | DriveKind::Fake);
                    let btn_text = if single && existing > 0 { "切换" } else { "添加" };
                    if ui.button(btn_text).clicked() {
                        chosen = Some(kind);
                    }
                    if existing > 0 {
                        ui.label(
                            RichText::new(format!("已有 {existing} 个"))
                                .size(11.0)
                                .color(t.text_weak),
                        );
                    }
                });
            });
            ui.separator();
        }
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            if ui.button("取消").clicked() {
                close = true;
            }
            widgets::hint(ui, &t, "提示：删除某类全部实例后，仍可在这里重新添加。");
        });
    });
    if let Some(kind) = chosen {
        app.add_drive_from_dialog(kind);
        return false;
    }
    !close
}

// ═══════════════════════════ 登录 ═══════════════════════════

fn login(app: &mut App, ctx: &egui::Context, d: &mut LoginDialog) -> bool {
    let t = app.theme;
    let mut close = false;
    let mut outcome: Option<LoginOutcome> = None;
    let mut request_poll = false;
    let drive_name = app
        .slot_by_id(&d.drive)
        .map(|s| s.display().to_string())
        .unwrap_or_else(|| d.drive.clone());

    egui::Modal::new(crate::app::modal_id("login_modal")).show(ctx, |ui| {
        ui.set_width(430.0);
        title_row(ui, &t, &format!("登录 · {drive_name}"), &mut close);

        // 登录方式标签
        ui.horizontal(|ui| {
            let mut tab = d.tab;
            if d.caps.qr
                && ui
                    .selectable_label(tab == LoginTab::Qr, "📱 扫码")
                    .clicked()
            {
                tab = LoginTab::Qr;
            }
            if d.caps.cookie
                && ui
                    .selectable_label(tab == LoginTab::Cookie, "🍪 Cookie")
                    .clicked()
            {
                tab = LoginTab::Cookie;
            }
            if d.caps.sms
                && ui
                    .selectable_label(tab == LoginTab::Sms, "📨 短信")
                    .clicked()
            {
                tab = LoginTab::Sms;
            }
            if d.caps.token
                && ui
                    .selectable_label(tab == LoginTab::Token, "🔑 令牌")
                    .clicked()
            {
                tab = LoginTab::Token;
            }
            if tab != d.tab {
                d.tab = tab;
                d.error.clear();
                if tab == LoginTab::Qr && d.qr.is_none() {
                    outcome = Some(LoginOutcome::StartQr);
                }
            }
        });
        ui.add_space(10.0);

        match d.tab {
            LoginTab::Qr => {
                if d.qr.is_none() {
                    widgets::hint(ui, &t, "正在向网盘申请登录二维码…");
                    if widgets::secondary_button(ui, &t, "重新获取二维码").clicked() {
                        outcome = Some(LoginOutcome::StartQr);
                    }
                } else {
                    let expired = d.qr_state == QrState::Expired;
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            if let Some(tex) = &d.qr_texture {
                                let sized = egui::load::SizedTexture::from_handle(tex);
                                ui.add(
                                    egui::Image::new(sized)
                                        .fit_to_exact_size(egui::vec2(200.0, 200.0)),
                                );
                            } else {
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(200.0, 200.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().rect_filled(rect, 8, t.bg);
                                ui.painter().text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    d.qr.as_ref()
                                        .and_then(|q| q.url.clone())
                                        .unwrap_or_else(|| "二维码图片不可用".into()),
                                    egui::FontId::proportional(12.0),
                                    t.text_weak,
                                );
                            }
                            ui.add_space(6.0);
                            let state_color = match d.qr_state {
                                QrState::Waiting => t.accent,
                                QrState::Scanned => t.warning,
                                QrState::Expired => t.error,
                                QrState::Success => t.success,
                                QrState::Canceled => t.text_weak,
                            };
                            ui.label(
                                RichText::new(d.qr_state.label())
                                    .size(13.0)
                                    .color(state_color),
                            );
                        });
                        ui.add_space(12.0);
                        ui.vertical(|ui| {
                            ui.set_max_width(190.0);
                            if let Some(q) = &d.qr {
                                if !q.hint.is_empty() {
                                    ui.label(RichText::new(&q.hint).size(12.0).color(t.text));
                                }
                                if let Some(code) = &q.code {
                                    ui.label(
                                        RichText::new(format!("设备码：{code}"))
                                            .size(12.0)
                                            .color(t.accent),
                                    );
                                }
                                if let Some(url) = &q.url {
                                    ui.add_space(4.0);
                                    ui.label(
                                        RichText::new("若二维码不显示，可打开：")
                                            .size(11.0)
                                            .color(t.text_weak),
                                    );
                                    ui.label(
                                        RichText::new(crate::widgets::ellipsis(url, 60))
                                            .size(10.5)
                                            .color(t.accent),
                                    );
                                }
                                if q.expires_in_secs > 0 && d.qr_state != QrState::Expired {
                                    ui.add_space(4.0);
                                    ui.label(
                                        RichText::new(format!("有效期约 {} 秒", q.expires_in_secs))
                                            .size(11.0)
                                            .color(t.text_weak),
                                    );
                                }
                            }
                            if !d.qr_message.is_empty() {
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new(&d.qr_message).size(12.0).color(t.text_weak),
                                );
                            }
                            if expired && widgets::primary_button(ui, &t, "⟳ 重新获取").clicked()
                            {
                                outcome = Some(LoginOutcome::StartQr);
                            }
                        });
                    });
                    // 轮询
                    if matches!(d.qr_state, QrState::Waiting | QrState::Scanned)
                        && !d.poll_inflight
                        && d.qr_timer.elapsed() > std::time::Duration::from_millis(800)
                    {
                        d.poll_inflight = true;
                        d.qr_timer = std::time::Instant::now();
                        request_poll = true;
                    }
                }
            }
            LoginTab::Cookie => {
                widgets::hint(
                    ui,
                    &t,
                    "把浏览器里的 Cookie（或 BDUSS+STOKEN）粘贴到下面，程序会先校验再保存。",
                );
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::multiline(&mut d.cookie_text)
                        .desired_width(f32::INFINITY)
                        .desired_rows(6)
                        .hint_text("BDUSS=...; STOKEN=...  或完整 Cookie 串"),
                );
                ui.add_space(6.0);
                if widgets::primary_button(ui, &t, "导入并校验").clicked() {
                    outcome = Some(LoginOutcome::Cookie);
                }
            }
            LoginTab::Sms => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("手机号").size(12.5).color(t.text));
                    ui.add(
                        egui::TextEdit::singleline(&mut d.phone)
                            .desired_width(200.0)
                            .hint_text("含国家码，如 +8613800000000"),
                    );
                    if widgets::secondary_button(ui, &t, "发送验证码").clicked() {
                        outcome = Some(LoginOutcome::SmsSend);
                    }
                });
                ui.add_space(6.0);
                if !d.sms_hint.is_empty() {
                    widgets::success_banner(ui, &t, &d.sms_hint);
                    ui.add_space(4.0);
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new("验证码").size(12.5).color(t.text));
                    ui.add(
                        egui::TextEdit::singleline(&mut d.sms_code)
                            .desired_width(120.0)
                            .hint_text("6 位数字"),
                    );
                    if widgets::primary_button(ui, &t, "登录").clicked() {
                        outcome = Some(LoginOutcome::SmsVerify);
                    }
                });
                widgets::hint(
                    ui,
                    &t,
                    "提示：部分网盘的短信登录仅在其官方客户端支持，失败时请改用扫码/Cookie。",
                );
            }
            LoginTab::Token => {
                widgets::hint(
                    ui,
                    &t,
                    "粘贴 access token（光鸭云盘支持 access/refresh token）。",
                );
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::multiline(&mut d.token_text)
                        .desired_width(f32::INFINITY)
                        .desired_rows(4)
                        .hint_text("access_token"),
                );
                ui.add_space(6.0);
                if widgets::primary_button(ui, &t, "令牌登录").clicked() {
                    outcome = Some(LoginOutcome::Token);
                }
            }
        }

        if !d.error.is_empty() {
            ui.add_space(8.0);
            widgets::error_banner(ui, &t, &d.error);
        }
        if d.busy {
            ui.add_space(6.0);
            widgets::loading(ui, &t, "正在处理…");
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("关闭").clicked() {
                close = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                widgets::hint(ui, &t, "登录成功后会自动刷新目录");
            });
        });
    });

    // 轮询请求（异步）
    if request_poll {
        app.poll_qr_async(&d.drive);
    }
    // 动作
    if let Some(o) = outcome {
        match o {
            LoginOutcome::StartQr => {
                d.qr = None;
                d.qr_texture = None;
                d.error.clear();
                d.busy = true;
                app.begin_qr_async(&d.drive);
            }
            LoginOutcome::Cookie => {
                let text = d.cookie_text.trim().to_string();
                if text.is_empty() {
                    d.error = "请先粘贴 Cookie".into();
                } else {
                    d.busy = true;
                    d.error.clear();
                    app.login_cookie_async(&d.drive, text);
                }
            }
            LoginOutcome::SmsSend => {
                let phone = d.phone.trim().to_string();
                if phone.is_empty() {
                    d.error = "请先填写手机号".into();
                } else {
                    d.busy = true;
                    d.error.clear();
                    app.login_sms_send_async(&d.drive, phone);
                }
            }
            LoginOutcome::SmsVerify => {
                d.busy = true;
                d.error.clear();
                app.login_sms_verify_async(
                    &d.drive,
                    d.phone.trim().to_string(),
                    d.sms_code.trim().to_string(),
                );
            }
            LoginOutcome::Token => {
                let token = d.token_text.trim().to_string();
                if token.is_empty() {
                    d.error = "请先粘贴令牌".into();
                } else {
                    d.busy = true;
                    d.error.clear();
                    app.login_token_async(&d.drive, token);
                }
            }
        }
    }
    if close {
        return false;
    }
    true
}

enum LoginOutcome {
    StartQr,
    Cookie,
    SmsSend,
    SmsVerify,
    Token,
}

// ═══════════════════════════ 输入框 ═══════════════════════════

fn input(app: &mut App, ctx: &egui::Context, d: &mut InputDialog) -> bool {
    let t = app.theme;
    let mut keep = true;
    let mut submit = false;
    let mut close = false;
    egui::Modal::new(crate::app::modal_id("input_modal")).show(ctx, |ui| {
        ui.set_width(400.0);
        title_row(ui, &t, &d.title, &mut close);
        ui.add_space(6.0);
        ui.label(RichText::new(&d.label).size(12.5).color(t.text_weak));
        let resp = ui.add(
            egui::TextEdit::singleline(&mut d.value)
                .desired_width(f32::INFINITY)
                .hint_text("请输入名称"),
        );
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            submit = true;
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if widgets::primary_button(ui, &t, "确定").clicked() {
                submit = true;
            }
            if ui.button("取消").clicked() {
                close = true;
            }
        });
    });
    if close {
        return false;
    }
    if submit {
        let value = d.value.trim().to_string();
        if value.is_empty() {
            app.toast(ToastKind::Warn, "名称不能为空");
            return true;
        }
        match &d.on_ok {
            InputAction::NewFolder { drive, dir } => {
                let path = wp_core::util::join_path(dir, &value);
                let Some(slot) = app.slot_by_id(drive) else {
                    return false;
                };
                let Some(drv) = slot.drive.clone() else {
                    return false;
                };
                app.spawn(async move {
                    crate::app::Event::OpDone {
                        message: format!("新建文件夹 {path}"),
                        refresh: true,
                        result: drv.ensure_dir(&path).await.map(|e| e.path),
                    }
                });
            }
            InputAction::Rename { drive, path } => {
                let Some(slot) = app.slot_by_id(drive) else {
                    return false;
                };
                let Some(drv) = slot.drive.clone() else {
                    return false;
                };
                let p = path.clone();
                app.spawn(async move {
                    crate::app::Event::OpDone {
                        message: "重命名".into(),
                        refresh: true,
                        result: drv.rename(&p, &value).await.map(|e| e.name),
                    }
                });
            }
        }
        keep = false;
    }
    keep
}

// ═══════════════════════════ 确认 ═══════════════════════════

fn confirm(app: &mut App, ctx: &egui::Context, d: crate::app::ConfirmDialog) -> Option<Dialog> {
    let t = app.theme;
    let mut yes = false;
    let mut close = false;
    egui::Modal::new(crate::app::modal_id("confirm_modal")).show(ctx, |ui| {
        ui.set_width(440.0);
        title_row(ui, &t, &d.title, &mut close);
        ui.add_space(8.0);
        ui.label(RichText::new(&d.message).size(13.0).color(t.text));
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if d.danger {
                if widgets::danger_button(ui, &t, "确认删除").clicked() {
                    yes = true;
                }
            } else if widgets::primary_button(ui, &t, "确定").clicked() {
                yes = true;
            }
            if ui.button("取消").clicked() {
                close = true;
            }
        });
    });
    if close {
        return None;
    }
    if yes {
        match d.action {
            ConfirmAction::Delete { drive, paths } => {
                let Some(slot) = app.slot_by_id(&drive) else {
                    return None;
                };
                let Some(drv) = slot.drive.clone() else {
                    return None;
                };
                let n = paths.len();
                app.spawn(async move {
                    crate::app::Event::OpDone {
                        message: format!("删除 {n} 项"),
                        refresh: true,
                        result: drv.delete(&paths).await.map(|r| r.summary()),
                    }
                });
            }
            ConfirmAction::CancelTasks { ids } => {
                for id in ids {
                    app.transfer.cancel(&id);
                }
                app.transfer.save();
            }
        }
    }
    None
}

// ═══════════════════════════ 目录选择器（移动/复制） ═══════════════════════════

fn dir_picker(app: &mut App, ctx: &egui::Context, d: &mut DirPicker) -> bool {
    let t = app.theme;
    let mut close = false;
    let mut navigate: Option<String> = None;
    let mut commit = false;

    // 首次打开：加载目录
    if d.entries.is_empty() && !d.loading && d.error.is_empty() {
        d.loading = true;
        app.load_picker_dir(&d.drive, &d.path);
    }

    let (title, ok_text) = match &d.action {
        DirPickerAction::Move { .. } => ("移动到…", "移动到此目录"),
        DirPickerAction::Copy { .. } => ("复制到…", "复制到此目录"),
    };
    egui::Modal::new(crate::app::modal_id("dir_picker_modal")).show(ctx, |ui| {
        ui.set_width(540.0);
        title_row(ui, &t, title, &mut close);
        // 路径
        ui.horizontal(|ui| {
            if ui.button("⬆ 上级").clicked() {
                navigate = Some(wp_core::util::parent_path(&d.path));
            }
            let crumbs = wp_core::util::breadcrumb(&d.path);
            egui::ScrollArea::horizontal()
                .id_salt("picker_crumbs")
                .max_height(24.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for (i, (name, path)) in crumbs.iter().enumerate() {
                            if i > 0 {
                                ui.label(RichText::new("/").size(11.0).color(t.text_weak));
                            }
                            let is_last = i + 1 == crumbs.len();
                            let text = if is_last {
                                RichText::new(name).size(12.0).strong().color(t.text)
                            } else {
                                RichText::new(name).size(12.0).color(t.accent)
                            };
                            if ui
                                .add(egui::Button::new(text).fill(egui::Color32::TRANSPARENT))
                                .clicked()
                                && !is_last
                            {
                                navigate = Some(path.clone());
                            }
                        }
                    });
                });
        });
        ui.add_space(4.0);
        let mut list_action: Option<String> = None;
        egui::ScrollArea::vertical()
            .id_salt("picker_list")
            .max_height(260.0)
            .show(ui, |ui| {
                if d.loading {
                    widgets::loading(ui, &t, "加载中…");
                } else if !d.error.is_empty() {
                    widgets::error_banner(ui, &t, &d.error);
                } else if d.entries.is_empty() {
                    widgets::hint(ui, &t, "（没有子目录）");
                }
                for e in &d.entries {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(format!("📁 {}", e.name))
                                    .size(13.0)
                                    .color(t.text),
                            )
                            .fill(egui::Color32::TRANSPARENT),
                        )
                        .clicked()
                    {
                        list_action = Some(e.path.clone());
                    }
                }
            });
        if let Some(p) = list_action {
            navigate = Some(p);
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if widgets::primary_button(ui, &t, ok_text).clicked() {
                commit = true;
            }
            if ui.button("取消").clicked() {
                close = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                widgets::hint(ui, &t, &format!("目标目录：{}", d.path));
            });
        });
    });

    if let Some(p) = navigate {
        d.path = p;
        d.entries.clear();
        d.error.clear();
        d.loading = true;
        app.load_picker_dir(&d.drive, &d.path);
    }
    if close {
        return false;
    }
    if commit {
        let dest = d.path.clone();
        let (paths, is_move) = match &d.action {
            DirPickerAction::Move { paths } => (paths.clone(), true),
            DirPickerAction::Copy { paths } => (paths.clone(), false),
        };
        app.do_move_copy(&d.drive, &paths, &dest, is_move);
        return false;
    }
    true
}

// ═══════════════════════════ 分享 ═══════════════════════════

fn share(app: &mut App, ctx: &egui::Context, d: &mut ShareDialog) -> bool {
    let t = app.theme;
    let mut close = false;
    let mut create = false;
    egui::Modal::new(crate::app::modal_id("share_modal")).show(ctx, |ui| {
        ui.set_width(460.0);
        title_row(ui, &t, "分享", &mut close);
        ui.label(
            RichText::new(format!("已选 {} 项", d.paths.len()))
                .size(12.5)
                .color(t.text),
        );
        for p in d.paths.iter().take(5) {
            ui.label(
                RichText::new(format!("· {p}"))
                    .size(11.5)
                    .color(t.text_weak),
            );
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("有效期").size(12.5).color(t.text));
            ui.add(
                egui::DragValue::new(&mut d.expire_days)
                    .range(0..=365)
                    .suffix(" 天"),
            );
            widgets::hint(ui, &t, "0 = 永久/默认");
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("提取码").size(12.5).color(t.text));
            ui.add(
                egui::TextEdit::singleline(&mut d.password)
                    .desired_width(120.0)
                    .hint_text("留空自动生成"),
            );
        });
        ui.add_space(8.0);
        if d.result.is_none() {
            if widgets::primary_button(ui, &t, "创建分享").clicked() {
                create = true;
            }
        }
        if let Some(info) = &d.result {
            ui.separator();
            widgets::success_banner(ui, &t, "分享创建成功");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("链接").size(12.0).color(t.text_weak));
                ui.label(RichText::new(&info.url).size(12.5).color(t.accent));
                if ui.small_button("复制").clicked() {
                    if let Some(c) = &app_ref_ctx(ui) {
                        c.copy_text(info.url.clone());
                        let _ = c;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("提取码").size(12.0).color(t.text_weak));
                ui.label(
                    RichText::new(&info.password)
                        .size(13.0)
                        .strong()
                        .color(t.text),
                );
            });
            if !info.share_id.is_empty() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("分享 ID").size(12.0).color(t.text_weak));
                    ui.label(
                        RichText::new(&info.share_id)
                            .size(12.0)
                            .monospace()
                            .color(t.text),
                    );
                });
            }
            ui.add_space(4.0);
            if ui.button("复制链接+提取码").clicked() {
                let text = format!("链接：{}\n提取码：{}", info.url, info.password);
                if let Some(c) = app_ref_ctx(ui) {
                    c.copy_text(text);
                }
            }
        }
        if !d.error.is_empty() {
            ui.add_space(6.0);
            widgets::error_banner(ui, &t, &d.error);
        }
        if d.busy {
            widgets::loading(ui, &t, "正在创建分享…");
        }
        ui.add_space(8.0);
        if ui.button("关闭").clicked() {
            close = true;
        }
    });
    if close {
        return false;
    }
    if create {
        d.busy = true;
        d.error.clear();
        let Some(slot) = app.slot_by_id(&d.drive) else {
            return false;
        };
        let Some(drv) = slot.drive.clone() else {
            return false;
        };
        let paths = d.paths.clone();
        let opts = wp_drive::ShareOptions {
            expire_days: d.expire_days,
            password: if d.password.trim().is_empty() {
                None
            } else {
                Some(d.password.trim().to_string())
            },
            title: String::new(),
        };
        app.spawn(
            async move { crate::app::Event::ShareCreated(drv.create_share(&paths, &opts).await) },
        );
    }
    true
}

fn app_ref_ctx(ui: &Ui) -> Option<egui::Context> {
    Some(ui.ctx().clone())
}

// ═══════════════════════════ 详情 ═══════════════════════════

fn details(app: &mut App, ctx: &egui::Context, d: DetailsDialog) -> Option<Dialog> {
    let t = app.theme;
    let mut close = false;
    egui::Modal::new(crate::app::modal_id("details_modal")).show(ctx, |ui| {
        ui.set_width(520.0);
        title_row(ui, &t, "文件详情", &mut close);
        ui.add_space(4.0);
        // 缩略图预览（图片/视频等有 thumbnail 的条目）
        if crate::thumbs::ThumbCache::is_candidate(&d.entry) {
            let drive_id = app.active_drive_id();
            let key = crate::thumbs::ThumbCache::key(&drive_id, &d.entry);
            if !app.thumbs.has(&key) {
                app.request_thumbnail(&d.entry);
            }
            match app.thumbs.texture(&key).cloned() {
                Some(tex) => {
                    let size = tex.size_vec2();
                    let scale = (240.0 / size.x).min(170.0 / size.y).min(1.0);
                    let target = size * scale;
                    ui.add(egui::Image::new(egui::load::SizedTexture::new(
                        tex.id(),
                        target,
                    )));
                    ui.add_space(6.0);
                }
                None => {
                    if !app.thumbs.has(&key) {
                        ui.label(RichText::new("缩略图加载中…").size(12.0).color(t.text_weak));
                        ui.add_space(2.0);
                    }
                }
            }
        }
        widgets::kv_row(ui, &t, "名称", &d.entry.name);
        widgets::kv_row(ui, &t, "路径", &d.entry.path);
        widgets::kv_row(ui, &t, "类型", if d.entry.is_dir { "目录" } else { "文件" });
        widgets::kv_row(
            ui,
            &t,
            "大小",
            &if d.entry.is_dir {
                "—".to_string()
            } else {
                format!(
                    "{}（{} 字节）",
                    wp_core::util::format_size(d.entry.size),
                    d.entry.size
                )
            },
        );
        widgets::kv_row(ui, &t, "修改时间", &d.entry.modified_text());
        if !d.entry.id.is_empty() {
            widgets::kv_row(ui, &t, "远端 ID", &d.entry.id);
        }
        if let Some(raw) = &d.entry.raw {
            ui.add_space(4.0);
            ui.label(RichText::new("原始响应").size(12.0).color(t.text_weak));
            egui::ScrollArea::vertical()
                .max_height(180.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(
                            &mut serde_json::to_string_pretty(raw).unwrap_or_default(),
                        )
                        .desired_width(f32::INFINITY)
                        .interactive(false),
                    );
                });
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if !d.entry.is_dir && ui.button("📋 复制路径").clicked() {
                if let Some(c) = app_ref_ctx(ui) {
                    c.copy_text(d.entry.path.clone());
                }
            }
            if ui.button("关闭").clicked() {
                close = true;
            }
        });
    });
    // 修复：之前恒返回 None，导致详情弹窗只显示一帧就被关闭
    if close {
        None
    } else {
        Some(Dialog::Details(d))
    }
}

// ═══════════════════════════ 跨盘传输 ═══════════════════════════

fn cross_copy(app: &mut App, ctx: &egui::Context, d: &mut CrossCopyDialog) -> bool {
    let t = app.theme;
    let mut close = false;
    let mut start = false;
    let targets: Vec<(String, String)> = app
        .drives
        .iter()
        .filter(|s| s.id() != d.src_drive && s.drive.is_some())
        .map(|s| (s.id().to_string(), s.display().to_string()))
        .collect();
    egui::Modal::new(crate::app::modal_id("cross_modal")).show(ctx, |ui| {
        ui.set_width(460.0);
        title_row(ui, &t, "跨网盘传输", &mut close);
        let src_name = app
            .slot_by_id(&d.src_drive)
            .map(|s| s.display().to_string())
            .unwrap_or_else(|| d.src_drive.clone());
        widgets::kv_row(ui, &t, "源网盘", &src_name);
        widgets::kv_row(ui, &t, "条目数", &d.paths.len().to_string());
        for p in d.paths.iter().take(4) {
            ui.label(
                RichText::new(format!("· {p}"))
                    .size(11.5)
                    .color(t.text_weak),
            );
        }
        if d.paths.len() > 4 {
            widgets::hint(ui, &t, &format!("…等 {} 项", d.paths.len()));
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("目标网盘").size(12.5).color(t.text));
            egui::ComboBox::from_id_salt("cross_target")
                .selected_text(
                    targets
                        .iter()
                        .find(|(id, _)| id == &d.dst_drive)
                        .map(|(_, n)| n.clone())
                        .unwrap_or_else(|| "（无可用目标）".into()),
                )
                .width(180.0)
                .show_ui(ui, |ui| {
                    for (id, name) in &targets {
                        if ui.selectable_label(&d.dst_drive == id, name).clicked() {
                            d.dst_drive = id.clone();
                        }
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("目标目录").size(12.5).color(t.text));
            ui.add(
                egui::TextEdit::singleline(&mut d.dst_dir)
                    .desired_width(240.0)
                    .hint_text("/"),
            );
        });
        widgets::hint(
            ui,
            &t,
            "流程：源盘下载到本地缓存 → 上传到目标盘（不经第三方中转服务）。",
        );
        ui.add_space(10.0);
        let can = !targets.is_empty();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    can,
                    egui::Button::new(
                        RichText::new("开始传输").color(egui::Color32::from_rgb(20, 20, 20)),
                    )
                    .fill(t.accent),
                )
                .clicked()
            {
                start = true;
            }
            if ui.button("取消").clicked() {
                close = true;
            }
        });
    });
    if close {
        return false;
    }
    if start {
        app.enqueue_cross_copy(&d.paths, &d.dst_drive, &d.dst_dir);
        return false;
    }
    let _ = TaskState::Queued;
    true
}

/// 在对话框里选择目录（由 DirPicker 打开时调用）。
impl App {
    /// 给 DirPicker 加载目录（页面里直接调用）。
    pub fn open_dir_picker(&mut self, action: DirPickerAction) {
        let drive = self.active_drive_id();
        self.dialog = Some(Dialog::DirPicker(DirPicker {
            drive: drive.clone(),
            path: self
                .browse_ref(&drive)
                .map(|s| s.path.clone())
                .unwrap_or_else(|| "/".into()),
            entries: Vec::new(),
            loading: false,
            error: String::new(),
            action,
        }));
    }
}
