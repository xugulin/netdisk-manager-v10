//! 分享：创建 / 列表 / 取消（含回查），以及分享链接解析。
//!
//! 两套 ID 不能混用：
//! * `share_id`：32 位 hex，管理接口（详情/取消）用；
//! * URL slug：12 位字母数字，`https://pan.quark.cn/s/<slug>`，转存接口用 `pwd_id`。
//!
//! 取消分享必须传 `{"share_ids": [...]}` 数组；传标量 `share_id` 会返回成功但
//! 什么都不删（静默 no-op），因此这里默认回查列表确认。

use crate::{as_str, data_of, pair, parse_timestamp_secs, QuarkDrive};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;
use wp_core::model::{OpReport, ShareInfo};
use wp_core::{Error, Result};
use wp_drive::ShareOptions;

const SHARE_PAGE_SIZE: usize = 100;
const SHARE_PAGES: usize = 5;

/// 从分享文案里解析 `(slug, 提取码)`。
///
/// Drive trait 没有「转存」能力，当前仅作为纯逻辑保留（含单元测试）；
/// 后续若增加转存 UI，可直接复用。
#[allow(dead_code)]
pub(crate) fn parse_share_link(text: &str) -> Result<(String, Option<String>)> {
    let t = text.trim();
    if t.is_empty() {
        return Err(Error::other("分享链接为空"));
    }
    let mut slug: Option<String> = None;
    for prefix in [
        "https://pan.quark.cn/s/",
        "http://pan.quark.cn/s/",
        "quark://share/",
    ] {
        if let Some(pos) = t.find(prefix) {
            let rest = &t[pos + prefix.len()..];
            let id: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if !id.is_empty() {
                slug = Some(id);
                break;
            }
        }
    }
    if slug.is_none() && t.len() >= 8 && t.chars().all(|c| c.is_ascii_alphanumeric()) {
        slug = Some(t.to_string());
    }
    let Some(slug) = slug else {
        return Err(Error::other(format!(
            "无法识别的分享链接：{}",
            t.chars().take(120).collect::<String>()
        )));
    };

    let mut code = None;
    let lower = t.to_ascii_lowercase();
    for kw in ["密码", "提取码", "访问码", "code"] {
        let pos = if kw == "code" {
            lower.find(kw)
        } else {
            t.find(kw)
        };
        if let Some(pos) = pos {
            let rest = &t[pos + kw.len()..];
            let rest =
                rest.trim_start_matches(|c: char| c == '：' || c == ':' || c.is_whitespace());
            let candidate: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .take(4)
                .collect();
            if candidate.chars().count() == 4 {
                code = Some(candidate);
                break;
            }
        }
    }
    Ok((slug, code))
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
}

// 转存（share/sharepage/token | detail | save）不在 Drive trait 的能力集内，
// 本 crate 暂不暴露；如后续需要，必须使用 https://drive.quark.cn/1/clouddrive
// 并处理「转存自己的分享 → 404 + 41017」以及 `fid_token_list` 待确认问题。

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
