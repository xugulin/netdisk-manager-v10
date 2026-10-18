//! 分享：创建 / 列表 / 取消 / 校验 / 转存。
//!
//! 文档 §4.4 的实测坑：
//! * 创建分享的 `pwd` **必需且必须 4 位**，空串会得 `pwd length param error`；
//!   响应**不回显 pwd**，调用方必须自己记住；
//! * `fid_list` 是 `fs_id` 数组，不是路径；
//! * 列表字段名易混：`shareId`（大写 I，对外 ID）≠ `shareid`，
//!   `passwd`（提取码）≠ `pwd`，`shortlink`（完整链接）≠ `shorturl`（仅短码）；
//! * 列表终止条件是 `nextpage == 0`，不是“本页是否满”；
//! * 取消分享用 `shareid`（不是 fs_id）。
//!
//! 校验/转存（文档 §4.4 + `分享接口.py`）：
//! 1. `POST /share/verify`：form `pwd` → 响应 `randsk`（URL 解码后即 `sekey`）；
//!    同时服务端 `Set-Cookie: BDCLND=<randsk>`，后续请求必须显式带上；
//! 2. `GET /share/list?shorturl=…&root=1`：**必须带 BDCLND**，否则 `errno:-9`；
//!    响应给 `share_id / uk / title / list`；
//! 3. `POST /share/transfer`：query `shareid/from/sekey/ondup=newcopy/async=1`，
//!    form `fsidlist=[...]&path=/目标目录` → `task_id`；
//! 4. `GET /share/taskquery?taskid=…` 轮询到 success。
//!
//! ⚠️ 顺序必须 **verify → list → transfer**；`sekey` 不是响应里的 `sekey` 字段，
//! 而是 `randsk` 的 URL 解码值。

use crate::files;
use crate::http::ReqOpts;
use crate::{now_ms, BaiduDrive};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;
use wp_core::model::{OpReport, RemoteEntry, ShareDetail, ShareInfo};
use wp_core::util::now_ts;
use wp_core::{Error, Result};
use wp_drive::ShareOptions;

fn p(key: &str, value: impl Into<String>) -> (String, String) {
    (key.to_string(), value.into())
}

fn json_text(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())
}

/// 校验/生成 4 位提取码。
///
/// 百度要求 4 位：不传或空串会报 `pwd length param error`；
/// 未指定时按 Python 版生成 4 位数字。
pub(crate) fn share_password(input: Option<&str>) -> Result<String> {
    if let Some(raw) = input {
        let pwd = raw.trim();
        if !pwd.is_empty() {
            let chars: Vec<char> = pwd.chars().collect();
            if chars.len() != 4 || !chars.iter().all(|c| c.is_ascii_alphanumeric()) {
                return Err(Error::other(
                    "百度分享提取码必须是 4 位字母/数字（空串会被服务端拒绝）",
                ));
            }
            return Ok(pwd.to_string());
        }
    }
    let n = (uuid::Uuid::new_v4().as_u128() % 10_000) as u32;
    Ok(format!("{n:04}"))
}

/// 天数 → 百度 `period`（0/1/7/30/365）。
///
/// `ShareOptions.expire_days = 0` 表示永久；其它值向上取到最近的支持档位。
pub(crate) fn period_for_days(days: u32) -> u32 {
    match days {
        0 => 0,
        1 => 1,
        2..=7 => 7,
        8..=30 => 30,
        31..=365 => 365,
        _ => 0,
    }
}

