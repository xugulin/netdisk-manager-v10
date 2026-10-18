//! 分享：创建 / 列表 / 取消。
//!
//! 文档 §4.4 的实测坑：
//! * 创建分享的 `pwd` **必需且必须 4 位**，空串会得 `pwd length param error`；
//!   响应**不回显 pwd**，调用方必须自己记住；
//! * `fid_list` 是 `fs_id` 数组，不是路径；
//! * 列表字段名易混：`shareId`（大写 I，对外 ID）≠ `shareid`，
//!   `passwd`（提取码）≠ `pwd`，`shortlink`（完整链接）≠ `shorturl`（仅短码）；
//! * 列表终止条件是 `nextpage == 0`，不是“本页是否满”；
//! * 取消分享用 `shareid`（不是 fs_id）。

use crate::files;
use crate::http::ReqOpts;
use crate::BaiduDrive;
use serde_json::{json, Value};
use wp_core::model::{OpReport, ShareInfo};
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
}
