//! 分享：创建 / 列表 / 取消（含回查）、链接解析、校验与转存。
//!
//! 两套 ID 不能混用：
//! * `share_id`：32 位 hex，管理接口（详情/取消）用；
//! * URL slug：12 位字母数字，`https://pan.quark.cn/s/<slug>`，转存接口用 `pwd_id`。
//!
//! 取消分享必须传 `{"share_ids": [...]}` 数组；传标量 `share_id` 会返回成功但
//! 什么都不删（静默 no-op），因此这里默认回查列表确认。
//!
//! 转存（`share/sharepage/token|detail|save`）必须使用分享域名
//! `https://drive.quark.cn/1/clouddrive`，不能复用 `drive-pc` 基址；转存自己的
//! 分享会被服务端以 404 + 41017 拒绝，这里统一映射成可读中文错误。

use crate::files::map_entry;
use crate::{
    as_i64, as_str, as_u64, compact_json, data_of, pair, parse_timestamp_secs, QuarkDrive,
};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;
use wp_core::error::ErrorKind;
use wp_core::model::{OpReport, RemoteEntry, ShareDetail, ShareInfo};
use wp_core::{Error, Result};
use wp_drive::ShareOptions;

const SHARE_PAGE_SIZE: usize = 100;
const SHARE_PAGES: usize = 5;
/// 分享页 API 基址（与 `drive-pc` 不同）。
pub(crate) const SHARE_API_BASE: &str = "https://drive.quark.cn/1/clouddrive";
/// 转存预览只取第一页；完整转存走 `pdir_save_all`，不依赖这一页。
const SHARE_DETAIL_PAGE_SIZE: usize = 50;
/// 转存是异步任务，参考 Python 实现给 120 秒。
const SHARE_SAVE_TIMEOUT_SECS: u64 = 120;
/// 夸克「禁止转存自己的分享」业务码。
const CODE_OWN_SHARE: i64 = 41017;

// ───────────────────────── 链接解析 ─────────────────────────

fn take_passcode(rest: &str) -> Option<String> {
    let rest = rest.trim_start_matches(|c: char| {
        c == '：' || c == ':' || c == '=' || c == '\'' || c == '"' || c.is_whitespace()
    });
    let mut chars = rest.chars();
    let candidate: String = chars.by_ref().take(4).collect();
    if candidate.chars().count() == 4
        && candidate.chars().all(|c| c.is_ascii_alphanumeric())
        && !chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
    {
        Some(candidate)
    } else {
        None
    }
}

