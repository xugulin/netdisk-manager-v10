//! 账号与容量总览页：每个网盘实例一张卡片，显示登录状态、账号名/UID、会员/到期、
//! 容量进度条（总/已用/剩余）；「详情」弹窗展示接口返回的完整字段。
//!
//! 数据来源：`Drive::account()`（百度 /api/user/getinfo + /api/quota；夸克 member；
//! 光鸭 /v1/user/me + /assets/v1/get_assets；本机 statvfs 磁盘容量）。

use crate::app::{AccountDetailsDialog, AccountStatus, App, Dialog};
use crate::theme::Theme;
use crate::widgets;
use egui::{RichText, Ui};
use serde_json::Value;
use std::time::{Duration, Instant};
use wp_core::model::{AccountInfo, DriveKind};
use wp_core::util::{format_size, format_time};

pub fn show(app: &mut App, ui: &mut Ui) {
    let t = app.theme;
    // 验收截图：所有账号加载完后自动打开「详情」弹窗（--features acceptance + 环境变量）
    #[cfg(feature = "acceptance")]
    if cfg!(feature = "acceptance")
        && std::env::var("WP_DEMO_ACCOUNT_DETAILS").is_ok()
        && app.dialog.is_none()
    {
        // 优先挑「有容量且有 UID」的实例，截图更完整；否则第一个有账号信息的
        let picked = app
            .drives
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.account.clone().map(|a| (i, a)))
            .find(|(i, a)| {
                *i != 0 && a.quota.is_some() && !a.extra.is_empty() && !a.user_id.trim().is_empty()
            });
        if let Some((idx, acc)) = picked {
            let s = &app.drives[idx];
            app.dialog = Some(Dialog::AccountDetails(AccountDetailsDialog {
                instance: s.id().to_string(),
                kind: s.cfg.kind,
                status: s.status.clone(),
                account: acc,
            }));
        }
    }
    // 进入页面自动刷新（30s 节流），保证容量/账号信息是新的
    let need = app
        .accounts_refresh_at
        .map(|at| at.elapsed() > Duration::from_secs(30))
        .unwrap_or(true);
    if need && !app.loading_accounts {
        app.accounts_refresh_at = Some(Instant::now());
        app.refresh_all_accounts();
    }

    let mut acts: Vec<Act> = Vec::new();
    ui.horizontal(|ui| {
        widgets::section_title(ui, &t, "账号与容量");
        ui.add_space(12.0);
        widgets::hint(
            ui,
            &t,
            "每个实例一张卡片；容量来自网盘实时接口，本机显示所在磁盘容量。「详情」可看完整账号字段。",
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::primary_button(ui, &t, "↻ 刷新全部").clicked() {
                acts.push(Act::RefreshAll);
            }
        });
    });
    ui.add_space(8.0);

    if app.drives.is_empty() {
        widgets::empty_state(
            ui,
            &t,
            "还没有网盘实例",
            "在「设置 → 添加网盘」创建实例后，这里会显示每个账号的容量与详情。",
        );
        return;
    }

    egui::ScrollArea::vertical()
        .id_salt("accounts_scroll")
        .show(ui, |ui| {
            for idx in 0..app.drives.len() {
                let (id, kind, display, status, account, init_error, loaded) = {
                    let s = &app.drives[idx];
                    (
                        s.id().to_string(),
                        s.cfg.kind,
                        s.display().to_string(),
                        s.status.clone(),
                        s.account.clone(),
                        s.init_error.clone(),
                        s.drive.is_some(),
                    )
                };
                widgets::card(ui, &t, |ui| {
                    ui.horizontal(|ui| {
                        let (st_text, st_color) = status_style(&status, &t);
                        ui.label(
                            RichText::new(format!("{}  {display}", kind.icon()))
                                .size(15.0)
                                .strong()
                                .color(t.text),
                        );
                        ui.label(
                            RichText::new(format!("{} · {id}", kind.label()))
                                .size(11.0)
                                .color(t.text_weak),
                        );
                        ui.add_space(6.0);
                        ui.label(RichText::new(st_text).size(12.0).color(st_color));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if widgets::secondary_button(ui, &t, "详情").clicked() {
                                if let Some(acc) = account.clone() {
                                    acts.push(Act::Details {
                                        instance: id.clone(),
                                        kind,
                                        status: status.clone(),
                                        account: acc,
                                    });
                                } else {
                                    acts.push(Act::Toast(
                                        "暂无账号信息可展示，请先刷新/登录".into(),
                                    ));
                                }
                            }
                            if matches!(
                                status,
                                AccountStatus::LoggedIn | AccountStatus::ReadOnly(_)
                            ) && loaded
                            {
                                if ui.button("退出").clicked() {
                                    acts.push(Act::Logout(id.clone()));
                                }
                            }
                            if ui.button("↻ 刷新").clicked() {
                                acts.push(Act::Refresh(id.clone()));
                            }
                            if matches!(
                                status,
                                AccountStatus::LoggedOut
                                    | AccountStatus::ReadOnly(_)
                                    | AccountStatus::Failed(_)
                            ) && loaded
                            {
                                if ui.button("登录").clicked() {
                                    acts.push(Act::Login(id.clone()));
                                }
                            }
                        });
                    });
                    ui.add_space(4.0);
                    if let Some(e) = &init_error {
                        widgets::error_banner(ui, &t, &format!("初始化失败：{e}"));
                    }
                    match &account {
                        Some(acc) => account_body(ui, &t, acc),
                        None => {
                            let reason = match &status {
                                AccountStatus::LoggedOut => {
                                    "尚未登录：点右侧「登录」后即可显示容量与账号详情。".to_string()
                                }
                                AccountStatus::ReadOnly(r) => {
                                    format!("会话只读：{r}（读接口可用，容量以刷新结果为准）")
                                }
                                AccountStatus::Failed(e) => format!("状态异常：{e}"),
                                AccountStatus::Checking => "正在读取账号信息…".to_string(),
                                AccountStatus::Unknown => "尚未检查，点「刷新」。".to_string(),
                                AccountStatus::LoggedIn => {
                                    "已登录，但接口未返回账号信息；点「刷新」重试。".to_string()
                                }
                            };
                            widgets::hint(ui, &t, &reason);
                        }
                    }
                });
                ui.add_space(8.0);
            }
        });

    for a in acts {
        apply(app, a);
    }
}

