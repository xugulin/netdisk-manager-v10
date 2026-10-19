//! 认证：扫码登录、Cookie 登录、会话校验/自愈/登出、账号信息。
//!
//! 扫码登录的实测要点（文档 §3.1-3.6）：
//! 1. `getqrcode` 返回 `sign` 和**省略 scheme** 的 `imgurl`，没有 server 端
//!    base64 二维码；需要客户端下载 PNG。
//! 2. `unicast` 的 `channel_id` 就是 `sign`；未扫码约 30s 返回 `errno:1`，
//!    长轮询读超时必须当“本轮无事件”。
//! 3. **确认判据是“有 32 位 hex 令牌”**，不是 `status==2`：真机报文是
//!    `status=0` + `v`（状态会从 1 回退到 0）。
//! 4. 换取 BDUSS 后必须用 **pan 域** 客户端取 bdstoken/uk；子域种植失败
//!    不影响登录，且**不要重试**。

use crate::files;
use crate::http::ReqOpts;
use crate::session::{QrSession, Session};
use crate::{lock, map_errno, now_ms, BaiduDrive, PASSPORT, QR_TTL_SECS};
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, REFERER, SET_COOKIE};
use serde_json::{json, Value};
use std::time::Duration;
use wp_core::model::{AccountInfo, QrState, Quota};
use wp_core::util::now_ts;
use wp_core::{Error, Result};
use wp_drive::{QrChallenge, QrPoll};
use wp_net::crypto::url_encode;

const PAN_MAIN: &str = "https://pan.baidu.com/disk/main";

fn passport_params() -> Vec<(String, String)> {
    vec![
        ("apiver".into(), "v3".into()),
        ("tpl".into(), "netdisk".into()),
        ("tt".into(), now_ms().to_string()),
    ]
}

/// 去掉 JSONP 外壳 `callback({...})`。
pub(crate) fn strip_jsonp(text: &str) -> String {
    let s = text.trim();
    if s.is_empty() || s.starts_with('{') || s.starts_with('[') {
        return s.to_string();
    }
    if let (Some(open), Some(close)) = (s.find('('), s.rfind(')')) {
        if close > open {
            return s[open + 1..close].trim().to_string();
        }
    }
    s.to_string()
}

/// 补全 scheme：文档实测 `imgurl` 形如 `passport.baidu.com/...`。
pub(crate) fn absolute_url(raw: &str) -> String {
    let raw = raw.trim();
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return raw.to_string();
    }
    if let Some(rest) = raw.strip_prefix("//") {
        return format!("https://{rest}");
    }
    format!("https://{}", raw.trim_start_matches('/'))
}