fn extract_share_slug(t: &str) -> Option<String> {
    let lower = t.to_ascii_lowercase();
    for prefix in [
        "https://pan.quark.cn/s/",
        "http://pan.quark.cn/s/",
        "pan.quark.cn/s/",
        "quark://share/",
    ] {
        if let Some(pos) = lower.find(prefix) {
            let rest = &t[pos + prefix.len()..];
            let id: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    // 兜底：用户直接贴了 slug（8 位以上纯字母数字）。
    if t.len() >= 8 && t.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Some(t.to_string());
    }
    None
}

/// 在 ASCII 文本里找「独立标记」：前面必须是分隔符或行首。
///
/// 否则 slug 本身含 `code`/`pwd`（如 `/s/code1234`）会被误当成提取码。
fn find_marker(lower: &str, kw: &str) -> Option<usize> {
    let mut from = 0usize;
    while from < lower.len() {
        let rel = lower[from..].find(kw)?;
        let pos = from + rel;
        let boundary = lower[..pos].chars().next_back().is_none_or(|c| {
            matches!(
                c,
                '?' | '&'
                    | '#'
                    | ':'
                    | '：'
                    | '='
                    | '\''
                    | '"'
                    | ' '
                    | '\t'
                    | '\r'
                    | '\n'
                    | ','
                    | '，'
                    | '('
                    | '（'
            )
        });
        if boundary {
            return Some(pos);
        }
        from = pos + kw.len();
    }
    None
}

fn extract_passcode(t: &str) -> Option<String> {
    // 中文标记优先，避免链接里的 `pwd` 覆盖用户手写的「提取码：xxxx」。
    for kw in ["密码", "提取码", "访问码"] {
        if let Some(pos) = t.find(kw) {
            if let Some(code) = take_passcode(&t[pos + kw.len()..]) {
                return Some(code);
            }
        }
    }
    let lower = t.to_ascii_lowercase();
    for kw in ["passcode", "password", "pwd", "code"] {
        if let Some(pos) = find_marker(&lower, kw) {
            if let Some(code) = take_passcode(&t[pos + kw.len()..]) {
                return Some(code);
            }
        }
    }
    None
}

/// 从分享文案里解析 `(slug, 提取码)`。
///
/// 支持：
/// * `https://pan.quark.cn/s/<slug>`（可带 `?pwd=1234`、`密码：1234`）；
/// * `pan.quark.cn/s/<slug>`（省略协议）；
/// * `quark://share/<slug>`；
/// * 直接贴 slug。
pub(crate) fn parse_share_link(text: &str) -> Result<(String, Option<String>)> {
    let t = text.trim();
    if t.is_empty() {
        return Err(Error::other("分享链接为空"));
    }
    let slug = extract_share_slug(t).ok_or_else(|| {
        Error::other(format!(
            "无法识别的分享链接：{}",
            t.chars().take(120).collect::<String>()
        ))
    })?;
    Ok((slug, extract_passcode(t)))
}

/// 显式传入的提取码优先于链接里带的；都为空时返回空串（公开分享）。
pub(crate) fn effective_passcode(explicit: &str, link_code: Option<&str>) -> String {
    let explicit = explicit.trim();
    if !explicit.is_empty() {
        return explicit.to_string();
    }
    link_code.unwrap_or("").trim().to_string()
}

pub(crate) fn format_share_url(id: &str) -> String {
    format!("https://pan.quark.cn/s/{}", id.trim())
}

pub(crate) fn delete_share_body(ids: &[String]) -> Value {
    json!({ "share_ids": ids })
}

fn map_share_info(raw: &Value) -> Option<ShareInfo> {
    let share_id = as_str(raw.get("share_id")).filter(|s| !s.is_empty())?;
    let pwd_id = as_str(raw.get("pwd_id")).unwrap_or_default();
    let url = as_str(raw.get("share_url"))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if pwd_id.is_empty() {
                None
            } else {
                Some(format_share_url(&pwd_id))
            }
        })
        .unwrap_or_default();
    Some(ShareInfo {
        share_id,
        url,
        password: as_str(raw.get("passcode")).unwrap_or_default(),
        expire_at: parse_timestamp_secs(raw.get("expired_at")),
        title: as_str(raw.get("title")).unwrap_or_default(),
    })
}

