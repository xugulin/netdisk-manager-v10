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
        Dialog::AccountDetails(d) => account_details(app, ctx, d),
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
    let kind = app
        .slot_by_id(&d.drive)
        .map(|s| s.cfg.kind)
        .unwrap_or(DriveKind::Baidu);

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
                                        RichText::new("若二维码不显示，可复制链接/打开登录页：")
                                            .size(11.0)
                                            .color(t.text_weak),
                                    );
                                    ui.label(
                                        RichText::new(crate::widgets::ellipsis(url, 60))
                                            .size(10.5)
                                            .color(t.accent),
                                    );
                                }
                                ui.add_space(4.0);
                                ui.horizontal_wrapped(|ui| {
                                    if let Some(url) = &q.url {
                                        if ui.small_button("📋 复制链接").clicked() {
                                            ui.ctx().copy_text(url.clone());
                                        }
                                    }
                                    if let Some(code) = &q.code {
                                        if ui.small_button("📋 复制设备码").clicked() {
                                            ui.ctx().copy_text(code.clone());
                                        }
                                    }
                                    if q.image_png.is_some()
                                        && ui.small_button("💾 保存二维码").clicked()
                                    {
                                        outcome = Some(LoginOutcome::SaveQr);
                                    }
                                    if ui.small_button("⟳ 刷新二维码").clicked() {
                                        outcome = Some(LoginOutcome::StartQr);
                                    }
                                });
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
                let (required, download_extra) = cookie_required(kind);
                let howto = cookie_howto(kind);
                widgets::hint(
                    ui,
                    &t,
                    &format!(
                        "粘贴浏览器 Cookie（需含 {}）。{howto}",
                        required.join(" + ")
                    ),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.small_button("🌐 打开登录页").clicked() {
                        let url = login_url(kind);
                        let _ = wp_core::paths::open_path(std::path::Path::new(url));
                    }
                    if ui.small_button("清空").clicked() {
                        d.cookie_text.clear();
                    }
                });
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::multiline(&mut d.cookie_text)
                        .desired_width(f32::INFINITY)
                        .desired_rows(5)
                        .hint_text(cookie_placeholder(kind)),
                );
                let chk = cookie_check(kind, &d.cookie_text);
                ui.add_space(6.0);
                if !d.cookie_text.trim().is_empty() {
                    if chk.missing.is_empty() {
                        widgets::success_banner(
                            ui,
                            &t,
                            &format!(
                                "已识别 {} 个字段，必填字段齐全（{}）",
                                chk.count,
                                required.join("、")
                            ),
                        );
                        if !download_extra.is_empty()
                            && !download_extra.iter().any(|k| chk.present.iter().any(|p| p == k))
                        {
                            ui.add_space(4.0);
                            widgets::warn_banner(
                                ui,
                                &t,
                                &format!(
                                    "未发现 {}：下载直链可能被 CDN 拒绝（V9 老项目实测）；建议从浏览器复制完整 Cookie，或先尝试登录。",
                                    download_extra.join("/")
                                ),
                            );
                        }
                    } else {
                        widgets::error_banner(
                            ui,
                            &t,
                            &format!(
                                "缺少必填字段：{}（当前识别 {} 个字段）",
                                chk.missing.join("、"),
                                chk.count
                            ),
                        );
                    }
                }
                ui.add_space(6.0);
                ui.add_enabled_ui(chk.missing.is_empty(), |ui| {
                    if widgets::primary_button(ui, &t, "导入并校验").clicked() {
                        outcome = Some(LoginOutcome::Cookie);
                    }
                });
            }
            LoginTab::Sms => {
                let resend_left = d
                    .sms_resend_at
                    .map(|at| {
                        at.saturating_duration_since(std::time::Instant::now())
                            .as_secs()
                    })
                    .unwrap_or(0);
                widgets::hint(
                    ui,
                    &t,
                    "光鸭短信登录：手机号需与账号绑定（含国家码），验证码 5 分钟内有效，60 秒后可重发。",
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("手机号").size(12.5).color(t.text));
                    ui.add(
                        egui::TextEdit::singleline(&mut d.phone)
                            .desired_width(210.0)
                            .hint_text("+8613800000000"),
                    );
                    let phone_ok = phone_valid(&d.phone);
                    let btn_text = if resend_left > 0 {
                        format!("⏱ {resend_left}s")
                    } else if d.sms_hint.is_empty() {
                        "发送验证码".to_string()
                    } else {
                        "重新发送".to_string()
                    };
                    if ui
                        .add_enabled(
                            phone_ok && resend_left == 0,
                            egui::Button::new(RichText::new(btn_text).size(12.0)),
                        )
                        .clicked()
                    {
                        outcome = Some(LoginOutcome::SmsSend);
                    }
                });
                if !phone_valid(&d.phone) && !d.phone.trim().is_empty() {
                    ui.add_space(2.0);
                    widgets::error_banner(ui, &t, "手机号格式不对：只需数字加可选国家码 +，如 +8613800000000");
                }
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
                    let code_ok = code_valid(&d.sms_code);
                    if ui
                        .add_enabled(
                            code_ok && phone_valid(&d.phone),
                            egui::Button::new(RichText::new("登录").size(12.0)),
                        )
                        .clicked()
                    {
                        outcome = Some(LoginOutcome::SmsVerify);
                    }
                });
                if !code_valid(&d.sms_code) && !d.sms_code.trim().is_empty() {
                    ui.add_space(2.0);
                    widgets::error_banner(ui, &t, "验证码应为 4~8 位数字");
                }
            }
            LoginTab::Token => {
                widgets::hint(
                    ui,
                    &t,
                    "光鸭令牌登录：可粘贴 access token，也可粘贴登录接口返回的 JSON（含 access_token/refresh_token）。",
                );
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::multiline(&mut d.token_text)
                        .desired_width(f32::INFINITY)
                        .desired_rows(4)
                        .hint_text("access_token 或 {\"access_token\":\"...\",\"refresh_token\":\"...\"}"),
                );
                ui.add_space(6.0);
                let token_ok = !d.token_text.trim().is_empty();
                ui.add_enabled_ui(token_ok, |ui| {
                    if widgets::primary_button(ui, &t, "令牌登录").clicked() {
                        outcome = Some(LoginOutcome::Token);
                    }
                });
                if !token_ok && !d.token_text.trim().is_empty() {
                    ui.add_space(2.0);
                    widgets::error_banner(ui, &t, "令牌为空或只有空白字符");
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
                if !phone_valid(&phone) {
                    d.error = "手机号格式不对：只需数字加可选国家码 +，如 +8613800000000".into();
                } else {
                    d.busy = true;
                    d.error.clear();
                    d.sms_code.clear();
                    // 老项目同款 60s 重发倒计时；事件返回前先置位，避免连点
                    d.sms_resend_at =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
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
            LoginOutcome::SaveQr => {
                if let Some(png) = d.qr.as_ref().and_then(|q| q.image_png.clone()) {
                    let dir = app.paths.root.join("登录");
                    let _ = std::fs::create_dir_all(&dir);
                    let file = dir.join(format!("{}-二维码.png", d.drive));
                    match std::fs::write(&file, &png) {
                        Ok(()) => app.toast(
                            crate::app::ToastKind::Success,
                            format!("二维码已保存：{}", file.display()),
                        ),
                        Err(e) => d.error = format!("保存二维码失败：{e}"),
                    }
                } else {
                    d.error = "当前没有可保存的二维码（可能使用链接扫码）".into();
                }
            }
        }
    }
    // 记住该网盘类型上次使用的登录方式（同一网盘多实例共用）
    if let Some(slot) = app.slot_by_id(&d.drive) {
        let key = slot.cfg.kind.slug().to_string();
        app.login_tabs.insert(key, d.tab);
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
    SaveQr,
}