/// 从 URL query 取 `sign`（不依赖 URL 编码细节，失败再手工切）。
pub(crate) fn query_param(url: &str, name: &str) -> Option<String> {
    if let Ok(parsed) = url::Url::parse(url) {
        for (k, v) in parsed.query_pairs() {
            if k == name {
                return Some(v.into_owned());
            }
        }
        return None;
    }
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == name {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn parse_passport_json(text: &str) -> Result<Value> {
    let cleaned = strip_jsonp(text);
    if cleaned.trim().is_empty() {
        return Err(Error::server(-1, "passport 响应为空"));
    }
    serde_json::from_str(&cleaned).map_err(|e| {
        let snippet: String = cleaned.chars().take(300).collect();
        Error::server(-1, format!("passport 响应不是合法 JSON（{e}）：{snippet}"))
    })
}

fn is_hex32(s: &str) -> bool {
    s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// 从 unicast 报文里取登录令牌（32 位 hex）。
///
/// 已知官方位置是 `channel_v.v`；按“已知字段 → 全文兜底”两级查找。
pub(crate) fn extract_login_token(outer: &Value, inner: &Value) -> Option<String> {
    for container in [inner, outer] {
        if let Some(obj) = container.as_object() {
            for key in ["v", "bduss", "token", "bduss_token", "channel_v"] {
                if let Some(s) = obj.get(key).and_then(Value::as_str) {
                    if is_hex32(s) {
                        return Some(s.to_string());
                    }
                }
            }
        }
    }
    // 兜底：任意 32 位 hex，排除已知非令牌字段。
    for container in [inner, outer] {
        if let Some(obj) = container.as_object() {
            for (k, v) in obj {
                if matches!(k.as_str(), "channel_id" | "sign" | "gid") {
                    continue;
                }
                if let Some(s) = v.as_str() {
                    if is_hex32(s) {
                        wp_core::log_debug!("百度", "登录令牌取自兜底字段 {k}");
                        return Some(s.to_string());
                    }
                }
            }
        }
    }
    None
}

/// 解析一次 unicast 报文 → (显示状态, 登录令牌)。
pub(crate) fn parse_unicast(v: &Value) -> (i64, Option<String>) {
    let inner = match v.get("channel_v") {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({})),
        Some(value @ Value::Object(_)) => value.clone(),
        _ => json!({}),
    };
    let status = inner
        .get("status")
        .and_then(Value::as_i64)
        .or_else(|| v.get("status").and_then(Value::as_i64))
        .unwrap_or(0);
    let token = extract_login_token(v, &inner);
    (status, token)
}

/// 开始扫码登录。
pub(crate) async fn begin_qr(d: &BaiduDrive) -> Result<QrChallenge> {
    let gid = uuid::Uuid::new_v4().to_string().to_uppercase();
    let mut params = vec![
        ("lp".to_string(), "pc".to_string()),
        ("qrloginfrom".to_string(), "pc".to_string()),
        ("gid".to_string(), gid.clone()),
        (
            "logPage".to_string(),
            "traceId:pc_loginv5,logPage:loginv5".to_string(),
        ),
    ];
    params.extend(passport_params());

    let text = d
        .passport_get_text("/v2/api/getqrcode", &params, Duration::from_secs(20), true)
        .await?;
    let v = parse_passport_json(&text)?;
    if let Some(code) = v.get("errno").and_then(Value::as_i64) {
        if code != 0 {
            return Err(map_errno(code, &v));
        }
    }

    let raw_img = v.get("imgurl").and_then(Value::as_str).unwrap_or("").trim();
    let img_url = if raw_img.is_empty() {
        String::new()
    } else {
        absolute_url(raw_img)
    };
    let sign = query_param(&img_url, "sign")
        .filter(|s| !s.is_empty())
        .or_else(|| {
            v.get("sign")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .ok_or_else(|| Error::server(-1, "getqrcode 未返回 sign，无法开始扫码登录"))?;

    let image_url = if img_url.is_empty() {
        format!("{PASSPORT}/v2/api/qrcode?sign={sign}&lp=pc&qrloginfrom=pc")
    } else {
        img_url
    };

    let mut image = match d
        .passport_get_bytes(&image_url, Duration::from_secs(20))
        .await
    {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            wp_core::log_warn!("百度", "二维码图片下载失败，尝试兜底地址：{e}");
            None
        }
    };
    if image.is_none() {
        let fallback = format!("{PASSPORT}/v2/api/qrcode?sign={sign}&lp=pc&qrloginfrom=pc");
        match d
            .passport_get_bytes(&fallback, Duration::from_secs(20))
            .await
        {
            Ok(bytes) => image = Some(bytes),
            Err(e) => wp_core::log_warn!("百度", "二维码兜底下载也失败：{e}"),
        }
    }
    if let Some(bytes) = &image {
        let png = bytes.starts_with(b"\x89PNG");
        wp_core::log_info!(
            "百度",
            "已获取二维码：{} 字节{}",
            bytes.len(),
            if png {
                ""
            } else {
                "（非 PNG 魔数，仍交给界面尝试）"
            }
        );
    }

    *lock(&d.qr) = Some(QrSession::new(sign.clone(), gid));
    Ok(QrChallenge {
        image_png: image,
        url: Some(image_url),
        code: Some(sign),
        hint: "请用「百度网盘」App 扫描二维码，并在手机端点击确认登录".into(),
        expires_in_secs: QR_TTL_SECS,
    })
}

/// 轮询扫码状态（一次调用只查一次；GUI 定时循环）。
pub(crate) async fn poll_qr(d: &BaiduDrive) -> Result<QrPoll> {
    let snapshot = {
        let guard = lock(&d.qr);
        guard
            .as_ref()
            .map(|q| (q.sign.clone(), q.gid.clone(), q.started))
    };
    let Some((sign, gid, started)) = snapshot else {
        return Err(Error::auth("尚未开始扫码登录，请先获取二维码"));
    };

    if started.elapsed().as_secs() >= QR_TTL_SECS {
        *lock(&d.qr) = None;
        return Ok(QrPoll::expired(
            "二维码已过期，请关闭窗口后重新发起扫码登录",
        ));
    }

    let mut params = vec![
        ("channel_id".to_string(), sign.clone()),
        ("gid".to_string(), gid),
        ("_sdkFrom".to_string(), "1".to_string()),
    ];
    params.extend(passport_params());

    let fut = d.passport_get_text("/channel/unicast", &params, Duration::from_secs(60), true);
    let text = match tokio::time::timeout(Duration::from_secs(60), fut).await {
        Err(_) => {
            wp_core::log_debug!("百度", "unicast 长轮询超时，按未扫码继续");
            return Ok(QrPoll::waiting("等待扫码（长轮询超时，继续等待）"));
        }
        Ok(Err(Error::Network(msg))) if msg.contains("超时") => {
            wp_core::log_debug!("百度", "unicast 读超时，按未扫码继续");
            return Ok(QrPoll::waiting("等待扫码（读超时，继续等待）"));
        }
        Ok(Err(e)) => return Err(e),
        Ok(Ok(text)) => text,
    };

    let cleaned = strip_jsonp(&text);
    if cleaned.trim().is_empty() {
        return Ok(QrPoll::waiting("等待扫码"));
    }
    let v = match serde_json::from_str::<Value>(&cleaned) {
        Ok(v) => v,
        Err(e) => {
            wp_core::log_debug!("百度", "unicast 响应无法解析（{e}），按未扫码继续");
            return Ok(QrPoll::waiting("等待扫码"));
        }
    };

    let (status, token) = parse_unicast(&v);
    if let Some(token) = token {
        match exchange_token(d, &token).await {
            Ok(()) => {
                *lock(&d.qr) = None;
                wp_core::log_info!("百度", "扫码登录成功，已建立会话");
                return Ok(QrPoll::success("登录成功，正在建立网盘会话…"));
            }
            Err(e) => {
                *lock(&d.qr) = None;
                return Err(e);
            }
        }
    }

    let shown = if status == 1 {
        QrState::Scanned
    } else {
        QrState::Waiting
    };
    {
        let mut guard = lock(&d.qr);
        if let Some(q) = guard.as_mut() {
            if q.sign == sign && q.last_state != shown {
                wp_core::log_info!("百度", "扫码状态：{:?} → {:?}", q.last_state, shown);
                q.last_state = shown;
            }
        }
    }
    Ok(match shown {
        QrState::Scanned => QrPoll::scanned("已扫码，请在手机上点击「确认登录」"),
        _ => QrPoll::waiting("请用「百度网盘」App 扫描二维码"),
    })
}

/// 取消/清空扫码会话。
pub(crate) fn cancel_qr(d: &BaiduDrive) {
    *lock(&d.qr) = None;
}

/// 用扫码令牌换取 BDUSS/STOKEN，并建立 pan 域会话。
async fn exchange_token(d: &BaiduDrive, login_token: &str) -> Result<()> {
    let mut params = vec![
        ("bduss".to_string(), login_token.to_string()),
        ("u".to_string(), PAN_MAIN.to_string()),
        ("loginVersion".to_string(), "v5".to_string()),
        ("qrcode".to_string(), "1".to_string()),
        ("time".to_string(), now_ts().to_string()),
    ];
    params.extend(passport_params());

    let url = format!("{PASSPORT}/v3/login/main/qrbdusslogin");
    let resp = d
        .http
        .passport
        .get(&url)
        .query(&params)
        .header(ACCEPT, "application/json, text/plain, */*")
        .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9")
        .header(REFERER, "https://pan.baidu.com/")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(wp_net::client::net_err)?;

    let status = resp.status();
    let set_cookies: Vec<String> = resp
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::to_string)
        .collect();

    let mut new_session = Session::default();
    for cookie in &set_cookies {
        new_session.apply_set_cookie(cookie);
    }

    if !new_session.valid() {
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(300).collect();
        return Err(Error::auth(format!(
            "扫码已确认，但换取 BDUSS 失败（HTTP {status}）：{}",
            if snippet.trim().is_empty() {
                "响应未下发 BDUSS/STOKEN"
            } else {
                snippet.trim()
            }
        )));
    }

    new_session.updated = now_ts();
    wp_core::log_info!("百度", "已取得扫码会话：{}", new_session.masked_summary());
    d.set_session(new_session)?;

    // ⑤ 建立 pan 域会话（失败只告警）
    establish_pan_session(d).await;
    // ⑥ 子域种植（尽力而为，禁止重试）
    plant_subdomains(d).await;
    // 登录后主动补 bdstoken/uk（必须用 pan 域客户端）
    if let Err(e) = d.refresh_template().await {
        wp_core::log_warn!("百度", "登录后刷新 bdstoken/uk 失败（下次请求会再试）：{e}");
    }
    Ok(())
}

/// 建立 pan 域会话：**手动跟随 302 并收集每一跳的 Set-Cookie**。
///
/// 为什么不能交给 reqwest 自动跟随：pan 域的 STOKEN 只会出现在重定向链的
/// `Set-Cookie` 里，自动跟随会丢掉中间响应头。不抓到它，会话里留的是
/// passport 域那份 STOKEN —— 读接口能过，写接口恒 `errno:-6`（V2 源码
/// `桥/后端_百度.py:43` 的真机结论）。
async fn establish_pan_session(d: &BaiduDrive) {
    let mut url = format!(
        "{PASSPORT}/v3/login/api/auth/?return_type=5&tpl=netdisk&u={}",
        url_encode(PAN_MAIN)
    );
    let mut set_cookies: Vec<String> = Vec::new();
    // pan 域 STOKEN 的权威来源：302 Location 里的 `stoken=<128字符>` 查询参数。
    // 真机 HAR 实测：passport 域 STOKEN 是 64 字符、只能读；pan 域是 128 字符，
    // 写接口（/api/create、上传）必须同时带 BDUSS + 这个 STOKEN。
    let mut pan_stoken: Option<String> = None;
    for hop in 0..6 {
        let cookie = d.cookie_header();
        let mut req = d
            .http
            .passport
            .get(&url)
            .header(REFERER, "https://pan.baidu.com/")
            .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9")
            .timeout(Duration::from_secs(20));
        if !cookie.is_empty() {
            req = req.header(reqwest::header::COOKIE, cookie);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                wp_core::log_warn!("百度", "pan 域会话建立失败（不影响登录）：{e}");
                return;
            }
        };
        let status = resp.status();
        for raw in resp
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
        {
            set_cookies.push(raw.to_string());
        }
        wp_core::log_debug!("百度", "pan 域会话 第 {hop} 跳 → HTTP {}", status.as_u16());
        if !status.is_redirection() {
            break;
        }
        let Some(loc) = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
        else {
            break;
        };
        // 从跳转地址里提取 pan 域 STOKEN（只认 128 字符以上的那份）
        if pan_stoken.is_none() {
            if let Ok(target) = url::Url::parse(&url).and_then(|base| base.join(loc)) {
                for (k, v) in target.query_pairs() {
                    if k == "stoken" && v.len() > 64 {
                        pan_stoken = Some(v.to_string());
                        break;
                    }
                }
            }
        }
        url = match url::Url::parse(&url)
            .ok()
            .and_then(|base| base.join(loc).ok())
        {
            Some(u) => u.to_string(),
            None => loc.to_string(),
        };
    }
    if set_cookies.is_empty() {
        wp_core::log_warn!("百度", "pan 域会话未收到任何 Set-Cookie（写操作可能只读）");
        return;
    }
    let captured = set_cookies.len();
    let mut names: Vec<String> = Vec::new();
    let mut old_len = 0usize;
    d.mutate_session(|s| {
        old_len = s.stoken.len();
        for sc in &set_cookies {
            if let Some(name) = s.apply_set_cookie(sc) {
                names.push(name.to_string());
            }
        }
        if let Some(st) = &pan_stoken {
            s.stoken = st.clone();
        }
    });
    wp_core::log_info!(
        "百度",
        "pan 域会话建立完成：STOKEN {} → {}（{} 个 Set-Cookie；捕获 {}）",
        old_len,
        d.session_snapshot().stoken.len(),
        captured,
        names.join("/")
    );
}

async fn plant_subdomains(d: &BaiduDrive) {
    let targets = [
        "https://pcs.baidu.com/rest/2.0/pcs/file?method=plantcookie&source=pcs",
        "https://pcsdata.baidu.com/rest/2.0/pcs/file?method=plantcookie&source=pcs",
    ];
    for target in targets {
        let params = vec![
            ("return_type".to_string(), "3".to_string()),
            ("tpl".to_string(), "netdisk".to_string()),
            ("u".to_string(), target.to_string()),
        ];
        let url = format!("{PASSPORT}/v3/login/api/auth");
        let cookie = d.cookie_header();
        let mut req = d
            .http
            .passport_follow
            .get(&url)
            .query(&params)
            .header(REFERER, "https://pan.baidu.com/")
            .timeout(Duration::from_secs(20));
        if !cookie.is_empty() {
            req = req.header(reqwest::header::COOKIE, cookie);
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                wp_core::log_debug!("百度", "子域种植 {target} → HTTP {status}");
            }
            Err(e) => {
                // 实测会被 wappass 打回 Unsupported open api；不重试。
                wp_core::log_debug!("百度", "子域种植 {target} 失败（忽略）：{e}");
            }
        }
    }
}

