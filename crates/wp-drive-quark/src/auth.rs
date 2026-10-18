//! 认证：本地渲染二维码登录、Cookie 手动登录、`member` 账号信息。
//!
//! 扫码流程（对齐 Python 版并保留其容错语义）：
//! 1. `GET uop.../getTokenForQrcodeLogin` → `data.members.token`
//! 2. 本地用 `qrcode` + `image` 渲染 PNG（内容是 `su.quark.cn/4_eMHBJ?...`）
//! 3. 轮询 `getServiceTicketByQrcodeToken`（GUI 每 2 秒调一次；总超时 300 秒）
//! 4. ticket → `pan.quark.cn/account/info?st=...&lw=scan`，手动跟随重定向收集 Set-Cookie
//! 5. 再请求一次 `pan.quark.cn/1/clouddrive/member` 补 `__puus`
//! 6. 校验 `__kps/__uid`，落盘 0600

use crate::session::{
    parse_cookie_expiry, set_cookie_name, Session, CRITICAL_KEYS, DEFAULT_TTL_SECS,
};
use crate::{
    as_i64, as_str, now_ts, QuarkDrive, ACCOUNT_INFO_URL, CAS_CLIENT_ID, CAS_STATUS_URL,
    CAS_TOKEN_URL, CAS_VERSION, QR_BASE_URL, QR_BIZ, QR_TIMEOUT_SECS, UA_LOGIN,
};
use image::{imageops::FilterType, DynamicImage, GrayImage, ImageFormat, Luma};
use qrcode::{EcLevel, QrCode};
use reqwest::header::{
    HeaderMap, ACCEPT, ACCEPT_LANGUAGE, CACHE_CONTROL, COOKIE, LOCATION, PRAGMA, REFERER,
    USER_AGENT,
};
use serde_json::Value;
use std::io::Cursor;
use std::time::Instant;
use wp_core::model::{AccountInfo, Quota};
use wp_core::{Error, Result};
use wp_drive::{QrChallenge, QrPoll};
use wp_net::CookieJar;

/// 一次扫码会话。
#[derive(Debug, Clone)]
pub struct QrSession {
    pub token: String,
    pub created: Instant,
}

impl QrSession {
    fn elapsed_secs(&self) -> u64 {
        self.created.elapsed().as_secs()
    }
}

fn url_with(base: &str, params: &[(&str, String)]) -> Result<String> {
    let mut url =
        url::Url::parse(base).map_err(|e| Error::other(format!("URL 解析失败（{base}）：{e}")))?;
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in params {
            q.append_pair(k, v);
        }
    }
    Ok(url.into())
}

/// 二维码内容 URL（顺序与 Python 版一致）。
pub(crate) fn build_qr_url(token: &str) -> Result<String> {
    url_with(
        QR_BASE_URL,
        &[
            ("token", token.to_string()),
            ("client_id", CAS_CLIENT_ID.to_string()),
            ("ssb", "weblogin".to_string()),
            ("uc_param_str", String::new()),
            ("uc_biz_str", QR_BIZ.to_string()),
        ],
    )
}

/// 本地渲染二维码 PNG（黑模块/白底，缩放到 300×300，与 Python UI 一致）。
pub(crate) fn render_qr_png(content: &str) -> Result<Vec<u8>> {
    let code = QrCode::with_error_correction_level(content.as_bytes(), EcLevel::M)
        .map_err(|e| Error::other(format!("二维码编码失败：{e}")))?;
    let width = code.width();
    let colors = code.to_colors();
    let border = 2usize;
    let scale = 10usize;
    let dim = (width + border * 2) * scale;
    let mut img: GrayImage = GrayImage::from_pixel(dim as u32, dim as u32, Luma([255u8]));
    for (i, color) in colors.iter().enumerate() {
        if matches!(color, qrcode::Color::Dark) {
            let x = i % width;
            let y = i / width;
            for dy in 0..scale {
                for dx in 0..scale {
                    let px = ((x + border) * scale + dx) as u32;
                    let py = ((y + border) * scale + dy) as u32;
                    img.put_pixel(px, py, Luma([0u8]));
                }
            }
        }
    }
    let img = image::imageops::resize(&img, 300, 300, FilterType::Nearest);
    let mut buf: Vec<u8> = Vec::new();
    DynamicImage::ImageLuma8(img)
        .write_to(Cursor::new(&mut buf), ImageFormat::Png)
        .map_err(|e| Error::other(format!("二维码 PNG 编码失败：{e}")))?;
    Ok(buf)
}