fn account_body(ui: &mut Ui, t: &Theme, acc: &AccountInfo) {
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
    ui.horizontal_wrapped(|ui| {
        inline(ui, t, "账号", &name);
        inline(ui, t, "UID", &uid);
        inline(ui, t, "会员", &vip);
        if let Some(exp) = vip_expire_text(acc) {
            inline(ui, t, "到期", &exp);
        }
        if let Some(av) = avatar_url(acc) {
            inline(ui, t, "头像", &widgets::ellipsis(&av, 42));
        }
    });
    ui.add_space(4.0);
    match acc.quota.filter(|q| q.total > 0) {
        Some(q) => {
            let ratio = q.used_ratio();
            let (color, level) = if ratio > 0.95 {
                (t.error, "空间紧张")
            } else if ratio > 0.8 {
                (t.warning, "空间偏满")
            } else {
                (t.success, "空间充足")
            };
            let text = format!(
                "已用 {} / 总 {} · 剩余 {}（{:.1}%，{level}）",
                format_size(q.used),
                format_size(q.total),
                format_size(q.total.saturating_sub(q.used)),
                ratio * 100.0
            );
            ui.add(
                egui::ProgressBar::new(ratio)
                    .desired_width(ui.available_width())
                    .text(RichText::new(text).size(11.5).color(t.text)),
            );
            ui.add_space(2.0);
            let _ = color;
        }
        None => {
            widgets::hint(
                ui,
                t,
                "容量：未获取（账号未登录 / 只读会话 / 接口不可用，可点「刷新」重试）",
            );
        }
    }
}

fn inline(ui: &mut Ui, t: &Theme, label: &str, value: &str) {
    ui.label(
        RichText::new(format!("{label}："))
            .size(11.5)
            .color(t.text_weak),
    );
    ui.label(RichText::new(value).size(12.0).color(t.text));
    ui.add_space(12.0);
}

fn status_style(s: &AccountStatus, t: &Theme) -> (String, egui::Color32) {
    match s {
        AccountStatus::LoggedIn => ("🟢 已登录".into(), t.success),
        AccountStatus::ReadOnly(_) => ("🟠 已登录（只读）".into(), t.warning),
        AccountStatus::LoggedOut => ("🟡 未登录".into(), t.warning),
        AccountStatus::Checking => ("⏳ 检查中".into(), t.warning),
        AccountStatus::Failed(e) => (format!("🔴 {e}"), t.error),
        AccountStatus::Unknown => ("⚪ 未检查".into(), t.text_weak),
    }
}

