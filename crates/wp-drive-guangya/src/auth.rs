//! 认证：扫码（设备授权码）、短信验证码、令牌导入、当前用户信息。
//!
//! 认证域名与业务域名分开：
//! * `account.guangyapan.com`：登录/刷新/用户信息，请求头不带 `did/dt/smid/traceparent`；
//! * `api.guangyapan.com`：业务接口，带全套设备头。
//!
//! 扫码轮询对 OAuth 错误码做显式映射（Python 原实现会一直轮询到超时）。

use reqwest::Method;
use serde_json::{json, Value};
use std::io::Cursor;
use std::sync::MutexGuard;
use wp_core::model::{AccountInfo, QrState, Quota};
use wp_core::util::now_ts;
use wp_core::{Error, Result};
use wp_drive::{QrChallenge, QrPoll};

use crate::session::jwt_exp;
use crate::{
    as_i64, data_or_self, parse_api_body, snippet, GuangyaDrive, QrPending, SmsPending, CLIENT_ID,
    SDK_LOGIN, SDK_REFRESH,
};

/// 扫码轮询最长等待（与 Python 的 `min(expires_in, 300)` 对齐）。
const QR_MAX_WAIT_SECS: u64 = 300;

// ==================== 当前用户 / 退出 ====================

/// `GET /v1/user/me` + 资产信息（容量/VIP）。
pub(crate) async fn account_info(drive: &GuangyaDrive) -> Result<AccountInfo> {
    let mut token = drive.bearer().await?;
    let mut call = drive
        .account_call(
            Method::GET,
            "/v1/user/me",
            None,
            Some(&token),
            SDK_LOGIN,
            &[],
        )
        .await?;
    if call.0 == 401 {
        // 用户信息也做一次 401 自愈（Python 没有，但这里成本很低）。
        let h = drive.account_headers(SDK_REFRESH)?;
        token = drive.session.force_refresh(&drive.api, &h).await?;
        call = drive
            .account_call(
                Method::GET,
                "/v1/user/me",
                None,
                Some(&token),
                SDK_LOGIN,
                &[],
            )
            .await?;
    }
    let (status, text, request_id) = call;
    let value = parse_api_body(status, &text, request_id.as_deref(), &[])?;
    let user = data_or_self(&value);
    let user = if user.get("user").map(|u| u.is_object()).unwrap_or(false) {
        &user["user"]
    } else {
        user
    };

    // 资产信息失败不影响账号基本信息（容量只是可选展示）。
    let assets = drive
        .api_post(
            "/assets/v1/get_assets",
            json!({ "clientId": CLIENT_ID }),
            &[],
        )
        .await
        .ok()
        .map(|v| data_or_self(&v).clone());

    Ok(account_from_values(
        user,
        assets.as_ref(),
        &drive.session.sub(),
    ))
}

/// 退出登录：只清本地令牌与临时登录状态。
pub(crate) async fn logout(drive: &GuangyaDrive) -> Result<()> {
    drive.session.clear()?;
    *lock(&drive.qr) = None;
    *lock(&drive.sms) = None;
    Ok(())
}

/// 从 `/v1/user/me` 的 data 与 `get_assets` 的 data 组装统一账号信息。
pub(crate) fn account_from_values(
    user: &Value,
    assets: Option<&Value>,
    token_sub: &str,
) -> AccountInfo {
    let user_id = first_str(user, &["sub", "userId", "user_id", "uid", "id"])
        .or_else(|| {
            if token_sub.trim().is_empty() {
                None
            } else {
                Some(token_sub.trim().to_string())
            }
        })
        .unwrap_or_default();
    let display_name = first_str(
        user,
        &[
            "name",
            "nickName",
            "nickname",
            "displayName",
            "userName",
            "username",
            "phone_number",
            "phone",
            "email",
            "sub",
        ],
    )
    .or_else(|| Some(user_id.clone()).filter(|s| !s.is_empty()))
    .unwrap_or_else(|| "光鸭云盘用户".to_string());
    let user_name = first_str(
        user,
        &[
            "phone_number",
            "phone",
            "email",
            "username",
            "userName",
            "user_name",
        ],
    )
    .unwrap_or_default();

    let mut extra = serde_json::Map::new();
    if !user.is_null() {
        extra.insert("user".into(), user.clone());
    }
    if let Some(a) = assets {
        extra.insert("assets".into(), a.clone());
    }

    let vip = match assets {
        Some(a) => {
            let svip = as_i64(a.get("svipStatus").unwrap_or(&Value::Null)).unwrap_or(0);
            let vip = as_i64(a.get("vipStatus").unwrap_or(&Value::Null)).unwrap_or(0);
            if svip == 2 {
                "SVIP".to_string()
            } else if vip == 2 {
                "VIP".to_string()
            } else if svip == 3 || svip == 4 || vip == 3 || vip == 4 {
                "会员已过期".to_string()
            } else {
                String::new()
            }
        }
        None => String::new(),
    };

    let quota = assets.and_then(quota_from_assets);
    AccountInfo {
        display_name,
        user_name,
        user_id,
        vip,
        quota,
        extra,
    }
}