/// 手工 Cookie 登录（最小集 BDUSS + STOKEN）。
pub(crate) async fn login_cookie(d: &BaiduDrive, text: &str) -> Result<AccountInfo> {
    let mut session = d.session_snapshot();
    let hits = session.apply_cookie_text(text).map_err(Error::auth)?;
    if !session.valid() {
        return Err(Error::auth(
            "至少需要 BDUSS（或 BDUSS_BFESS）与 STOKEN；推荐三者都带上",
        ));
    }
    session.updated = now_ts();
    d.set_session(session)?;
    wp_core::log_info!("百度", "已导入 Cookie：{}", hits.join("、"));
    account(d).await
}

/// 登出：只清本地凭证，不动云端。
pub(crate) fn logout(d: &BaiduDrive) -> Result<()> {
    *lock(&d.session) = Session::default();
    Session::remove(&d.data_dir).map_err(Error::Io)?;
    *lock(&d.qr) = None;
    d.clear_dir_cache();
    wp_core::log_info!("百度", "已退出登录，本地凭证已清除");
    Ok(())
}

/// 账号信息：校验会话 → 刷新 bdstoken/uk → getinfo + quota。
pub(crate) async fn account(d: &BaiduDrive) -> Result<AccountInfo> {
    if !d.session_valid() {
        return Err(Error::auth("未登录：缺少 BDUSS/STOKEN"));
    }
    // 同时充当会话有效性校验；双通道（模板变量 → loginStatus）都失败才告警。
    if let Err(e) = d.refresh_template().await {
        wp_core::log_warn!("百度", "会话刷新失败，仍尝试读取账号信息：{e}");
    }

    let uk = d
        .uk_value()
        .ok_or_else(|| Error::auth("会话缺少 uk，无法查询用户信息"))?;
    let params = vec![
        ("need_boardinfo".to_string(), "1".to_string()),
        ("user_list".to_string(), format!("[{uk}]")),
    ];
    let v = d
        .api_get("/api/user/getinfo", &params, ReqOpts::authed())
        .await?;
    let record = v
        .get("records")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or_else(|| {
            Error::server(
                -1,
                "user/getinfo 响应缺少 records[0]（检查 need_boardinfo/user_list 参数）",
            )
        })?;

    let display_name = record
        .get("uname")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| record.get("username").and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string();
    let user_name = record
        .get("username")
        .and_then(Value::as_str)
        .or_else(|| record.get("uname").and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string();
    let vip = vip_text(record);

    let mut extra = serde_json::Map::new();
    for key in [
        "avatar_url",
        "vip_type",
        "vip_level",
        "priority_name",
        "isdocuser",
    ] {
        if let Some(value) = record.get(key) {
            extra.insert(key.to_string(), value.clone());
        }
    }
    if let Some(user_uk) = record.get("uk") {
        extra.insert("uk".to_string(), user_uk.clone());
    }
    // 写会话能力：false 表示「已登录但只读」（读接口正常、写接口会 errno:-6）。
    // GUI 用它把状态显示成「已登录（只读）」并给出重新登录指引。
    let write_ok = d.write_ok_value();
    extra.insert("write_ok".to_string(), Value::Bool(write_ok));
    if !write_ok {
        wp_core::log_warn!(
            "百度",
            "当前会话只读：浏览/下载可用，上传/新建/改名/删除等写操作会被拒（errno:-6）。\
             请重新扫码登录，或在浏览器重新登录后导入最新 Cookie（BDUSS + STOKEN）；\
             若账号空间已超限，请先清理/扩容"
        );
    }

    let quota = match d
        .api_get("/api/quota", &[], ReqOpts::authed().no_channel())
        .await
    {
        Ok(q) => parse_quota(&q),
        Err(e) => {
            wp_core::log_warn!("百度", "容量查询失败（账号信息仍可用）：{e}");
            None
        }
    };

    Ok(AccountInfo {
        display_name: if display_name.is_empty() {
            format!("百度网盘用户{uk}")
        } else {
            display_name
        },
        user_name,
        user_id: uk.to_string(),
        vip,
        quota,
        extra,
    })
}

fn vip_text(record: &Value) -> String {
    if let Some(name) = record
        .get("priority_name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        return name.trim().to_string();
    }
    let vip_type = record.get("vip_type").and_then(Value::as_i64).unwrap_or(0);
    let level = record.get("vip_level").and_then(Value::as_i64).unwrap_or(0);
    match vip_type {
        2 => "超级会员 SVIP".to_string(),
        1 => {
            if level > 0 {
                format!("会员 VIP{level}")
            } else {
                "会员 VIP".to_string()
            }
        }
        _ => String::new(),
    }
}

/// 解析 `/api/quota`。
pub(crate) fn parse_quota(v: &Value) -> Option<Quota> {
    let total = v.get("total").and_then(Value::as_u64)?;
    let used = v.get("used").and_then(Value::as_u64).unwrap_or(0);
    let free = v
        .get("free")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| total.saturating_sub(used));
    Some(Quota { total, used, free })
}