fn text_of(raw: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(s) = as_str(raw.get(*key)) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// `share/sharepage/detail` 的 query 参数（与 Python 参考实现一致）。
pub(crate) fn share_detail_params(
    slug: &str,
    stoken: &str,
    page: usize,
    size: usize,
) -> Vec<(String, String)> {
    vec![
        pair("pwd_id", slug),
        pair("stoken", stoken),
        pair("pdir_fid", "0"),
        pair("force", "0"),
        pair("_page", page.to_string()),
        pair("_size", size.to_string()),
        pair("_fetch_banner", "1"),
        pair("_fetch_share", "1"),
        pair("_fetch_total", "1"),
        pair("_sort", "file_type:asc,file_name:asc"),
    ]
}

/// `share/sharepage/save` 请求体：`pdir_save_all=true` 转存分享内全部内容。
pub(crate) fn save_shared_body(slug: &str, stoken: &str, dest_fid: &str) -> Value {
    let dest_fid = if dest_fid.trim().is_empty() {
        "0"
    } else {
        dest_fid.trim()
    };
    json!({
        "fid_list": [],
        "fid_token_list": [],
        "to_pdir_fid": dest_fid,
        "pwd_id": slug,
        "stoken": stoken,
        "pdir_fid": "0",
        "pdir_save_all": true,
        "exclude_fids": [],
        "scene": "link",
    })
}

/// 把 `share/sharepage/detail` 的响应映射成统一模型。
///
/// `list` 只取第一页（最多 50 项）；`total` 更大时在 `note` 里说明，
/// 真正的转存走 `pdir_save_all`，不受预览页影响。
pub(crate) fn map_share_detail(resp: &Value, slug: &str, password: &str) -> ShareDetail {
    let data = data_of(resp);
    let share = data.get("share").unwrap_or(&Value::Null);
    let files: Vec<RemoteEntry> = data
        .get("list")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(|raw| map_entry(raw, "/")).collect())
        .unwrap_or_default();
    let title = text_of(share, &["title", "share_name", "shareName", "name"])
        .or_else(|| text_of(&data, &["title", "share_name", "shareName"]))
        .or_else(|| files.first().map(|f| f.name.clone()))
        .unwrap_or_else(|| slug.to_string());
    let total = as_u64(data.get("total"))
        .or_else(|| as_u64(resp.get("metadata").and_then(|m| m.get("_total"))))
        .unwrap_or(files.len() as u64);
    let expire_at = parse_timestamp_secs(
        share
            .get("expired_at")
            .or_else(|| share.get("expire_at"))
            .or_else(|| data.get("expired_at")),
    );
    let note = if files.is_empty() {
        "服务端未返回文件明细（仍可尝试转存全部内容）".to_string()
    } else if total > files.len() as u64 {
        format!(
            "共 {total} 项，仅预览前 {} 项；转存会保存分享内全部内容",
            files.len()
        )
    } else {
        String::new()
    };
    ShareDetail {
        title,
        url: format_share_url(slug),
        password: password.to_string(),
        files,
        expire_at,
        note,
    }
}

fn is_own_share_error(code: Option<i64>, lower: &str) -> bool {
    code == Some(CODE_OWN_SHARE)
        || lower.contains("自己的分享")
        || lower.contains("转存自己")
        || lower.contains("own share")
}