/// 创建分享。
pub(crate) async fn create_share(
    d: &BaiduDrive,
    paths: &[String],
    opts: &ShareOptions,
) -> Result<ShareInfo> {
    if paths.is_empty() {
        return Err(Error::other("分享文件列表不能为空"));
    }
    let mut fids = Vec::with_capacity(paths.len());
    for path in paths {
        let entry = files::stat(d, path).await?;
        let id = entry
            .id
            .trim()
            .parse::<i64>()
            .map_err(|_| Error::server(-1, format!("{path} 缺少数字 fs_id，无法分享")))?;
        fids.push(id);
    }
    let pwd = share_password(opts.password.as_deref())?;
    let period = period_for_days(opts.expire_days);
    let form = vec![
        p("is_knowledge", "0"),
        p("public", "0"),
        p("period", period.to_string()),
        p("pwd", pwd.clone()),
        p("eflag_disable", "true"),
        p("linkOrQrcode", "link"),
        p("channel_list", "[]"),
        p("schannel", "4"),
        p("fid_list", json_text(&json!(fids))),
    ];
    let v = d
        .api_post_form("/share/pset", &[], &form, ReqOpts::authed().with_bdstoken())
        .await?;

    let share_id = v
        .get("shareid")
        .or_else(|| v.get("shareId"))
        .map(value_to_string)
        .unwrap_or_default();
    let url = share_url(&v);
    let expire_at = v
        .get("expiretime")
        .and_then(Value::as_i64)
        .filter(|t| *t > 0)
        .or_else(|| {
            if period > 0 {
                Some(now_ts() + period as i64 * 86_400)
            } else {
                None
            }
        });
    wp_core::log_info!(
        "百度",
        "创建分享 ✅ shareid={share_id} 链接={url} 提取码={pwd}"
    );
    Ok(ShareInfo {
        share_id,
        url,
        password: pwd,
        expire_at,
        title: opts.title.clone(),
    })
}

fn value_to_string(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        s.to_string()
    } else if let Some(n) = v.as_i64() {
        n.to_string()
    } else if let Some(n) = v.as_u64() {
        n.to_string()
    } else {
        String::new()
    }
}

/// 从创建/列表响应中拼分享 URL。
fn share_url(v: &Value) -> String {
    if let Some(link) = v
        .get("link")
        .or_else(|| v.get("shortlink"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return link.to_string();
    }
    if let Some(short) = v
        .get("shorturl")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if short.starts_with("http://") || short.starts_with("https://") {
            return short.to_string();
        }
        return format!("https://pan.baidu.com/s/{short}");
    }
    String::new()
}

/// 映射分享列表条目。
pub(crate) fn share_info_from_value(v: &Value) -> Option<ShareInfo> {
    let share_id = ["shareId", "shareid", "share_id"]
        .iter()
        .filter_map(|k| v.get(*k))
        .map(value_to_string)
        .find(|s| !s.is_empty())?;
    let url = share_url(v);
    let password = ["passwd", "pwd"]
        .iter()
        .filter_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();
    let expire_at = ["expiretime", "expire_time", "expiredType"]
        .iter()
        .filter_map(|k| v.get(*k).and_then(Value::as_i64))
        .find(|t| *t > 1_000_000_000)
        .or(None);
    let title = ["title", "share_name", "typicalName", "typicalPath"]
        .iter()
        .filter_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();
    Some(ShareInfo {
        share_id,
        url,
        password,
        expire_at,
        title,
    })
}