// ───────────── 登录方式差异化的纯逻辑（可单测） ─────────────

/// 各网盘 Cookie 必需字段 / 下载额外字段。
fn cookie_required(kind: DriveKind) -> (&'static [&'static str], &'static [&'static str]) {
    match kind {
        // 第二组是「下载直链额外必需字段」：百度无额外必需字段，夸克实测要求 __puus
        DriveKind::Baidu => (&["BDUSS", "STOKEN"], &[]),
        DriveKind::Quark => (&["__kps", "__uid"], &["__puus"]),
        _ => (&[], &[]),
    }
}

fn cookie_howto(kind: DriveKind) -> &'static str {
    match kind {
        DriveKind::Baidu => {
            "获取：浏览器登录 pan.baidu.com → F12 → Network → 任一请求 → 复制 Request Headers 里 Cookie 整行。"
        }
        DriveKind::Quark => {
            "获取：浏览器登录 pan.quark.cn → F12 → Network → 任一请求 → 复制 Cookie 整行（含 __pus/__puus 下载更稳）。"
        }
        _ => "该网盘暂不支持 Cookie 登录。",
    }
}

fn login_url(kind: DriveKind) -> &'static str {
    match kind {
        DriveKind::Baidu => "https://pan.baidu.com/",
        DriveKind::Quark => "https://pan.quark.cn/",
        DriveKind::Guangya => "https://www.guangya.com/",
        _ => "https://pan.baidu.com/",
    }
}