/// 写权限探测：创建并删除一个隐藏探测目录（真实写入）。
pub(crate) async fn write_probe(d: &BaiduDrive) -> Result<String> {
    // 这里只做登录态校验；真正的创建/删除放在 files 模块，避免循环依赖。
    if !d.session_valid() {
        return Err(Error::auth("未登录，无法探测写权限"));
    }
    let probe = format!(
        "/.wp_write_probe_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    files::ensure_dir(d, &probe).await?;
    let report = files::delete(d, std::slice::from_ref(&probe)).await?;
    if report.is_ok() {
        Ok(format!("写权限正常（已创建并删除探测目录 {probe}）"))
    } else {
        Err(Error::other(format!(
            "写权限探测目录已创建，但删除失败：{}",
            report.summary()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonp_and_url_helpers() {
        assert_eq!(strip_jsonp("cb({\"a\":1});"), "{\"a\":1}");
        assert_eq!(strip_jsonp("  {\"a\":1}  "), "{\"a\":1}");
        assert_eq!(
            absolute_url("passport.baidu.com/v2/api/qrcode?sign=x"),
            "https://passport.baidu.com/v2/api/qrcode?sign=x"
        );
        assert_eq!(
            absolute_url("//passport.baidu.com/x"),
            "https://passport.baidu.com/x"
        );
        assert_eq!(
            query_param(
                "https://passport.baidu.com/v2/api/qrcode?sign=abc&lp=pc",
                "sign"
            )
            .as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn unicast_token_detection_is_token_based() {
        // 真机确认报文：status=0 + v，必须识别为成功
        let v: Value =
            serde_json::from_str(r#"{"channel_v":"{\"status\":0,\"v\":\"0123456789abcdef0123456789abcdef\",\"u\":\"\"}"}"#)
                .unwrap();
        let (status, token) = parse_unicast(&v);
        assert_eq!(status, 0);
        assert_eq!(token.as_deref(), Some("0123456789abcdef0123456789abcdef"));

        // 已扫描待确认
        let v: Value = serde_json::from_str(r#"{"channel_v":"{\"status\":1}"}"#).unwrap();
        let (status, token) = parse_unicast(&v);
        assert_eq!(status, 1);
        assert!(token.is_none());

        // 未扫码 errno:1 的形态
        let v: Value = serde_json::from_str(r#"{"errno":1}"#).unwrap();
        let (status, token) = parse_unicast(&v);
        assert_eq!(status, 0);
        assert!(token.is_none());

        // 兜底字段
        let v: Value = serde_json::from_str(
            r#"{"channel_v":"{\"status\":0}","foo":"ffffffffffffffffffffffffffffffff"}"#,
        )
        .unwrap();
        assert_eq!(
            parse_unicast(&v).1.as_deref(),
            Some("ffffffffffffffffffffffffffffffff")
        );
    }

    #[test]
    fn quota_and_vip_mapping() {
        let q = parse_quota(&json!({"total": 1000, "used": 250, "free": 750})).unwrap();
        assert_eq!(q.total, 1000);
        assert_eq!(q.used, 250);
        assert_eq!(q.free, 750);
        let q = parse_quota(&json!({"total": 1000, "used": 250})).unwrap();
        assert_eq!(q.free, 750);
        assert!(parse_quota(&json!({})).is_none());

        assert_eq!(vip_text(&json!({"vip_type": 2})), "超级会员 SVIP");
        assert_eq!(
            vip_text(&json!({"vip_type": 1, "vip_level": 3})),
            "会员 VIP3"
        );
        assert_eq!(vip_text(&json!({"priority_name": "超级会员"})), "超级会员");
        assert_eq!(vip_text(&json!({})), "");
    }

    #[test]
    fn base_name_is_used_for_probe_names() {
        assert_eq!(
            wp_core::util::base_name("/.wp_write_probe_abcd"),
            ".wp_write_probe_abcd"
        );
    }
}