/// 解析手动粘贴的 Cookie 文本（换行/分号分隔，去掉两端引号）。
pub(crate) fn parse_cookie_text(text: &str) -> CookieJar {
    let mut jar = CookieJar::new();
    for part in text.replace('\n', ";").split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        let v = v.trim().trim_matches('"').trim_matches('\'');
        jar.set(k.to_string(), v.to_string());
    }
    jar
}

/// 把 `GET member` 的响应解析成统一账号模型。
pub(crate) fn parse_account_info(v: &Value) -> AccountInfo {
    let data = v.get("data").filter(|d| d.is_object()).unwrap_or(v);
    let total = crate::as_u64(data.get("total_capacity")).unwrap_or(0);
    let used = crate::as_u64(data.get("use_capacity")).unwrap_or(0);
    let quota = if total > 0 || used > 0 {
        Some(Quota {
            total,
            used,
            free: total.saturating_sub(used),
        })
    } else {
        None
    };

    let member_type = as_str(data.get("member_type")).unwrap_or_default();
    let mut vip = match member_type.as_str() {
        "SUPER_VIP" | "SVIP" => "超级会员".to_string(),
        "VIP" => "会员".to_string(),
        "" => String::new(),
        other => other.to_string(),
    };
    if let Some(exp) = as_i64(data.get("exp_at")) {
        if exp > 0 {
            let exp = if exp > 100_000_000_000 {
                exp / 1000
            } else {
                exp
            };
            if let Some(dt) = chrono::DateTime::from_timestamp(exp, 0) {
                let date = dt.with_timezone(&chrono::Local).format("%Y-%m-%d");
                if vip.is_empty() {
                    vip = format!("到期 {date}");
                } else {
                    vip = format!("{vip}（到期 {date}）");
                }
            }
        }
    }

    let member_info = data.get("member_info");
    let display_name = [
        data.get("nickname"),
        data.get("nick_name"),
        data.get("user_name"),
        data.get("name"),
        member_info.and_then(|m| m.get("nickname")),
        member_info.and_then(|m| m.get("name")),
    ]
    .into_iter()
    .flatten()
    .filter_map(|v| as_str(Some(v)))
    .find(|s| !s.trim().is_empty())
    .unwrap_or_else(|| "夸克网盘用户".to_string());

    let user_id = [
        data.get("user_id"),
        data.get("uid"),
        data.get("member_id"),
        data.get("user_info").and_then(|u| u.get("user_id")),
    ]
    .into_iter()
    .flatten()
    .filter_map(|v| as_str(Some(v)))
    .find(|s| !s.is_empty())
    .unwrap_or_default();

    let mut extra = serde_json::Map::new();
    for key in [
        "member_type",
        "exp_at",
        "subscribe_status",
        "total_capacity",
        "use_capacity",
        "secret_total_capacity",
        "deep_recycle_stat",
        "extend_capacity_composition",
    ] {
        if let Some(value) = data.get(key) {
            if !value.is_null() {
                extra.insert(key.to_string(), value.clone());
            }
        }
    }

    AccountInfo {
        display_name,
        user_name: String::new(),
        user_id,
        vip,
        quota,
        extra,
    }
}

fn login_headers(rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    rb.header(USER_AGENT, UA_LOGIN)
        .header(ACCEPT, "application/json, text/plain, */*")
        .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9")
        .header(CACHE_CONTROL, "no-cache")
        .header(PRAGMA, "no-cache")
        .header("origin", "https://pan.quark.cn")
        .header(REFERER, "https://pan.quark.cn/")
}

impl QuarkDrive {
    // ───────────────────── 扫码登录 ─────────────────────