fn is_passcode_error(lower: &str) -> bool {
    [
        "提取码",
        "访问码",
        "密码",
        "passcode",
        "invalid password",
        "wrong password",
        "incorrect password",
        "password error",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

fn is_share_gone(lower: &str) -> bool {
    [
        "分享不存在",
        "分享已失效",
        "分享已过期",
        "分享被取消",
        "分享已取消",
        "分享文件已删除",
        "分享已被删除",
        "share not found",
        "share not exist",
        "share expired",
        "share has expired",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// 把夸克分享相关业务错误翻译成可读错误。
///
/// 重点：
/// * `41017` = 禁止转存自己的分享 → 明确中文提示；
/// * 提取码/密码相关 → 提示检查提取码；
/// * 分享失效/过期/被删 → `NotFound`，便于 UI 区分。
pub(crate) fn share_error_from_parts(code: Option<i64>, msg: &str, had_passcode: bool) -> Error {
    let msg = msg.trim();
    let lower = msg.to_lowercase();
    if is_own_share_error(code, &lower) {
        return Error::other(
            "不能转存自己的分享：夸克服务端禁止转存自己创建的分享（错误码 41017）。\
             请换用其他账号转存，或直接下载后再上传。",
        );
    }
    if is_passcode_error(&lower) {
        let detail = if msg.is_empty() {
            "服务端拒绝访问该分享".to_string()
        } else {
            msg.to_string()
        };
        return Error::other(if had_passcode {
            format!("提取码校验失败：{detail}。请检查提取码是否正确，或分享是否已失效。")
        } else {
            format!("该分享需要提取码或提取码不正确：{detail}。请填写提取码后重试。")
        });
    }
    if is_share_gone(&lower) {
        return Error::not_found(if msg.is_empty() {
            "分享不存在、已过期或被取消".to_string()
        } else {
            format!("分享不存在、已过期或被取消：{msg}")
        });
    }

    let code_suffix = code
        .filter(|c| *c != 0)
        .map(|c| format!("（错误码 {c}）"))
        .unwrap_or_default();
    if msg.is_empty() {
        return if had_passcode {
            Error::other(format!(
                "提取码校验失败：服务端未返回分享访问令牌（stoken）{code_suffix}。\
                 请检查提取码，或确认分享未失效。"
            ))
        } else {
            Error::other(format!(
                "未获取到分享访问令牌（stoken）{code_suffix}：\
                 分享可能需要提取码，或链接已失效/被取消。"
            ))
        };
    }
    if let Some(code) = code.filter(|c| *c != 0) {
        Error::server(code, msg.to_string())
    } else {
        Error::other(msg.to_string())
    }
}

/// 对已有错误做分享语义增强；认证/网络等错误原样保留。
pub(crate) fn map_share_api_error(err: Error, had_passcode: bool) -> Error {
    match err {
        Error::Server { code, msg } => share_error_from_parts(Some(code), &msg, had_passcode),
        Error::NotFound(msg) => {
            if is_share_gone(&msg.to_lowercase()) {
                share_error_from_parts(None, &msg, had_passcode)
            } else {
                Error::NotFound(msg)
            }
        }
        other => other,
    }
}

impl QuarkDrive {
    pub(crate) async fn create_share_impl(
        &self,
        paths: &[String],
        opts: &ShareOptions,
    ) -> Result<ShareInfo> {
        self.api.ensure_valid()?;
        if paths.is_empty() {
            return Err(Error::other("创建分享失败：文件列表为空"));
        }
        let mut fids = Vec::new();
        for path in paths {
            let entry = self.resolve_retry(path, 2, 400).await?;
            if entry.id.is_empty() || entry.id == "0" {
                return Err(Error::other(format!("不能分享根目录：{path}")));
            }
            fids.push(entry.id);
        }

        let passcode = opts
            .password
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let mut body = json!({
            "fid_list": fids,
            "title": opts.title.trim(),
            "url_type": if passcode.is_some() { 2 } else { 1 },
            "expired_type": if opts.expire_days > 0 { 2 } else { 1 },
        });
        let expire_at = if opts.expire_days > 0 {
            let at = crate::now_ts() + i64::from(opts.expire_days) * 86_400;
            body["expired_at"] = json!(at * 1000);
            Some(at)
        } else {
            None
        };
        if let Some(code) = &passcode {
            body["passcode"] = json!(code);
        }

        let resp = self.api.post("share", body).await?;
        let data = data_of(&resp);
        let task_id = as_str(data.get("task_id"))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::other(format!(
                    "创建分享未返回 task_id：{}",
                    crate::compact_json(&resp)
                ))
            })?;
        let done = self
            .api
            .wait_task_timeout(&task_id, "创建分享", Duration::from_secs(60))
            .await?;
        let share_id = as_str(done.get("share_id"))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::other(format!(
                    "分享任务完成但未返回 share_id：{}",
                    crate::compact_json(&done)
                ))
            })?;

        let detail = self
            .api
            .post("share/password", json!({"share_id": share_id}))
            .await?;
        let detail = data_of(&detail);
        let url = as_str(detail.get("share_url"))
            .filter(|s| !s.is_empty())
            .or_else(|| {
                as_str(detail.get("pwd_id"))
                    .filter(|s| !s.is_empty())
                    .map(|s| format_share_url(&s))
            })
            .unwrap_or_default();
        let password = as_str(detail.get("passcode"))
            .filter(|s| !s.is_empty())
            .or(passcode)
            .unwrap_or_default();
        let title = as_str(detail.get("title"))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| opts.title.clone());

        let expire_at = parse_timestamp_secs(detail.get("expired_at")).or(expire_at);
        Ok(ShareInfo {
            share_id,
            url,
            password,
            expire_at,
            title,
        })
    }

    pub(crate) async fn list_shares_impl(&self) -> Result<Vec<ShareInfo>> {
        self.api.ensure_valid()?;
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for page in 1..=SHARE_PAGES {
            let params = vec![
                pair("_page", page.to_string()),
                pair("_size", SHARE_PAGE_SIZE.to_string()),
                pair("_order_field", "created_at"),
                pair("_order_type", "desc"),
                pair("_fetch_total", "1"),
                pair("_fetch_notify_follow", "1"),
            ];
            let resp = self.api.get("share/mypage/detail", params).await?;
            let data = data_of(&resp);
            let list = data
                .get("list")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let n = list.len();
            for raw in &list {
                if let Some(info) = map_share_info(raw) {
                    if seen.insert(info.share_id.clone()) {
                        out.push(info);
                    }
                }
            }
            if n < SHARE_PAGE_SIZE {
                break;
            }
        }
        Ok(out)
    }

    /// 回查「我的分享」中仍然存在的 share_id（最多 3 页 × 100）。
    async fn live_share_ids(&self, pages: usize) -> Result<HashSet<String>> {
        let mut live = HashSet::new();
        for page in 1..=pages.max(1) {
            let params = vec![
                pair("_page", page.to_string()),
                pair("_size", "100".to_string()),
                pair("_order_field", "created_at"),
                pair("_order_type", "desc"),
                pair("_fetch_total", "1"),
                pair("_fetch_notify_follow", "1"),
            ];
            let resp = self.api.get("share/mypage/detail", params).await?;
            let data = data_of(&resp);
            let list = data
                .get("list")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let n = list.len();
            for raw in &list {
                if let Some(id) = as_str(raw.get("share_id")) {
                    if !id.is_empty() {
                        live.insert(id);
                    }
                }
            }
            if n < 100 {
                break;
            }
        }
        Ok(live)
    }

    pub(crate) async fn cancel_shares_impl(&self, ids: &[String]) -> Result<OpReport> {
        self.api.ensure_valid()?;
        let ids: Vec<String> = ids
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let mut report = OpReport::default();
        if ids.is_empty() {
            return Ok(report);
        }

        self.api
            .post("share/delete", delete_share_body(&ids))
            .await?;

        // 列表有滞后：最多回查 4 次（每次 1.5 秒）。
        let mut verified = false;
        let mut remaining: HashSet<String> = ids.iter().cloned().collect();
        let mut verify_error: Option<String> = None;
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            match self.live_share_ids(3).await {
                Ok(live) => {
                    verified = true;
                    remaining = ids
                        .iter()
                        .filter(|id| live.contains(*id))
                        .cloned()
                        .collect();
                    if remaining.is_empty() {
                        break;
                    }
                }
                Err(e) => {
                    verify_error = Some(wp_core::error::err_line(&e));
                    break;
                }
            }
        }

        for id in &ids {
            if let Some(err) = &verify_error {
                report.fail(format!(
                    "{id}：已提交取消，但回查分享列表失败，无法确认（{err}）"
                ));
            } else if verified && !remaining.contains(id) {
                report.success();
            } else {
                report.fail(format!(
                    "{id}：服务端返回成功但回查仍存在（取消是静默 no-op？）"
                ));
            }
        }
        if remaining.is_empty() && verified {
            wp_core::log_info!("夸克", "已确认取消 {} 条分享", report.ok);
        } else if !verify_error.is_some() {
            wp_core::log_warn!(
                "夸克",
                "取消分享后仍有 {} 条存活，已如实标记失败",
                remaining.len()
            );
        }
        Ok(report)
    }

    // ───────────────────── 分享校验 / 转存 ─────────────────────

    /// `POST share/sharepage/token` → `data.stoken`。
    ///
    /// 提取码错误时夸克可能返回业务错误，也可能 200 但 `data` 里没有 stoken；
    /// 两种情况都按可读错误上报。
    async fn fetch_share_token(&self, slug: &str, passcode: &str) -> Result<String> {
        let had_passcode = !passcode.trim().is_empty();
        let body = json!({
            "pwd_id": slug,
            "passcode": passcode,
            "support_visit_limit_private_share": true,
        });
        let resp = self
            .api
            .post_at(SHARE_API_BASE, "share/sharepage/token", body)
            .await
            .map_err(|e| map_share_api_error(e, had_passcode))?;
        let data = data_of(&resp);
        if let Some(stoken) = as_str(data.get("stoken"))
            .or_else(|| as_str(resp.get("stoken")))
            .filter(|s| !s.trim().is_empty())
        {
            return Ok(stoken);
        }
        let code = as_i64(data.get("code"))
            .or_else(|| as_i64(resp.get("code")))
            .filter(|c| *c != 0);
        let msg = as_str(data.get("message"))
            .or_else(|| as_str(data.get("msg")))
            .or_else(|| as_str(resp.get("message")))
            .or_else(|| as_str(resp.get("msg")))
            .unwrap_or_default();
        Err(share_error_from_parts(code, &msg, had_passcode))
    }

    /// `GET share/sharepage/detail`（分享域）。
    async fn fetch_share_detail_raw(
        &self,
        slug: &str,
        stoken: &str,
        had_passcode: bool,
    ) -> Result<Value> {
        let params = share_detail_params(slug, stoken, 1, SHARE_DETAIL_PAGE_SIZE);
        self.api
            .get_at(SHARE_API_BASE, "share/sharepage/detail", params)
            .await
            .map_err(|e| map_share_api_error(e, had_passcode))
    }

    /// 校验分享链接并预览内容（token → detail）。
    pub(crate) async fn share_info_impl(&self, url: &str, password: &str) -> Result<ShareDetail> {
        self.api.ensure_valid()?;
        let (slug, link_code) = parse_share_link(url)?;
        let passcode = effective_passcode(password, link_code.as_deref());
        let stoken = self.fetch_share_token(&slug, &passcode).await?;
        let resp = self
            .fetch_share_detail_raw(&slug, &stoken, !passcode.is_empty())
            .await?;
        Ok(map_share_detail(&resp, &slug, &passcode))
    }

    /// 转存分享内全部内容到 `dest_dir`（不存在则逐级创建）。
    ///
    /// 返回 [`OpReport`]：成功记 1 项转存任务；任务阶段失败时把可读原因写进
    /// `errors`（认证/网络错误仍以 `Err` 上抛，避免把「登录失效」误报成转存失败）。
    pub(crate) async fn save_shared_impl(
        &self,
        url: &str,
        password: &str,
        dest_dir: &str,
    ) -> Result<OpReport> {
        self.api.ensure_valid()?;
        let (slug, link_code) = parse_share_link(url)?;
        let passcode = effective_passcode(password, link_code.as_deref());
        let had_passcode = !passcode.is_empty();

        // 目标目录必须先解析/创建成 fid；根目录 fid=0。
        let dest = self.ensure_dir_entry(dest_dir).await?;
        let dest_fid = if dest.id.trim().is_empty() {
            "0".to_string()
        } else {
            dest.id
        };

        let stoken = self.fetch_share_token(&slug, &passcode).await?;
        let body = save_shared_body(&slug, &stoken, &dest_fid);
        let resp = self
            .api
            .post_at(SHARE_API_BASE, "share/sharepage/save", body)
            .await
            .map_err(|e| map_share_api_error(e, had_passcode))?;
        let data = data_of(&resp);
        let task_id = as_str(data.get("task_id"))
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| Error::other(format!("转存未返回 task_id：{}", compact_json(&resp))))?;

        match self
            .api
            .wait_task_timeout(
                &task_id,
                "转存",
                Duration::from_secs(SHARE_SAVE_TIMEOUT_SECS),
            )
            .await
        {
            Ok(_) => {
                // 目标目录刚写入了新条目，清缓存避免列表看不到。
                self.invalidate_cache();
                Ok(OpReport {
                    ok: 1,
                    ..Default::default()
                })
            }
            Err(e) => {
                let mapped = map_share_api_error(e, had_passcode);
                if mapped.is_auth() || matches!(mapped.kind(), ErrorKind::Network | ErrorKind::Io) {
                    return Err(mapped);
                }
                let mut report = OpReport::default();
                report.fail(format!("转存失败：{mapped}"));
                Ok(report)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_share_links() {
        let (id, code) =
            parse_share_link("https://pan.quark.cn/s/292d917b145c 密码：1234").unwrap();
        assert_eq!(id, "292d917b145c");
        assert_eq!(code.as_deref(), Some("1234"));

        let (id, code) = parse_share_link("quark://share/abc123def456").unwrap();
        assert_eq!(id, "abc123def456");
        assert_eq!(code, None);

        let (id, code) =
            parse_share_link("https://pan.quark.cn/s/abcdef123456?pwd=9999 提取码: abcd").unwrap();
        assert_eq!(id, "abcdef123456");
        assert_eq!(code.as_deref(), Some("abcd"));

        let (id, _) = parse_share_link("abcdef123456").unwrap();
        assert_eq!(id, "abcdef123456");
        assert!(parse_share_link("这不是链接").is_err());
        assert!(parse_share_link("").is_err());

        // 带 Query 的提取码（pwd=），以及省略协议的链接。
        let (id, code) = parse_share_link("https://pan.quark.cn/s/abc123def456?pwd=9876").unwrap();
        assert_eq!(id, "abc123def456");
        assert_eq!(code.as_deref(), Some("9876"));

        let (id, code) = parse_share_link("pan.quark.cn/s/abc123def456 提取码：4321").unwrap();
        assert_eq!(id, "abc123def456");
        assert_eq!(code.as_deref(), Some("4321"));

        // 5 位以上的码不截断成 4 位。
        let (_, code) = parse_share_link("https://pan.quark.cn/s/abc123def456?pwd=12345").unwrap();
        assert_eq!(code, None);

        // slug 本身含 "code" 时不能被误当成提取码。
        let (id, code) = parse_share_link("https://pan.quark.cn/s/code1234").unwrap();
        assert_eq!(id, "code1234");
        assert_eq!(code, None);
    }

    #[test]
    fn effective_passcode_prefers_explicit_argument() {
        assert_eq!(effective_passcode(" 8888 ", Some("1234")), "8888");
        assert_eq!(effective_passcode("", Some("1234")), "1234");
        assert_eq!(effective_passcode("   ", None), "");
    }

    #[test]
    fn share_detail_params_and_save_body_contract() {
        let params = share_detail_params("slug-1", "stoken-1", 1, 50);
        let get = |key: &str| {
            params
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
                .unwrap_or("")
        };
        assert_eq!(get("pwd_id"), "slug-1");
        assert_eq!(get("stoken"), "stoken-1");
        assert_eq!(get("pdir_fid"), "0");
        assert_eq!(get("_page"), "1");
        assert_eq!(get("_size"), "50");
        assert_eq!(get("_fetch_total"), "1");
        assert_eq!(get("_sort"), "file_type:asc,file_name:asc");

        let body = save_shared_body("slug-1", "stoken-1", "123");
        assert_eq!(body["pwd_id"], "slug-1");
        assert_eq!(body["stoken"], "stoken-1");
        assert_eq!(body["to_pdir_fid"], "123");
        assert_eq!(body["pdir_save_all"], true);
        assert_eq!(body["scene"], "link");
        assert!(body["fid_list"].as_array().unwrap().is_empty());
        assert!(body["fid_token_list"].as_array().unwrap().is_empty());
        // 分享域接口不能错用 file/delete 的字段。
        assert!(body.get("current_dir_fid").is_none());

        // 空目标目录回退到根 fid=0。
        let root = save_shared_body("slug-1", "stoken-1", "  ");
        assert_eq!(root["to_pdir_fid"], "0");
    }

    #[test]
    fn map_share_detail_fields_and_note() {
        let resp = json!({
            "data": {
                "total": 100,
                "share": {
                    "title": "我的分享",
                    "expired_at": 1789322627000i64
                },
                "list": [
                    {"fid": "f1", "file_name": "电影.mp4", "file_type": 1, "size": 123},
                    {"fid": "d1", "file_name": "目录", "dir": true}
                ]
            }
        });
        let d = map_share_detail(&resp, "abcdef123456", "9999");
        assert_eq!(d.title, "我的分享");
        assert_eq!(d.url, "https://pan.quark.cn/s/abcdef123456");
        assert_eq!(d.password, "9999");
        assert_eq!(d.files.len(), 2);
        assert!(!d.files[0].is_dir);
        assert_eq!(d.files[0].path, "/电影.mp4");
        assert!(d.files[1].is_dir);
        assert_eq!(d.expire_at, Some(1789322627));
        assert!(d.note.contains("共 100 项"));
        assert!(d.note.contains("仅预览前 2 项"));

        // 没有 share 标题时用首个文件名；列表为空时给出可读说明。
        let only_file = json!({"data": {"list": [{"fid": "f", "file_name": "one.txt"}]}});
        let d = map_share_detail(&only_file, "slug-x", "");
        assert_eq!(d.title, "one.txt");
        assert!(d.note.is_empty());
        assert_eq!(d.url, "https://pan.quark.cn/s/slug-x");

        let empty = json!({"data": {"share": {"share_name": "空分享"}}});
        let d = map_share_detail(&empty, "slug-y", "");
        assert_eq!(d.title, "空分享");
        assert!(d.files.is_empty());
        assert!(!d.note.is_empty());
    }

    #[test]
    fn share_error_mapping_is_readable() {
        // 自己的分享：41017 必须映射为中文提示。
        let own = map_share_api_error(Error::server(41017, "用户禁止转存自己的分享"), true);
        assert_eq!(own.kind(), ErrorKind::Other);
        let text = own.to_string();
        assert!(text.contains("自己的分享"));
        assert!(text.contains("41017"));

        // 提取码错误。
        let code = map_share_api_error(Error::server(400, "提取码错误"), true);
        assert!(code.to_string().contains("提取码校验失败"));

        // 分享失效 → NotFound。
        let gone = map_share_api_error(Error::not_found("share expired"), false);
        assert_eq!(gone.kind(), ErrorKind::NotFound);

        // 认证错误不能被改写成业务错误。
        let auth = map_share_api_error(Error::auth("登录已过期"), true);
        assert!(auth.is_auth());
    }

    #[test]
    fn missing_stoken_errors_are_actionable() {
        let with_code = share_error_from_parts(None, "", true).to_string();
        assert!(with_code.contains("提取码"));
        assert!(with_code.contains("stoken"));

        let without_code = share_error_from_parts(None, "", false).to_string();
        assert!(without_code.contains("需要提取码"));
        assert!(without_code.contains("失效"));

        // 有业务码时也带上码值，方便用户/日志排查。
        let coded = share_error_from_parts(Some(41004), "", true).to_string();
        assert!(coded.contains("41004"));
    }

    #[test]
    fn share_url_and_delete_body() {
        assert_eq!(
            format_share_url(" 292d917b145c "),
            "https://pan.quark.cn/s/292d917b145c"
        );
        let body = delete_share_body(&["a".into(), "b".into()]);
        assert_eq!(body["share_ids"].as_array().unwrap().len(), 2);
        // 关键：字段名是 share_ids 数组，不能是 share_id 标量。
        assert!(body.get("share_id").is_none());
    }

    #[test]
    fn map_share_list_item() {
        let raw = json!({
            "share_id": "61fdd052e98b42d9933e3a011dfdf4f8",
            "pwd_id": "292d917b145c",
            "share_url": "https://pan.quark.cn/s/292d917b145c",
            "title": "电影",
            "status": 1,
            "expired_at": 1789322627000i64,
            "passcode": ""
        });
        let info = map_share_info(&raw).unwrap();
        assert_eq!(info.share_id, "61fdd052e98b42d9933e3a011dfdf4f8");
        assert_eq!(info.url, "https://pan.quark.cn/s/292d917b145c");
        assert_eq!(info.expire_at, Some(1789322627));
        assert!(info.password.is_empty());

        let raw = json!({"share_id":"s1","pwd_id":"slug1"});
        let info = map_share_info(&raw).unwrap();
        assert_eq!(info.url, "https://pan.quark.cn/s/slug1");
        assert!(info.expire_at.is_none());

        assert!(map_share_info(&json!({"pwd_id":"x"})).is_none());
    }
}