fn quota_from_assets(a: &Value) -> Option<Quota> {
    let total = first_u64(a, &["totalSpaceSize", "totalSpace", "total"]);
    let used = first_u64(a, &["usedSpaceSize", "usedSpace", "used"]);
    let free = first_u64(a, &["freeSpaceSize", "freeSpace", "free"])
        .unwrap_or_else(|| total.unwrap_or(0).saturating_sub(used.unwrap_or(0)));
    let total = total.unwrap_or(0);
    if total == 0 && used.unwrap_or(0) == 0 && free == 0 {
        return None;
    }
    Some(Quota {
        total,
        used: used.unwrap_or(0),
        free,
    })
}

fn first_u64(v: &Value, keys: &[&str]) -> Option<u64> {
    for k in keys {
        let Some(val) = v.get(*k) else { continue };
        if let Some(n) = val.as_u64() {
            return Some(n);
        }
        if let Some(n) = val.as_i64() {
            if n >= 0 {
                return Some(n as u64);
            }
        }
        if let Some(s) = val.as_str() {
            if let Ok(n) = s.trim().parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

fn first_str(v: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        let Some(val) = v.get(*k) else { continue };
        let s = match val {
            Value::String(s) => s.trim().to_string(),
            Value::Number(n) => n.to_string(),
            _ => continue,
        };
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

fn minimal_account(sub: &str) -> AccountInfo {
    let id = sub.trim().to_string();
    AccountInfo {
        display_name: if id.is_empty() {
            "光鸭云盘用户".to_string()
        } else {
            id.clone()
        },
        user_id: id,
        ..Default::default()
    }
}

/// 登录成功后尽量取完整账号信息；网络问题退化为最小信息，真正的 Auth 错误才失败。
async fn finish_login(drive: &GuangyaDrive, sub: &str) -> Result<AccountInfo> {
    match account_info(drive).await {
        Ok(a) => Ok(a),
        Err(e) if e.is_auth() => Err(e),
        Err(e) => {
            wp_core::log_debug!("光鸭", "登录后取账号信息失败（忽略）：{e}");
            Ok(minimal_account(sub))
        }
    }
}

// ==================== 令牌导入 ====================

/// 解析令牌导入输入。
///
/// 支持三种写法：
/// 1. `token` = access_token，`extra` = refresh_token；
/// 2. `token` = JSON：`{"access_token":..,"refresh_token":..,"expires_in":..,"sub":..}`；
/// 3. `extra` = JSON（只提供 refresh/expires/sub），`token` 仍是 access_token。
pub(crate) fn parse_token_input(
    token: &str,
    extra: &str,
) -> Result<(String, String, Option<i64>, String)> {
    let token = token.trim();
    let access: String;
    let mut refresh = String::new();
    let mut expires_in = None;
    let mut sub = String::new();

    if token.starts_with('{') {
        let v: Value = serde_json::from_str(token)
            .map_err(|e| Error::auth(format!("令牌 JSON 解析失败：{e}")))?;
        access = first_str(&v, &["access_token", "accessToken", "token"]).unwrap_or_default();
        refresh = first_str(&v, &["refresh_token", "refreshToken"]).unwrap_or_default();
        expires_in = v.get("expires_in").and_then(as_i64);
        sub = first_str(&v, &["sub", "user_id", "userId"]).unwrap_or_default();
    } else {
        access = token
            .strip_prefix("Bearer ")
            .unwrap_or(token)
            .trim()
            .to_string();
        let extra = extra.trim();
        if extra.starts_with('{') {
            let v: Value = serde_json::from_str(extra)
                .map_err(|e| Error::auth(format!("附加令牌 JSON 解析失败：{e}")))?;
            refresh = first_str(&v, &["refresh_token", "refreshToken"]).unwrap_or_default();
            expires_in = v.get("expires_in").and_then(as_i64);
            sub = first_str(&v, &["sub", "user_id", "userId"]).unwrap_or_default();
        } else if !extra.is_empty() {
            let r = extra
                .strip_prefix("refresh_token=")
                .or_else(|| extra.strip_prefix("refresh="))
                .unwrap_or(extra)
                .trim();
            if !r.is_empty() {
                refresh = r.to_string();
            }
        }
    }

    if access.is_empty() {
        return Err(Error::auth("令牌为空：请粘贴 access_token 或登录 JSON"));
    }
    if expires_in.unwrap_or(0) <= 0 {
        // JWT 的 exp 可能已过期：保留负值，让会话层判定为“立即过期”，
        // 而不是错误地按默认 7200 秒当作有效令牌。
        expires_in = jwt_exp(&access).map(|exp| (exp - now_ts() as f64).floor() as i64);
    }
    Ok((access, refresh, expires_in, sub))
}

// ==================== Drive 登录接口 ====================

impl GuangyaDrive {
    /// 开始扫码登录（Drive::begin_qr 的实现体）。
    pub(crate) async fn qr_begin(&self) -> Result<QrChallenge> {
        let body = json!({
            "scope": "",
            "client_id": CLIENT_ID,
            "project_id": "",
        });
        let (status, text, request_id) = self
            .account_call(
                Method::POST,
                "/v1/auth/device/code",
                Some(&body),
                None,
                SDK_LOGIN,
                &[],
            )
            .await?;
        let value = parse_api_body(status, &text, request_id.as_deref(), &[])?;
        let d = data_or_self(&value);

        let device_code = first_str(d, &["device_code", "deviceCode"]).ok_or_else(|| {
            Error::server(
                status as i64,
                format!("设备授权码响应异常：{}", snippet(&text, 300)),
            )
        })?;
        let user_code = first_str(d, &["user_code", "userCode"]).unwrap_or_default();
        let uri = first_str(
            d,
            &[
                "verification_uri_complete",
                "verification_uri",
                "verification_url",
            ],
        )
        .unwrap_or_default();
        let interval = as_i64(d.get("interval").unwrap_or(&Value::Null))
            .unwrap_or(3)
            .max(1) as u64;
        let expires_in = as_i64(d.get("expires_in").unwrap_or(&Value::Null))
            .unwrap_or(120)
            .max(30) as u64;
        let now = now_ts();
        let wait_secs = expires_in.min(QR_MAX_WAIT_SECS);
        *lock(&self.qr) = Some(QrPending {
            device_code,
            interval_secs: interval,
            expires_at: now + wait_secs as i64,
            next_poll_at: now + interval as i64,
        });

        let image_png = if uri.is_empty() { None } else { qr_png(&uri) };
        let hint = if user_code.is_empty() {
            format!("请使用光鸭云盘 App 扫码登录（{wait_secs} 秒内有效）")
        } else {
            format!("请扫码登录；也可在 App 输入用户码 {user_code}（{wait_secs} 秒内有效）")
        };
        Ok(QrChallenge {
            image_png,
            url: if uri.is_empty() { None } else { Some(uri) },
            code: if user_code.is_empty() {
                None
            } else {
                Some(user_code)
            },
            hint,
            expires_in_secs: wait_secs,
        })
    }

    /// 轮询扫码状态（Drive::poll_qr 的实现体）。
    pub(crate) async fn qr_poll(&self) -> Result<QrPoll> {
        let Some(pending) = lock(&self.qr).clone() else {
            return Err(Error::other("没有进行中的扫码登录，请先调用 begin_qr()"));
        };
        let now = now_ts();
        if now >= pending.expires_at {
            *lock(&self.qr) = None;
            return Ok(QrPoll::expired("二维码已过期，请重新获取"));
        }
        if now < pending.next_poll_at {
            return Ok(QrPoll::waiting("等待扫码…"));
        }

        let body = json!({
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            "device_code": pending.device_code,
            "client_id": CLIENT_ID,
        });
        let (status, text, _request_id) = self
            .account_call(
                Method::POST,
                "/v1/auth/token",
                Some(&body),
                None,
                SDK_LOGIN,
                &[],
            )
            .await?;

        let parsed: Option<Value> = serde_json::from_str(&text).ok();
        let payload = parsed.as_ref().map(data_or_self);
        if (200..300).contains(&status) {
            if let Some(v) = payload {
                if let Some(access) = first_str(v, &["access_token"]) {
                    let expires_in = as_i64(v.get("expires_in").unwrap_or(&Value::Null))
                        .filter(|n| *n > 0)
                        .unwrap_or(7200);
                    let refresh = first_str(v, &["refresh_token"]).unwrap_or_default();
                    let sub = first_str(v, &["sub"]).unwrap_or_default();
                    self.session.save_login(
                        &access,
                        expires_in,
                        if refresh.is_empty() {
                            None
                        } else {
                            Some(&refresh)
                        },
                        if sub.is_empty() { None } else { Some(&sub) },
                    )?;
                    *lock(&self.qr) = None;
                    return Ok(QrPoll::success(if sub.is_empty() {
                        "登录成功".to_string()
                    } else {
                        format!("登录成功（用户 {sub}）")
                    }));
                }
            }
        }

        // OAuth 轮询错误：authorization_pending / slow_down / access_denied / expired_token。
        let err_code = payload
            .and_then(|v| first_str(v, &["error", "error_code", "code", "msg"]))
            .unwrap_or_else(|| format!("HTTP {status}"));
        let err_msg = payload
            .and_then(|v| first_str(v, &["error_description", "error_message", "msg"]))
            .unwrap_or_default();
        let interval = pending.interval_secs.max(1);
        match err_code.as_str() {
            "authorization_pending" => {
                update_next_poll(self, now + interval as i64);
                Ok(QrPoll::waiting("等待手机端确认…"))
            }
            "slow_down" => {
                let next = (interval + 5).min(30);
                update_pending(self, |p| {
                    p.interval_secs = next;
                    p.next_poll_at = now + next as i64;
                });
                Ok(QrPoll::waiting("轮询过快，已放慢"))
            }
            "access_denied" => {
                *lock(&self.qr) = None;
                Ok(QrPoll {
                    state: QrState::Canceled,
                    message: if err_msg.is_empty() {
                        "用户拒绝授权".to_string()
                    } else {
                        format!("用户拒绝授权：{err_msg}")
                    },
                })
            }
            "expired_token" => {
                *lock(&self.qr) = None;
                Ok(QrPoll::expired("设备码已过期，请重新获取"))
            }
            _ => {
                // 400/428/网络抖动都可能出现；保持轮询，但把服务端文案带给界面。
                update_next_poll(self, now + interval as i64);
                let msg = if err_msg.is_empty() {
                    format!("等待授权（{err_code}）")
                } else {
                    format!("等待授权（{err_msg}）")
                };
                Ok(QrPoll::waiting(msg))
            }
        }
    }

    /// 取消扫码（Drive::cancel_qr 的实现体）。
    pub(crate) async fn qr_cancel(&self) -> Result<()> {
        *lock(&self.qr) = None;
        Ok(())
    }

    /// 令牌导入（Drive::login_token 的实现体）。
    pub(crate) async fn login_with_token(&self, token: &str, extra: &str) -> Result<AccountInfo> {
        let (access, refresh, expires_in, sub) = parse_token_input(token, extra)?;
        self.session.save_login(
            &access,
            expires_in.unwrap_or(7200),
            if refresh.is_empty() {
                None
            } else {
                Some(&refresh)
            },
            if sub.is_empty() { None } else { Some(&sub) },
        )?;
        let result = finish_login(self, &sub).await;
        if let Err(e) = &result {
            if e.is_auth() {
                let _ = self.session.clear();
            }
        }
        result
    }

    /// 发送短信验证码（Drive::send_sms 的实现体）。
    pub(crate) async fn sms_send(&self, phone: &str) -> Result<String> {
        let phone = normalize_phone(phone);
        if phone.is_empty() {
            return Err(Error::other("手机号不能为空"));
        }

        // 第 1 步：盾初始化，取 captcha_token。
        let init_body = json!({
            "client_id": CLIENT_ID,
            "action": "POST:/v1/auth/verification",
            "device_id": self.device.device_id,
            "captcha_token": "",
            "meta": { "phone_number": phone },
        });
        let (status, text, request_id) = self
            .account_call(
                Method::POST,
                "/v1/shield/captcha/init",
                Some(&init_body),
                None,
                SDK_LOGIN,
                &[],
            )
            .await?;
        let value = parse_api_body(status, &text, request_id.as_deref(), &[])?;
        let captcha = first_str(data_or_self(&value), &["captcha_token"]).ok_or_else(|| {
            Error::server(
                status as i64,
                format!("盾初始化失败：{}", snippet(&text, 300)),
            )
        })?;

        // 第 2 步：发送短信。
        let send_body = json!({
            "phone_number": phone,
            "target": "ANY",
            "client_id": CLIENT_ID,
        });
        let (status, text, request_id) = self
            .account_call(
                Method::POST,
                "/v1/auth/verification",
                Some(&send_body),
                None,
                SDK_LOGIN,
                &[("x-captcha-token", &captcha)],
            )
            .await?;
        let value = parse_api_body(status, &text, request_id.as_deref(), &[])?;
        let d = data_or_self(&value);
        let verification_id =
            first_str(d, &["verification_id", "verificationId"]).ok_or_else(|| {
                Error::server(
                    status as i64,
                    format!("发送验证码响应异常：{}", snippet(&text, 300)),
                )
            })?;
        let is_user = d.get("is_user").and_then(Value::as_bool).unwrap_or(true);
        let expires_in = as_i64(d.get("expires_in").unwrap_or(&Value::Null))
            .filter(|n| *n > 0)
            .unwrap_or(300);
        *lock(&self.sms) = Some(SmsPending {
            phone: phone.clone(),
            verification_id,
            expires_at: now_ts() + expires_in.min(600),
        });

        Ok(if is_user {
            format!("验证码已发送到 {phone}（{expires_in} 秒内有效）")
        } else {
            format!("验证码已发送到 {phone}；该手机号未注册光鸭账号（仍可继续尝试登录）")
        })
    }

    /// 校验短信验证码并登录（Drive::verify_sms 的实现体）。
    pub(crate) async fn sms_verify(&self, phone: &str, code: &str) -> Result<AccountInfo> {
        let phone = normalize_phone(phone);
        let code = code.trim();
        if code.is_empty() {
            return Err(Error::other("验证码不能为空"));
        }
        let Some(pending) = lock(&self.sms).clone() else {
            return Err(Error::other("请先发送短信验证码"));
        };
        if pending.phone != phone {
            return Err(Error::other("手机号与已发送验证码的号码不一致"));
        }
        if now_ts() >= pending.expires_at {
            *lock(&self.sms) = None;
            return Err(Error::other("验证码已过期，请重新发送"));
        }

        // 第 3 步：校验验证码，换 verification_token。
        let verify_body = json!({
            "verification_id": pending.verification_id,
            "verification_code": code,
            "client_id": CLIENT_ID,
        });
        let (status, text, request_id) = self
            .account_call(
                Method::POST,
                "/v1/auth/verification/verify",
                Some(&verify_body),
                None,
                SDK_LOGIN,
                &[],
            )
            .await?;
        let value = parse_api_body(status, &text, request_id.as_deref(), &[])?;
        let verification_token = first_str(data_or_self(&value), &["verification_token"])
            .ok_or_else(|| {
                Error::server(
                    status as i64,
                    format!("验证码校验响应异常：{}", snippet(&text, 300)),
                )
            })?;

        // 第 4 步：登录换令牌。
        let signin_body = json!({
            "verification_code": code,
            "verification_token": verification_token,
            "username": phone,
            "client_id": CLIENT_ID,
        });
        let (status, text, signin_request_id) = self
            .account_call(
                Method::POST,
                "/v1/auth/signin",
                Some(&signin_body),
                None,
                SDK_LOGIN,
                &[],
            )
            .await?;
        let value = parse_api_body(status, &text, signin_request_id.as_deref(), &[])?;
        let d = data_or_self(&value);
        let access = first_str(d, &["access_token"]).ok_or_else(|| {
            Error::server(
                status as i64,
                format!("登录响应异常：{}", snippet(&text, 300)),
            )
        })?;
        let refresh = first_str(d, &["refresh_token"]).unwrap_or_default();
        let sub = first_str(d, &["sub"]).unwrap_or_default();
        let expires_in = as_i64(d.get("expires_in").unwrap_or(&Value::Null))
            .filter(|n| *n > 0)
            .unwrap_or(7200);
        self.session.save_login(
            &access,
            expires_in,
            if refresh.is_empty() {
                None
            } else {
                Some(&refresh)
            },
            if sub.is_empty() { None } else { Some(&sub) },
        )?;
        *lock(&self.sms) = None;
        let result = finish_login(self, &sub).await;
        if let Err(e) = &result {
            if e.is_auth() {
                let _ = self.session.clear();
            }
        }
        result
    }
}

// ==================== 工具 ====================

fn lock<T>(m: &std::sync::Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn update_next_poll(drive: &GuangyaDrive, when: i64) {
    update_pending(drive, |p| p.next_poll_at = when);
}

fn update_pending(drive: &GuangyaDrive, f: impl FnOnce(&mut QrPending)) {
    let mut guard = lock(&drive.qr);
    if let Some(p) = guard.as_mut() {
        f(p);
    }
}

/// 手机号规范化：`+86 13800138000`（与 Python UI 一致）。
pub(crate) fn normalize_phone(raw: &str) -> String {
    let s: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    if s.is_empty() {
        return String::new();
    }
    if let Some(rest) = s.strip_prefix("+86") {
        return format!("+86 {}", rest);
    }
    if s.starts_with('+') {
        return s;
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        if let Some(rest) = s.strip_prefix("86") {
            if rest.len() == 11 {
                return format!("+86 {rest}");
            }
        }
        return format!("+86 {s}");
    }
    s
}

/// 本地生成二维码 PNG（失败时只返回链接，不阻塞登录）。
fn qr_png(text: &str) -> Option<Vec<u8>> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let img = code.render::<image::Luma<u8>>().build();
    let mut buf: Vec<u8> = Vec::new();
    image::DynamicImage::ImageLuma8(img)
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
        .ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_normalization() {
        assert_eq!(normalize_phone("13800138000"), "+86 13800138000");
        assert_eq!(normalize_phone("+86 13800138000"), "+86 13800138000");
        assert_eq!(normalize_phone("+8613800138000"), "+86 13800138000");
        assert_eq!(normalize_phone("8613800138000"), "+86 13800138000");
        assert_eq!(normalize_phone("+1 555-0100"), "+15550100");
        assert_eq!(normalize_phone("   "), "");
    }

    #[test]
    fn account_mapping_with_assets() {
        let user = json!({
            "sub": "u-100",
            "nickName": "小鸭",
            "phone_number": "13800138000",
        });
        let assets = json!({
            "totalSpaceSize": 10_000_000_000u64,
            "usedSpaceSize": 2_500_000_000u64,
            "freeSpaceSize": 7_500_000_000u64,
            "svipStatus": 2,
            "svipExpireTime": 1800000000,
        });
        let a = account_from_values(&user, Some(&assets), "");
        assert_eq!(a.user_id, "u-100");
        assert_eq!(a.display_name, "小鸭");
        assert_eq!(a.user_name, "13800138000");
        assert_eq!(a.vip, "SVIP");
        let q = a.quota.unwrap();
        assert_eq!(q.total, 10_000_000_000);
        assert_eq!(q.used, 2_500_000_000);
        assert_eq!(q.free, 7_500_000_000);
        assert!(a.extra.contains_key("assets"));

        // 无资产/无昵称时仍能构造可登录账号
        let b = account_from_values(&json!({"sub":"u-2"}), None, "");
        assert_eq!(b.display_name, "u-2");
        assert!(b.quota.is_none());
        assert!(b.vip.is_empty());
    }

    #[test]
    fn token_input_variants() {
        let (a, r, e, s) = parse_token_input("access-A", "refresh-A").unwrap();
        assert_eq!(
            (a.as_str(), r.as_str(), e, s.as_str()),
            ("access-A", "refresh-A", None, "")
        );
        assert_eq!(
            parse_token_input("access-A", "refresh_token=refresh-B")
                .unwrap()
                .1,
            "refresh-B"
        );
        assert_eq!(
            parse_token_input("Bearer access-C", "").unwrap().0,
            "access-C"
        );

        let json_input =
            r#"{"access_token":"a1","refresh_token":"r1","expires_in":3600,"sub":"u9"}"#;
        let (a, r, e, s) = parse_token_input(json_input, "").unwrap();
        assert_eq!(
            (a.as_str(), r.as_str(), e, s.as_str()),
            ("a1", "r1", Some(3600), "u9")
        );

        let (a, r, e, s) =
            parse_token_input("a2", r#"{"refresh_token":"r2","sub":"u10"}"#).unwrap();
        assert_eq!(
            (a.as_str(), r.as_str(), e, s.as_str()),
            ("a2", "r2", None, "u10")
        );
        assert!(parse_token_input("", "").is_err());
        assert!(parse_token_input("{bad json", "").is_err());
    }

    #[test]
    fn qr_png_generated() {
        let png = qr_png("https://www.guangyapan.com/device?code=ABCD").unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "应生成合法 PNG");
        assert!(png.len() > 100);
    }
}