fn extra_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// 会员到期时间（各网盘字段不同）：夸克 exp_at、光鸭 assets.vipExpireTime、百度 record.vip_end_time 等。
pub(crate) fn vip_expire_text(acc: &AccountInfo) -> Option<String> {
    let extra = &acc.extra;
    let candidates = [
        extra.get("exp_at"),
        extra.get("assets").and_then(|a| a.get("vipExpireTime")),
        extra.get("assets").and_then(|a| a.get("vipExpireTimeMs")),
        extra.get("record").and_then(|r| r.get("vip_expire")),
        extra.get("record").and_then(|r| r.get("vip_expire_time")),
        extra.get("record").and_then(|r| r.get("vip_end_time")),
        extra.get("vip_expire"),
        extra.get("expire_at"),
    ];
    for c in candidates {
        if let Some(ts) = c.and_then(extra_i64).filter(|ts| *ts > 0) {
            let secs = if ts > 100_000_000_000 { ts / 1000 } else { ts };
            let text = format_time(secs);
            if text != "—" {
                return Some(text);
            }
        }
    }
    None
}

/// 头像地址（百度 record.avatar_url / 通用 avatar_url）。
pub(crate) fn avatar_url(acc: &AccountInfo) -> Option<String> {
    let extra = &acc.extra;
    let candidates = [
        extra.get("avatar_url"),
        extra.get("record").and_then(|r| r.get("avatar_url")),
        extra.get("avatar"),
        extra.get("user").and_then(|u| u.get("avatar")),
    ];
    for c in candidates {
        if let Some(s) = c.and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
            return Some(s.trim().to_string());
        }
    }
    None
}

enum Act {
    RefreshAll,
    Refresh(String),
    Login(String),
    Logout(String),
    Toast(String),
    Details {
        instance: String,
        kind: DriveKind,
        status: AccountStatus,
        account: AccountInfo,
    },
}

fn apply(app: &mut App, act: Act) {
    match act {
        Act::RefreshAll => {
            app.accounts_refresh_at = Some(Instant::now());
            app.refresh_all_accounts();
            app.toast(crate::app::ToastKind::Info, "正在刷新所有实例的账号与容量…");
        }
        Act::Refresh(id) => {
            app.refresh_account(&id);
            app.toast(
                crate::app::ToastKind::Info,
                format!("正在刷新 {id} 的账号信息…"),
            );
        }
        Act::Login(id) => {
            if let Some(idx) = app.slot_idx(&id) {
                app.active = idx;
                app.page = crate::app::Page::Browser;
                app.open_login_dialog();
            } else {
                app.toast(crate::app::ToastKind::Warn, "该实例未启用，无法登录");
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
        Act::Toast(msg) => app.toast(crate::app::ToastKind::Warn, msg),
        Act::Details {
            instance,
            kind,
            status,
            account,
        } => {
            app.dialog = Some(Dialog::AccountDetails(AccountDetailsDialog {
                instance,
                kind,
                status,
                account,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc_with_extra(extra: serde_json::Value) -> AccountInfo {
        AccountInfo {
            display_name: "u".into(),
            user_name: "u".into(),
            user_id: "1".into(),
            vip: "VIP".into(),
            quota: None,
            extra: extra.as_object().cloned().unwrap_or_default(),
        }
    }

    #[test]
    fn vip_expire_reads_quark_and_guangya_fields() {
        // 夸克 exp_at（秒）
        let a = acc_with_extra(serde_json::json!({"exp_at": 1893456000}));
        assert!(vip_expire_text(&a).is_some());
        // 光鸭 assets.vipExpireTime（秒）
        let a = acc_with_extra(serde_json::json!({"assets": {"vipExpireTime": 1877598254}}));
        let text = vip_expire_text(&a).expect("光鸭到期时间");
        assert!(text.starts_with("20"), "格式化日期：{text}");
        // 毫秒也兼容
        let a = acc_with_extra(serde_json::json!({"exp_at": 1893456000000i64}));
        assert!(vip_expire_text(&a).is_some());
        // 无字段
        let a = acc_with_extra(serde_json::json!({}));
        assert!(vip_expire_text(&a).is_none());
    }

    #[test]
    fn avatar_url_reads_baidu_record() {
        let a = acc_with_extra(serde_json::json!({"record": {"avatar_url": "https://x/a.jpg"}}));
        assert_eq!(avatar_url(&a).as_deref(), Some("https://x/a.jpg"));
        let a = acc_with_extra(serde_json::json!({"avatar_url": "https://x/b.jpg"}));
        assert_eq!(avatar_url(&a).as_deref(), Some("https://x/b.jpg"));
        let a = acc_with_extra(serde_json::json!({"avatar_url": ""}));
        assert_eq!(avatar_url(&a), None);
    }
}