    pub(crate) async fn begin_qr_login(&self) -> Result<QrChallenge> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let url = url_with(
            CAS_TOKEN_URL,
            &[
                ("client_id", CAS_CLIENT_ID.to_string()),
                ("v", CAS_VERSION.to_string()),
                ("request_id", request_id),
            ],
        )?;
        let resp = login_headers(self.login_http.get(&url))
            .send()
            .await
            .map_err(wp_net::client::net_err)?;
        let http_status = resp.status().as_u16();
        let text = resp.text().await.map_err(wp_net::client::net_err)?;
        if http_status != 200 {
            return Err(Error::server(
                http_status as i64,
                format!("请求二维码 token 失败：HTTP {http_status}"),
            ));
        }
        let value: Value = serde_json::from_str(&text).map_err(|e| {
            Error::other(format!(
                "二维码 token 响应不是合法 JSON：{e}；{}",
                text.chars().take(200).collect::<String>()
            ))
        })?;
        let code = as_i64(value.get("status")).unwrap_or(-1);
        if code != 2_000_000 {
            let msg = as_str(value.get("message"))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| crate::compact_json(&value));
            return Err(Error::server(code, format!("获取二维码 token 失败：{msg}")));
        }
        let token = value
            .get("data")
            .and_then(|d| d.get("members"))
            .and_then(|m| m.get("token"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if token.is_empty() {
            return Err(Error::other("二维码 token 响应中未找到 data.members.token"));
        }
        let qr_url = build_qr_url(&token)?;
        let png = render_qr_png(&qr_url)?;
        *self.qr.lock().unwrap_or_else(|e| e.into_inner()) = Some(QrSession {
            token,
            created: Instant::now(),
        });
        wp_core::log_info!("夸克", "已生成扫码登录二维码（有效期约 5 分钟）");
        Ok(QrChallenge {
            image_png: Some(png),
            url: Some(qr_url),
            code: None,
            hint: "请使用夸克 App 扫码并在手机上确认（约 5 分钟内有效）".into(),
            expires_in_secs: QR_TIMEOUT_SECS,
        })
    }

    fn clear_qr(&self) {
        *self.qr.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub(crate) fn cancel_qr_login(&self) {
        self.clear_qr();
    }

    pub(crate) async fn poll_qr_login(&self) -> Result<QrPoll> {
        let session = self.qr.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(session) = session else {
            return Err(Error::other("没有进行中的扫码会话，请先调用 begin_qr"));
        };
        if session.elapsed_secs() >= QR_TIMEOUT_SECS {
            self.clear_qr();
            return Ok(QrPoll::expired(format!(
                "扫码超时（{QR_TIMEOUT_SECS} 秒），请重新获取二维码"
            )));
        }

        let request_id = uuid::Uuid::new_v4().to_string();
        let url = url_with(
            CAS_STATUS_URL,
            &[
                ("client_id", CAS_CLIENT_ID.to_string()),
                ("v", CAS_VERSION.to_string()),
                ("token", session.token.clone()),
                ("request_id", request_id),
            ],
        )?;
        let resp = match login_headers(self.login_http.get(&url)).send().await {
            Ok(r) => r,
            Err(e) => {
                // 源码语义：网络异常当「等待」，继续轮询到 300 秒超时。
                return Ok(QrPoll::waiting(format!(
                    "查询扫码状态异常：{}",
                    wp_net::client::net_err(e)
                )));
            }
        };
        let http_status = resp.status().as_u16();
        if http_status != 200 {
            return Ok(QrPoll::waiting(format!("HTTP {http_status}")));
        }
        let text = match resp.text().await {
            Ok(t) => t,
            Err(_) => return Ok(QrPoll::waiting("响应读取失败")),
        };
        let value: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => return Ok(QrPoll::waiting("响应非 JSON")),
        };
        let code = as_i64(value.get("status")).unwrap_or(-1);
        let message = as_str(value.get("message")).unwrap_or_default();
        let ticket = value
            .get("data")
            .and_then(|d| d.get("members"))
            .and_then(|m| m.get("service_ticket"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        if code == 2_000_000 && !ticket.is_empty() {
            if let Err(e) = self.exchange_ticket(&ticket).await {
                self.clear_qr();
                return Err(e);
            }
            self.clear_qr();
            wp_core::log_info!("夸克", "扫码登录成功，Cookie 已保存");
            return Ok(QrPoll::success("登录成功"));
        }

        let lower = message.to_lowercase();
        let failed = matches!(code, 5_000_4002 | 5_000_4003 | 5_000_4004)
            || ["expired", "failed", "error", "timeout", "invalid"]
                .iter()
                .any(|k| lower.contains(k));
        if failed {
            self.clear_qr();
            return Ok(QrPoll::expired(format!(
                "扫码失败：{}",
                if message.is_empty() {
                    "服务端判定失败"
                } else {
                    message.as_str()
                }
            )));
        }

        let msg = if message.is_empty() {
            format!("等待扫码…（已等待 {}s）", session.elapsed_secs())
        } else {
            format!("{}（已等待 {}s）", message, session.elapsed_secs())
        };
        Ok(QrPoll::waiting(msg))
    }

    async fn exchange_ticket(&self, ticket: &str) -> Result<()> {
        let mut jar = CookieJar::new();
        let url = url_with(
            ACCOUNT_INFO_URL,
            &[("st", ticket.to_string()), ("lw", "scan".to_string())],
        )?;
        let (status, _body, exp1) = self.browser_get_collect(&url, &mut jar).await?;
        if status != 200 {
            return Err(Error::auth(format!("换取 Cookie 失败：HTTP {status}")));
        }
        let mut min_expiry = exp1;

        // CAS 扫码不下发 __puus；再请求一次 member 让服务端补上。
        let member_url = url_with(
            "https://pan.quark.cn/1/clouddrive/member",
            &[("pr", "ucpro".to_string()), ("fr", "pc".to_string())],
        )?;
        match self.browser_get_collect(&member_url, &mut jar).await {
            Ok((_, _, exp2)) => {
                if let Some(exp) = exp2 {
                    min_expiry = Some(min_expiry.map_or(exp, |m| m.min(exp)));
                }
            }
            Err(e) => {
                wp_core::log_warn!(
                    "夸克",
                    "扫码后补全 __puus 失败（下载可能受影响）：{}",
                    wp_core::error::err_line(&e)
                );
            }
        }

        let now = now_ts();
        let expires_at = min_expiry
            .filter(|e| *e > now)
            .unwrap_or(now + DEFAULT_TTL_SECS);
        let mut probe = Session {
            jar: jar.clone(),
            source: "qrcode".into(),
            saved_at: now,
            expires_at,
            file_mtime: None,
        };
        if let Err(reason) = probe.validate() {
            let names: Vec<String> = jar.map.keys().take(8).cloned().collect();
            return Err(Error::auth(format!(
                "服务端未下发有效 Cookie（{reason}）；实际收到 {} 个：{}",
                jar.len(),
                names.join("、")
            )));
        }
        if !probe.has_puus() {
            wp_core::log_warn!(
                "夸克",
                "扫码登录未获取到 __puus —— 上传可用，但下载可能被 CDN 拒绝；\
                 建议改用浏览器 Cookie 手动登录"
            );
        }
        probe.expires_at = expires_at;
        self.api.replace_credentials(jar, "qrcode", expires_at)?;
        self.invalidate_cache();
        Ok(())
    }

    // ───────────────────── 手动 Cookie ─────────────────────

    pub(crate) async fn login_with_cookie(&self, text: &str) -> Result<AccountInfo> {
        let input = parse_cookie_text(text);
        let now = now_ts();
        let mut probe = Session {
            jar: input.clone(),
            source: "manual".into(),
            saved_at: now,
            expires_at: now + DEFAULT_TTL_SECS,
            file_mtime: None,
        };
        if let Err(reason) = probe.validate() {
            return Err(Error::auth(format!("Cookie 校验失败：{reason}")));
        }

        // 与旧 Cookie 合并，允许用户只粘贴变化的部分（如只补 __puus）。
        let old = self.api.snapshot();
        let mut merged = old.jar;
        merged.merge(&input);
        probe.jar = merged;
        if let Err(reason) = probe.validate() {
            return Err(Error::auth(format!("Cookie 校验失败：{reason}")));
        }

        let temp_api = self.api.clone_with_session(probe.clone());
        let member = match temp_api.member(false).await {
            Ok(v) => v,
            Err(e) if e.is_auth() => {
                return Err(Error::auth(format!(
                    "Cookie 字段齐全（含 __kps/__uid），但服务端校验未通过：{}",
                    wp_core::error::err_line(&e)
                )));
            }
            Err(e) => return Err(e),
        };
        // 验证请求本身可能触发 Set-Cookie（__puus）；若仍缺失再补一次 member。
        if !temp_api.has_puus() {
            let _ = temp_api.refresh_puus(false).await;
        }
        let final_jar = temp_api.snapshot().jar;
        self.api
            .replace_credentials(final_jar, "manual", now + DEFAULT_TTL_SECS)?;
        self.invalidate_cache();
        wp_core::log_info!(
            "夸克",
            "手动 Cookie 登录成功（{} 个字段，__puus={}）",
            self.api.snapshot().cookie_count(),
            if self.api.has_puus() { "有" } else { "无" }
        );
        Ok(parse_account_info(&member))
    }

    // ───────────────────── 账号 / 退出 ─────────────────────

    pub(crate) async fn fetch_account(&self) -> Result<AccountInfo> {
        self.api.ensure_valid()?;
        let value = self.api.member(false).await?;
        Ok(parse_account_info(&value))
    }

    pub(crate) async fn logout_account(&self) -> Result<()> {
        self.clear_qr();
        self.api.logout()?;
        self.invalidate_cache();
        wp_core::log_info!("夸克", "已退出登录并删除本地凭证");
        Ok(())
    }

    // ───────────────────── 手写重定向（收集 Cookie） ─────────────────────

    /// 手动跟随重定向，逐跳合并 `Set-Cookie`，供扫码换票使用。
    async fn browser_get_collect(
        &self,
        url: &str,
        jar: &mut CookieJar,
    ) -> Result<(u16, String, Option<i64>)> {
        let mut current = url.to_string();
        let mut min_expiry: Option<i64> = None;
        for _ in 0..10 {
            let cookie = jar.header();
            let mut rb = login_headers(self.login_http.get(&current));
            if !cookie.is_empty() {
                rb = rb.header(COOKIE, cookie);
            }
            let resp = rb.send().await.map_err(wp_net::client::net_err)?;
            Self::absorb_cookies(resp.headers(), jar, &mut min_expiry);
            let status = resp.status().as_u16();
            if (300..400).contains(&status) {
                let Some(loc) = resp.headers().get(LOCATION).and_then(|v| v.to_str().ok()) else {
                    return Ok((status, String::new(), min_expiry));
                };
                let base = url::Url::parse(&current)
                    .map_err(|e| Error::other(format!("URL 解析失败：{e}")))?;
                current = base
                    .join(loc)
                    .map_err(|e| Error::other(format!("重定向地址解析失败：{e}")))?
                    .to_string();
                continue;
            }
            let text = resp.text().await.map_err(wp_net::client::net_err)?;
            return Ok((status, text, min_expiry));
        }
        Err(Error::network("登录重定向次数过多（>10）"))
    }

    fn absorb_cookies(headers: &HeaderMap, jar: &mut CookieJar, min_expiry: &mut Option<i64>) {
        for hv in headers.get_all(reqwest::header::SET_COOKIE).iter() {
            let Ok(text) = hv.to_str() else { continue };
            jar.apply_set_cookie(text);
            if let Some(name) = set_cookie_name(text) {
                if CRITICAL_KEYS.contains(&name.as_str()) {
                    if let Some(exp) = parse_cookie_expiry(text) {
                        *min_expiry = Some(min_expiry.map_or(exp, |m| m.min(exp)));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn qr_url_is_percent_encoded() {
        let url = build_qr_url("tok+/=").unwrap();
        assert!(url.starts_with("https://su.quark.cn/4_eMHBJ?"));
        assert!(url.contains("token=tok%2B%2F%3D"));
        assert!(url.contains("client_id=532"));
        assert!(url.contains("ssb=weblogin"));
        assert!(url.contains("uc_param_str="));
        assert!(url.contains("uc_biz_str=S%3Acustom%7COPT%3ASAREA%400%7COPT%3AIMMERSIVE%401%7COPT%3ABACK_BTN_STYLE%400"));
        let parsed = url::Url::parse(&url).unwrap();
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("uc_biz_str").map(String::as_str), Some(QR_BIZ));
        assert_eq!(pairs.get("token").map(String::as_str), Some("tok+/="));
    }

    #[test]
    fn qr_png_renders_300px_png() {
        let url = build_qr_url("abc123").unwrap();
        let png = render_qr_png(&url).unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]));
        let img = image::load_from_memory(&png).unwrap();
        assert_eq!(img.width(), 300);
        assert_eq!(img.height(), 300);
    }

    #[test]
    fn cookie_text_parser() {
        let jar = parse_cookie_text("__kps=abc; __uid='xyz'\n__puus=\"p+q/=\" ; ctoken=1; broken");
        assert_eq!(jar.get("__kps"), Some("abc"));
        assert_eq!(jar.get("__uid"), Some("xyz"));
        assert_eq!(jar.get("__puus"), Some("p+q/="));
        assert_eq!(jar.get("ctoken"), Some("1"));
        assert_eq!(jar.len(), 4);
    }

    #[test]
    fn account_parser_quota_and_vip() {
        let v = json!({
            "status": 200,
            "data": {
                "total_capacity": "1099511627776",
                "use_capacity": 1024,
                "member_type": "SUPER_VIP",
                "exp_at": 1789322627000i64,
                "subscribe_status": 1,
                "nickname": "夸克用户",
                "user_id": "u-1"
            }
        });
        let info = parse_account_info(&v);
        assert_eq!(info.display_name, "夸克用户");
        assert_eq!(info.user_id, "u-1");
        assert!(info.vip.starts_with("超级会员"));
        let q = info.quota.unwrap();
        assert_eq!(q.total, 1_099_511_627_776);
        assert_eq!(q.used, 1024);
        assert_eq!(q.free, q.total - 1024);
        assert_eq!(info.extra.get("member_type").unwrap(), "SUPER_VIP");
    }

    #[test]
    fn account_parser_without_quota() {
        let info = parse_account_info(&json!({"data": {"member_type": "VIP"}}));
        assert_eq!(info.display_name, "夸克网盘用户");
        assert_eq!(info.vip, "会员");
        assert!(info.quota.is_none());
    }
}