/// 取全部有效分享（`nextpage == 0` 终止，最多 50 页）。
pub(crate) async fn list_shares(d: &BaiduDrive) -> Result<Vec<ShareInfo>> {
    let mut out = Vec::new();
    let mut page = 1u32;
    while page <= 50 {
        let params = vec![
            p("num", "100"),
            p("page", page.to_string()),
            p("order", "ctime"),
            p("desc", "1"),
            p("is_batch", "1"),
        ];
        let v = d
            .api_get("/share/record", &params, ReqOpts::authed())
            .await?;
        let Some(items) = v.get("list").and_then(Value::as_array) else {
            break;
        };
        for item in items {
            if let Some(info) = share_info_from_value(item) {
                out.push(info);
            }
        }
        let nextpage = v
            .get("nextpage")
            .and_then(|x| {
                x.as_i64()
                    .or_else(|| x.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
            })
            .unwrap_or(0);
        if nextpage == 0 || items.is_empty() {
            break;
        }
        page += 1;
    }
    wp_core::log_debug!("百度", "分享列表 → {} 项", out.len());
    Ok(out)
}

/// 取消分享（参数是 shareid，不是 fs_id）。
pub(crate) async fn cancel_shares(d: &BaiduDrive, ids: &[String]) -> Result<OpReport> {
    if ids.is_empty() {
        return Err(Error::other("shareid 列表不能为空"));
    }
    let mut numbers = Vec::with_capacity(ids.len());
    for id in ids {
        let n = id
            .trim()
            .parse::<i64>()
            .map_err(|_| Error::other(format!("shareid 不是数字：{id:?}")))?;
        numbers.push(n);
    }
    let form = vec![p("shareid_list", json_text(&json!(numbers)))];
    d.api_post_form(
        "/share/cancel",
        &[],
        &form,
        ReqOpts::authed().with_bdstoken(),
    )
    .await?;
    wp_core::log_info!("百度", "取消分享 {} 个 shareid", ids.len());
    Ok(OpReport {
        ok: ids.len(),
        ..Default::default()
    })
}

// ═══════════════════════════ 分享校验 / 转存 ═══════════════════════════

/// 用户粘贴的分享链接解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedShare {
    /// 规范化后的完整链接（去掉 `?pwd=`，提取码单独放在 `pwd`）。
    pub url: String,
    /// `/share/verify` 与 `/share/list` 使用的短码（新版 `/s/1xxxx` 已去掉 `1`）。
    pub surl: String,
    /// 链接里 `?pwd=xxxx` 提取码；没有则为空。
    pub pwd: String,
}

fn is_code_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// 从 `.../s/<code>` 里取短码（支持链接前后夹带文字/标点）。
fn code_after_slash_s(text: &str) -> Option<String> {
    let start = text.find("/s/")? + 3;
    let code: String = text[start..]
        .chars()
        .take_while(|c| is_code_char(*c))
        .collect();
    if code.is_empty() {
        None
    } else {
        Some(code)
    }
}

/// 取文本里最长的一段“短码字符”，用于兜底识别裸短码。
fn longest_code_run(text: &str) -> Option<String> {
    let mut best = String::new();
    let mut cur = String::new();
    for c in text.chars() {
        if is_code_char(c) {
            cur.push(c);
        } else {
            if cur.len() > best.len() {
                best.clone_from(&cur);
            }
            cur.clear();
        }
    }
    if cur.len() > best.len() {
        best = cur;
    }
    if best.is_empty() {
        None
    } else {
        Some(best)
    }
}

/// 从混合文本里提取分享短码。
///
/// 支持（与 Python `解析分享链接` 对齐并略放宽）：
/// * `https://pan.baidu.com/s/1xxxx?pwd=abcd`
/// * `pan.baidu.com/s/1xxxx`
/// * 裸短码 `1xxxx` / `xxxx`
/// * 前后带空格、中文说明（`分享链接：… 提取码：…`）
fn extract_code(text: &str) -> Option<String> {
    if let Some(code) = code_after_slash_s(text) {
        return Some(code);
    }
    // 不含 /s/ 的 URL 不猜短码，避免把 "https"/"pan" 当成短码。
    if text.contains("://") || text.contains("pan.baidu.com") {
        return None;
    }
    let mut best: Option<String> = None;
    for raw in text.split_whitespace() {
        let token = raw.trim_matches(|c: char| !is_code_char(c));
        if token.len() >= 2 && token.chars().all(is_code_char) {
            if best.as_ref().map_or(true, |b| token.len() > b.len()) {
                best = Some(token.to_string());
            }
        }
    }
    best.or_else(|| longest_code_run(text).filter(|s| s.len() >= 2))
}