fn cookie_placeholder(kind: DriveKind) -> &'static str {
    match kind {
        DriveKind::Baidu => "BDUSS=...; STOKEN=...; （可整行粘贴 Cookie: ...）",
        DriveKind::Quark => "__pus=...; __kps=...; __uid=...; __puus=...",
        _ => "Cookie",
    }
}

#[derive(Debug, Default, Clone)]
struct CookieCheck {
    count: usize,
    present: Vec<String>,
    missing: Vec<&'static str>,
}

/// 解析 Cookie 字符串（容忍 `Cookie:` 前缀、换行、多余空格）。
fn parse_cookie_pairs(text: &str) -> Vec<(String, String)> {
    let t = text.trim();
    let t = t
        .strip_prefix("Cookie:")
        .or_else(|| t.strip_prefix("cookie:"))
        .unwrap_or(t);
    t.split(';')
        .filter_map(|part| {
            let mut kv = part.splitn(2, '=');
            let k = kv.next()?.trim();
            let v = kv.next()?.trim();
            if k.is_empty() || v.is_empty() {
                None
            } else {
                Some((k.to_string(), v.to_string()))
            }
        })
        .collect()
}

fn cookie_check(kind: DriveKind, text: &str) -> CookieCheck {
    let pairs = parse_cookie_pairs(text);
    let present: Vec<String> = pairs.iter().map(|(k, _)| k.clone()).collect();
    let (required, _) = cookie_required(kind);
    let missing = required
        .iter()
        .copied()
        .filter(|r| !present.iter().any(|k| k == r))
        .collect();
    CookieCheck {
        count: pairs.len(),
        present,
        missing,
    }
}

fn phone_valid(phone: &str) -> bool {
    let d = phone.trim().trim_start_matches('+');
    d.len() >= 6 && d.chars().all(|c| c.is_ascii_digit())
}

fn code_valid(code: &str) -> bool {
    let c = code.trim();
    (4..=8).contains(&c.len()) && c.chars().all(|x| x.is_ascii_digit())
}

#[cfg(test)]
mod login_tests {
    use super::*;