/// `?pwd=xxxx` / `&pwd=xxxx`；参数必须是 4 位字母数字（与源码正则一致）。
fn extract_pwd(text: &str) -> Option<String> {
    let mut offset = 0usize;
    while let Some(rel) = text[offset..].find("pwd=") {
        let i = offset + rel;
        let preceded = i > 0 && matches!(text.as_bytes()[i - 1], b'?' | b'&');
        if preceded {
            let rest = &text[i + 4..];
            let code: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if code.chars().count() == 4 {
                return Some(code);
            }
        }
        offset = i + 4;
    }
    None
}

/// 百度短码规则：新版分享链接 `/s/1xxxx`，接口用去掉开头 `1` 的形式。
///
/// 与 Python 一致：只有长度 > 1 且以 `1` 开头时才去掉；老短码误判边界
/// 在文档里仍是「待确认」，这里不额外猜测。
fn strip_share_prefix(code: &str) -> String {
    if code.len() > 1 && code.starts_with('1') {
        code[1..].to_string()
    } else {
        code.to_string()
    }
}

/// 解析用户输入的分享链接 / 短码。
pub(crate) fn parse_share_link(input: &str) -> Result<ParsedShare> {
    let text = input.trim();
    if text.is_empty() {
        return Err(Error::other("分享链接为空"));
    }
    let pwd = extract_pwd(text).unwrap_or_default();
    let code = extract_code(text).ok_or_else(|| {
        Error::other(format!(
            "无法从输入中解析出百度分享短码：{text:?}（支持 /s/xxxx 链接或裸短码）"
        ))
    })?;
    let surl = strip_share_prefix(&code);
    if surl.is_empty() {
        return Err(Error::other(format!("分享短码为空：{text:?}")));
    }
    Ok(ParsedShare {
        url: format!("https://pan.baidu.com/s/{code}"),
        surl,
        pwd,
    })
}

/// 提取码：显式参数优先于链接里的 `?pwd=`；非空时必须是 4 位字母/数字。
pub(crate) fn effective_password(input: &str, from_link: &str) -> Result<String> {
    let raw = if input.trim().is_empty() {
        from_link.trim()
    } else {
        input.trim()
    };
    if raw.is_empty() {
        return Ok(String::new());
    }
    let chars: Vec<char> = raw.chars().collect();
    if chars.len() != 4 || !chars.iter().all(|c| c.is_ascii_alphanumeric()) {
        return Err(Error::other(format!(
            "百度分享提取码必须是 4 位字母/数字：{raw:?}"
        )));
    }
    Ok(raw.to_string())
}

/// 百分号解码（只处理 UTF-8 的 `%XX`，与 Python `urllib.parse.unquote` 对齐）。
///
/// 注意：**不把 `+` 转成空格**（unquote 不转，unquote_plus 才转）；
/// `randsk` 里的 `+` 是 sekey 的一部分，转错会导致转存失败。
pub(crate) fn percent_decode(text: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 校验失败时把 errno 翻译成用户能照做的提示。
///
/// `/share/verify` 的失败码在真机上并不固定（源码统一抛「提取码错误」），
/// 因此除 Auth / Network / 132 风控外，统一归为“提取码错误或分享失效”。
pub(crate) fn wrap_verify_error(e: Error) -> Error {
    match e {
        Error::Auth(m) => Error::Auth(m),
        Error::Network(m) => Error::Network(m),
        Error::Canceled(m) => Error::Canceled(m),
        Error::Io(e) => Error::Io(e),
        Error::Server { code: 132, msg } => Error::Server { code: 132, msg },
        Error::Server { code, msg } => {
            Error::other(format!("提取码错误或分享链接失效（errno:{code}）：{msg}"))
        }
        Error::NotFound(msg) => Error::other(format!("提取码错误或分享链接失效：{msg}")),
        Error::Unsupported(m) => Error::Unsupported(m),
        Error::Other(m) => Error::Other(m),
    }
}

/// 已校验并打开（拿到 share_id/uk/文件列表）的分享。
pub(crate) struct OpenedShare {
    pub parsed: ParsedShare,
    pub password: String,
    /// URL 解码后的 randsk，`/share/transfer?sekey=` 使用。
    pub sekey: String,
    /// `randsk` 原文，作为 `BDCLND` Cookie 显式携带。
    pub bdclnd: String,
    pub share_id: String,
    pub uk: String,
    pub title: String,
    pub files: Vec<RemoteEntry>,
    pub expire_at: Option<i64>,
}

/// `POST /share/verify`：校验提取码，返回 `(sekey, bdclnd)`。
async fn verify_share(d: &BaiduDrive, surl: &str, pwd: &str) -> Result<(String, String)> {
    let params = vec![
        p("t", now_ms().to_string()),
        p("bioc", "1"),
        p("surl", surl),
        // 与 Python 一样显式带 channel，同时走 no_channel，避免重复。
        p("channel", "chunlei"),
    ];
    let form = vec![p("pwd", pwd), p("vcode", ""), p("vcode_str", "")];
    let v = match d
        .api_post_form(
            "/share/verify",
            &params,
            &form,
            ReqOpts::authed().no_channel().with_bdstoken(),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(wrap_verify_error(e)),
    };
    let errno = v.get("errno").and_then(Value::as_i64).unwrap_or(0);
    let randsk = v
        .get("randsk")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if errno != 0 || randsk.is_empty() {
        let msg = crate::err_text(&v, "");
        return Err(Error::other(if msg.is_empty() {
            format!("提取码错误或分享链接失效（errno:{errno}）")
        } else {
            format!("提取码错误或分享链接失效（errno:{errno}）：{msg}")
        }));
    }
    let sekey = percent_decode(&randsk);
    if sekey.is_empty() {
        return Err(Error::other("校验分享失败：randsk 解码后为空"));
    }
    Ok((sekey, randsk))
}

/// 分享过期时间（接口有时给 `expiretime`，`/share/list` 实测多数不返回）。
fn expire_at_of(v: &Value) -> Option<i64> {
    ["expiretime", "expire_time", "expire_at", "expired_at"]
        .iter()
        .filter_map(|k| {
            v.get(*k).and_then(|x| {
                x.as_i64()
                    .or_else(|| x.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
            })
        })
        .find(|t| *t > 1_000_000_000)
}

/// `GET /share/list`：拿 share_id/uk/标题/文件清单（自动翻页，最多 10 页）。
///
/// 必须带 `BDCLND=<randsk>`；不带会得到 `errno:-9`（文档 §4.4 实测）。
async fn fetch_share_list(
    d: &BaiduDrive,
    surl: &str,
    bdclnd: &str,
) -> Result<(Value, Vec<RemoteEntry>)> {
    let mut first: Option<Value> = None;
    let mut files = Vec::new();
    for page in 1..=10u32 {
        let mut opts = ReqOpts::authed().no_channel().with_bdstoken();
        opts.extra_cookie = Some(format!("BDCLND={bdclnd}"));
        let params = vec![
            p("shorturl", surl),
            p("desc", "1"),
            p("showempty", "0"),
            p("page", page.to_string()),
            p("num", "100"),
            p("order", "time"),
            p("root", "1"),
            p("view_mode", "1"),
            p("channel", "chunlei"),
        ];
        let v = d.api_get("/share/list", &params, opts).await?;
        if first.is_none() {
            first = Some(v.clone());
        }
        let Some(items) = v.get("list").and_then(Value::as_array) else {
            break;
        };
        let count = items.len();
        for item in items {
            if let Some(entry) = files::entry_from_value(item, "/") {
                files.push(entry);
            }
        }
        if count < 100 {
            break;
        }
    }
    Ok((first.unwrap_or_else(|| json!({})), files))
}

/// 完整链路：解析链接 → verify → list。
async fn open_share(d: &BaiduDrive, url: &str, password: &str) -> Result<OpenedShare> {
    let parsed = parse_share_link(url)?;
    let password = effective_password(password, &parsed.pwd)?;
    let (sekey, bdclnd) = verify_share(d, &parsed.surl, &password).await?;
    let (raw, files) = fetch_share_list(d, &parsed.surl, &bdclnd).await?;
    let share_id = raw.get("share_id").map(value_to_string).unwrap_or_default();
    let uk = raw.get("uk").map(value_to_string).unwrap_or_default();
    let title = ["title", "share_name", "typicalName"]
        .iter()
        .filter_map(|k| raw.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("百度网盘分享")
        .to_string();
    let expire_at = expire_at_of(&raw);
    wp_core::log_info!(
        "百度",
        "打开分享 surl={} → share_id={share_id} uk={uk} 顶层 {} 项",
        parsed.surl,
        files.len()
    );
    Ok(OpenedShare {
        parsed,
        password,
        sekey,
        bdclnd,
        share_id,
        uk,
        title,
        files,
        expire_at,
    })
}

/// 附加说明（GUI 的 note 字段）。
pub(crate) fn share_note(password: &str, uk: &str, count: usize) -> String {
    let mut parts = Vec::new();
    parts.push(if password.is_empty() {
        "无需提取码".to_string()
    } else {
        format!("提取码 {password}")
    });
    if !uk.is_empty() {
        parts.push(format!("分享者 uk={uk}"));
    }
    parts.push(format!("顶层 {count} 项"));
    parts.join("；")
}

/// 解析/校验分享链接（转存前预览）。
pub(crate) async fn share_info(d: &BaiduDrive, url: &str, password: &str) -> Result<ShareDetail> {
    let opened = open_share(d, url, password).await?;
    Ok(ShareDetail {
        title: opened.title.clone(),
        url: opened.parsed.url.clone(),
        password: opened.password.clone(),
        files: opened.files.clone(),
        expire_at: opened.expire_at,
        note: share_note(&opened.password, &opened.uk, opened.files.len()),
    })
}

/// 从共享文件列表里取可转存的 fs_id（去重、跳过空 id）。
pub(crate) fn fsid_list(files: &[RemoteEntry]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for f in files {
        let id = f.id.trim();
        if id.is_empty() {
            continue;
        }
        if seen.insert(id.to_string()) {
            out.push(id.to_string());
        }
    }
    out
}

/// 转存分享到自己的网盘目录。
///
/// 协议事实（Python `一键转存` + HAR）：`/share/transfer` 的 `fsidlist`
/// 是**一次请求里的数组**，服务端只回一个 `task_id`；不是“每个文件发一次请求”。
/// 这里按事实源发一次请求，`OpReport` 的计数按转存的顶层条目数记。
pub(crate) async fn save_shared(
    d: &BaiduDrive,
    url: &str,
    password: &str,
    dest_dir: &str,
) -> Result<OpReport> {
    let opened = open_share(d, url, password).await?;
    if opened.share_id.is_empty() || opened.uk.is_empty() {
        return Err(Error::server(
            -1,
            "/share/list 未返回 share_id/uk，无法转存（分享可能已失效）",
        ));
    }
    let fids = fsid_list(&opened.files);
    if fids.is_empty() {
        return Err(Error::other("分享里没有可转存的文件（接口未返回 fs_id）"));
    }
    // 目标目录先确保存在，转存接口的 path 只接受已存在的目录。
    let dest = files::ensure_dir(d, dest_dir).await?;

    let mut opts = ReqOpts::authed().no_channel().with_bdstoken();
    opts.extra_cookie = Some(format!("BDCLND={}", opened.bdclnd));
    let params = vec![
        p("shareid", opened.share_id.clone()),
        p("from", opened.uk.clone()),
        p("sekey", opened.sekey.clone()),
        p("ondup", "newcopy"),
        p("async", "1"),
        p("channel", "chunlei"),
    ];
    // HAR 里 fsidlist 是字符串数组；Python 用数字数组，两种服务端都接受。
    let form = vec![
        p("fsidlist", json_text(&json!(fids))),
        p("path", dest.path.clone()),
    ];
    let v = d
        .api_post_form("/share/transfer", &params, &form, opts)
        .await?;

    let count = fids.len();
    let task_id = files::task_id_of(&v);
    wp_core::log_info!(
        "百度",
        "转存 {} 项到 {} → task_id={:?}（share_id={}）",
        count,
        dest.path,
        task_id,
        opened.share_id
    );
    let mut report = OpReport::default();
    match task_id {
        None => {
            // 没有 task_id 时按“已提交”计（服务端可能同步完成）。
            report.ok = count;
        }
        Some(tid) => {
            match files::wait_task(d, &tid, Duration::from_secs(3), Duration::from_secs(300)).await
            {
                Ok(_) => report.ok = count,
                Err(e) => {
                    report.failed = count;
                    report.errors.push(e.to_string());
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_password_validation() {
        assert_eq!(share_password(Some("ab12")).unwrap(), "ab12");
        assert_eq!(share_password(Some(" 1234 ")).unwrap(), "1234");
        assert!(share_password(Some("123")).is_err());
        assert!(share_password(Some("12345")).is_err());
        assert!(share_password(Some("中文12")).is_err());
        let auto = share_password(None).unwrap();
        assert_eq!(auto.len(), 4);
        assert!(auto.chars().all(|c| c.is_ascii_digit()));
        let auto2 = share_password(Some("")).unwrap();
        assert_eq!(auto2.len(), 4);
    }

    #[test]
    fn period_mapping() {
        assert_eq!(period_for_days(0), 0);
        assert_eq!(period_for_days(1), 1);
        assert_eq!(period_for_days(3), 7);
        assert_eq!(period_for_days(7), 7);
        assert_eq!(period_for_days(10), 30);
        assert_eq!(period_for_days(30), 30);
        assert_eq!(period_for_days(100), 365);
        assert_eq!(period_for_days(365), 365);
        assert_eq!(period_for_days(366), 0);
    }

    #[test]
    fn share_record_mapping() {
        let v = serde_json::json!({
            "shareId": 45163615061i64,
            "passwd": "pxi8",
            "shortlink": "https://pan.baidu.com/s/1LaFlYKlcychx64Na_dmrnQ",
            "shorturl": "1LaFlYKlcychx64Na_dmrnQ",
            "title": "我的分享",
            "expiretime": 0
        });
        let info = share_info_from_value(&v).unwrap();
        assert_eq!(info.share_id, "45163615061");
        assert_eq!(info.password, "pxi8");
        assert_eq!(info.url, "https://pan.baidu.com/s/1LaFlYKlcychx64Na_dmrnQ");
        assert_eq!(info.title, "我的分享");
        assert_eq!(info.expire_at, None);

        // 只有短码时拼完整 URL
        let v = serde_json::json!({"shareId": "1", "shorturl": "1abc", "passwd": "x"});
        assert_eq!(
            share_info_from_value(&v).unwrap().url,
            "https://pan.baidu.com/s/1abc"
        );
    }

    #[test]
    fn share_link_parsing_full_url_and_pwd() {
        let p = parse_share_link("  https://pan.baidu.com/s/1LaFlYKlcychx64Na_dmrnQ?pwd=pxi8  ")
            .unwrap();
        assert_eq!(p.surl, "LaFlYKlcychx64Na_dmrnQ");
        assert_eq!(p.pwd, "pxi8");
        assert_eq!(p.url, "https://pan.baidu.com/s/1LaFlYKlcychx64Na_dmrnQ");
    }

    #[test]
    fn share_link_parsing_spaces_chinese_and_bare_code() {
        // 前后夹中文说明与空格
        let p =
            parse_share_link("分享链接： https://pan.baidu.com/s/1abcDEF?pwd=12ab 提取码：12ab")
                .unwrap();
        assert_eq!(p.surl, "abcDEF");
        assert_eq!(p.pwd, "12ab");

        // 无 scheme
        let p = parse_share_link("  pan.baidu.com/s/1abcDEF  ").unwrap();
        assert_eq!(p.url, "https://pan.baidu.com/s/1abcDEF");
        assert_eq!(p.surl, "abcDEF");

        // 裸短码（前后空格 / 中文）
        assert_eq!(parse_share_link("  1abcDEF  ").unwrap().surl, "abcDEF");
        let p = parse_share_link("短码 1abcDEF").unwrap();
        assert_eq!(p.surl, "abcDEF");
        assert_eq!(p.url, "https://pan.baidu.com/s/1abcDEF");

        // 老短码不以 1 开头时原样保留
        assert_eq!(parse_share_link("abcDEF").unwrap().surl, "abcDEF");

        assert!(parse_share_link("").is_err());
        assert!(parse_share_link("中文没有链接").is_err());
        assert!(parse_share_link("https://pan.baidu.com/share/init?surl=abc").is_err());
    }

    #[test]
    fn share_password_precedence_and_validation() {
        // 显式参数优先于链接里的 pwd
        assert_eq!(effective_password("ab12", "zzzz").unwrap(), "ab12");
        assert_eq!(effective_password("", "zzzz").unwrap(), "zzzz");
        assert_eq!(effective_password("   ", "zzzz").unwrap(), "zzzz");
        assert_eq!(effective_password("", "").unwrap(), "");
        assert!(effective_password("123", "zzzz").is_err());
        assert!(effective_password("中文12", "zzzz").is_err());
    }

    #[test]
    fn percent_decode_matches_unquote_without_plus_conversion() {
        let raw = "Tiu6lMaFeDt4fwewkmmU0hKE%2ByMvPYSJn5ckIWQHwXo%3D";
        let key = percent_decode(raw);
        assert!(key.contains('+'));
        assert!(key.ends_with('='));
        assert_eq!(percent_decode("a+b"), "a+b");
        assert_eq!(percent_decode("100%25"), "100%");
        assert_eq!(percent_decode("%E4%B8%AD"), "中");
        // 非法百分号原样保留，不能 panic
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("abc%"), "abc%");
    }

    #[test]
    fn share_fsid_list_dedupes_and_skips_empty() {
        let mk = |id: &str| RemoteEntry {
            id: id.to_string(),
            name: "x".into(),
            path: "/x".into(),
            ..Default::default()
        };
        let files = vec![mk("1"), mk("1"), mk(""), mk("2"), mk(" 3 ")];
        assert_eq!(fsid_list(&files), vec!["1", "2", "3"]);
        assert!(fsid_list(&[]).is_empty());
    }

    #[test]
    fn verify_error_is_readable_and_keeps_auth() {
        let e = wrap_verify_error(Error::server(105, "pwd error"));
        let text = e.to_string();
        assert!(text.contains("提取码错误"), "实际：{text}");
        assert!(text.contains("105"), "实际：{text}");

        // 132 风控保持 Server(132)，交给上层给“需短信验证”提示。
        match wrap_verify_error(Error::server(132, "need verify")) {
            Error::Server { code, .. } => assert_eq!(code, 132),
            other => panic!("132 应保持 Server，实际 {other:?}"),
        }
        // 未登录不能被包装成“提取码错误”。
        assert!(wrap_verify_error(Error::auth("未登录")).is_auth());
    }

    #[test]
    fn share_note_mentions_pwd_and_count() {
        assert!(share_note("", "", 0).contains("无需提取码"));
        let n = share_note("pxi8", "1101796850381", 3);
        assert!(n.contains("pxi8"));
        assert!(n.contains("1101796850381"));
        assert!(n.contains("顶层 3 项"));
    }
}