    #[test]
    fn cookie_parse_tolerates_prefix_and_spaces() {
        let pairs = parse_cookie_pairs("Cookie: BDUSS=abc; STOKEN= def ; ;; x");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("BDUSS".into(), "abc".into()));
        assert_eq!(pairs[1], ("STOKEN".into(), "def".into()));
    }

    #[test]
    fn cookie_check_per_drive_required_keys() {
        let c = cookie_check(DriveKind::Baidu, "BDUSS=a; STOKEN=b");
        assert!(c.missing.is_empty());
        assert_eq!(c.count, 2);
        let c = cookie_check(DriveKind::Baidu, "BDUSS=a");
        assert_eq!(c.missing, vec!["STOKEN"]);
        let c = cookie_check(DriveKind::Quark, "__pus=1; __uid=3");
        assert_eq!(c.missing, vec!["__kps"]); // __kps 缺失（__pus/uid 有）
        let c = cookie_check(DriveKind::Quark, "__kps=2; __uid=3");
        assert!(c.missing.is_empty());
    }

    #[test]
    fn phone_and_code_validation() {
        assert!(phone_valid("+8613800000000"));
        assert!(phone_valid("13800000000"));
        assert!(!phone_valid("+86abc"));
        assert!(!phone_valid("123"));
        assert!(code_valid("123456"));
        assert!(code_valid("1234"));
        assert!(!code_valid("12a4"));
        assert!(!code_valid("12"));
    }
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
    // 未点「确定/取消」时必须把对话框写回，否则模态框只渲染一帧就消失
    // （之前 Confirm 永远返回 None，空目录引导等确认窗会闪一下就没了）。
    if !yes {
        return Some(Dialog::Confirm(d));
    }
    {
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
            ConfirmAction::RunPendingDirAction => {
                app.apply_pending_dir_action();
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
        DirPickerAction::PickCrossCopy { .. } => ("选择目标目录…", "选择此目录"),
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
        // 跨盘「浏览目标目录」取消：退回跨盘弹窗，不丢失已选条目
        if let DirPickerAction::PickCrossCopy { src_drive, paths } = &d.action {
            app.dialog = Some(Dialog::CrossCopy(CrossCopyDialog {
                src_drive: src_drive.clone(),
                paths: paths.clone(),
                dst_drive: d.drive.clone(),
                dst_dir: d.path.clone(),
            }));
        }
        return false;
    }
    if commit {
        let dest = d.path.clone();
        match &d.action {
            DirPickerAction::Move { paths } => {
                app.do_move_copy(&d.drive, paths, &dest, true);
            }
            DirPickerAction::Copy { paths } => {
                app.do_move_copy(&d.drive, paths, &dest, false);
            }
            DirPickerAction::PickCrossCopy { src_drive, paths } => {
                app.pending_cross = None;
                app.dialog = Some(Dialog::CrossCopy(CrossCopyDialog {
                    src_drive: src_drive.clone(),
                    paths: paths.clone(),
                    dst_drive: d.drive.clone(),
                    dst_dir: dest,
                }));
            }
        }
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
    let mut browse = false;
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
                            if d.dst_drive != *id {
                                d.dst_drive = id.clone();
                                d.dst_dir = "/".into();
                            }
                        }
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("目标目录").size(12.5).color(t.text));
            ui.add(
                egui::TextEdit::singleline(&mut d.dst_dir)
                    .desired_width(200.0)
                    .hint_text("/"),
            );
            if ui.button("📁 浏览选择…").clicked() {
                browse = true;
            }
        });
        widgets::hint(
            ui,
            &t,
            "目标目录可手填，也可点「浏览选择…」用鼠标逐级选择目标网盘的目录；\
             流程：源盘下载到本地缓存 → 上传到目标盘（不经第三方中转服务）。",
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
    if browse {
        // 记录待回填的源实例/条目，打开目标盘目录选择器
        app.pending_cross = Some((d.src_drive.clone(), d.paths.clone()));
        let start_dir = if d.dst_dir.trim().is_empty() {
            "/".to_string()
        } else {
            d.dst_dir.clone()
        };
        let action = DirPickerAction::PickCrossCopy {
            src_drive: d.src_drive.clone(),
            paths: d.paths.clone(),
        };
        app.open_dir_picker_at(&d.dst_drive, &start_dir, action);
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
    /// 给 DirPicker 加载目录（页面里直接调用，默认当前盘/当前路径）。
    pub fn open_dir_picker(&mut self, action: DirPickerAction) {
        let drive = self.active_drive_id();
        let path = self
            .browse_ref(&drive)
            .map(|s| s.path.clone())
            .unwrap_or_else(|| "/".into());
        self.open_dir_picker_at(&drive, &path, action);
    }

    /// 在指定网盘/路径打开目录选择器（跨盘「浏览目标目录」用）。
    pub fn open_dir_picker_at(&mut self, drive: &str, path: &str, action: DirPickerAction) {
        self.dialog = Some(Dialog::DirPicker(DirPicker {
            drive: drive.to_string(),
            path: path.to_string(),
            entries: Vec::new(),
            loading: false,
            error: String::new(),
            action,
        }));
    }
}

// ═══════════════════════════ 账号详情 ═══════════════════════════

/// 账号详情：实例/类型/状态 + 账号名、UID、会员/到期、容量（总/已用/剩余/百分比）
/// + 只读原因 + 接口返回的原始字段（各网盘字段不同，原样展示便于核对）。
fn account_details(
    app: &mut App,
    ctx: &egui::Context,
    d: crate::app::AccountDetailsDialog,
) -> Option<Dialog> {
    let t = app.theme;
    let mut close = false;
    let acc = &d.account;
    let name = if acc.user_name.trim().is_empty() {
        acc.display_name.clone()
    } else {
        acc.user_name.clone()
    };
    let uid = if acc.user_id.trim().is_empty() {
        "—".to_string()
    } else {
        acc.user_id.clone()
    };
    let vip = if acc.vip.trim().is_empty() {
        "—".to_string()
    } else {
        acc.vip.clone()
    };
    egui::Modal::new(crate::app::modal_id("account_details_modal")).show(ctx, |ui| {
        ui.set_width(620.0);
        title_row(ui, &t, &format!("账号详情 · {}", d.instance), &mut close);

        widgets::kv_row(ui, &t, "实例标识", &d.instance);
        widgets::kv_row(ui, &t, "网盘类型", d.kind.label());
        widgets::kv_row(ui, &t, "登录状态", &d.status.label());
        widgets::kv_row(ui, &t, "账号名", &name);
        widgets::kv_row(ui, &t, "显示名", &acc.display_name);
        widgets::kv_row(ui, &t, "用户 ID", &uid);
        widgets::kv_row(ui, &t, "会员", &vip);
        if let Some(exp) = crate::pages::accounts::vip_expire_text(acc) {
            widgets::kv_row(ui, &t, "会员到期", &exp);
        }
        if let Some(avatar) = crate::pages::accounts::avatar_url(acc) {
            widgets::kv_row(ui, &t, "头像地址", &avatar);
        }
        match acc.quota.filter(|q| q.total > 0) {
            Some(q) => {
                widgets::kv_row(ui, &t, "容量总量", &wp_core::util::format_size(q.total));
                widgets::kv_row(ui, &t, "已用", &wp_core::util::format_size(q.used));
                widgets::kv_row(
                    ui,
                    &t,
                    "剩余",
                    &wp_core::util::format_size(q.total.saturating_sub(q.used)),
                );
                widgets::kv_row(
                    ui,
                    &t,
                    "已用占比",
                    &format!("{:.1}%", q.used_ratio() * 100.0),
                );
            }
            None => widgets::kv_row(ui, &t, "容量", "未获取（未登录/只读会话/接口不可用）"),
        }
        if let crate::app::AccountStatus::ReadOnly(reason) = &d.status {
            widgets::kv_row(ui, &t, "只读原因", reason);
        }
        ui.add_space(6.0);
        ui.label(
            RichText::new("接口返回的原始字段")
                .size(12.5)
                .strong()
                .color(t.text),
        );
        let raw = serde_json::to_string_pretty(&serde_json::Value::Object(acc.extra.clone()))
            .unwrap_or_default();
        egui::ScrollArea::vertical()
            .id_salt("account_extra")
            .max_height(220.0)
            .show(ui, |ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(&raw)
                            .size(10.5)
                            .monospace()
                            .color(t.text_weak),
                    )
                    .wrap(),
                );
            });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("关闭").clicked() {
                close = true;
            }
            widgets::hint(
                ui,
                &t,
                "字段是各网盘接口原样返回；只读/空间超限会在状态与容量处标出。",
            );
        });
    });
    if close {
        None
    } else {
        Some(Dialog::AccountDetails(d))
    }
}
