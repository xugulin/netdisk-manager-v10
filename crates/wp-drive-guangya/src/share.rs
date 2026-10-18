//! 分享：创建 / 列表 / 取消。
//!
//! 关键差异（协议 §4.7）：
//! * 取消分享必须用响应里的纯数字 `id`（不是带 `_userId` 后缀的 `shareId`）；
//! * 分享 URL 优先用服务端 `shareUrl`，不要在客户端重复拼 `_userId`；
//! * `shareType`：0 无密码 / 2 自定义密码（1 随机密码没有 UI 入口）。
//! * `trafficLimit` 是字符串字节数（默认 "0"）。

use serde_json::{json, Value};
use wp_core::model::{OpReport, ShareInfo};
use wp_core::util::{base_name, now_ts};
use wp_core::{Error, Result};
use wp_drive::ShareOptions;

use crate::{as_i64, data_or_self, files, GuangyaDrive};

/// 创建分享。
pub(crate) async fn create_share(
    drive: &GuangyaDrive,
    paths: &[String],
    opts: &ShareOptions,
) -> Result<ShareInfo> {
    if paths.is_empty() {
        return Err(Error::other("分享文件列表不能为空"));
    }
    let mut ids = Vec::new();
    for path in paths {
        let entry = files::resolve_entry(drive, path).await?;
        if entry.id.is_empty() {
            return Err(Error::other("不能分享根目录"));
        }
        ids.push(entry.id);
    }
    let title = if opts.title.trim().is_empty() {
        base_name(&paths[0])
    } else {
        opts.title.trim().to_string()
    };
    let body = build_share_body(&ids, &title, opts.expire_days, opts.password.as_deref());
    let value = drive.api_post("/userres/v1/share_file", body, &[]).await?;
    let data = data_or_self(&value);
    if data.get("shareUrl").and_then(Value::as_str).is_none() {
        return Err(Error::server(
            0,
            format!("创建分享失败：{}", crate::snippet(&value.to_string(), 300)),
        ));
    }
    Ok(parse_share(data, &title, opts.password.as_deref()))
}

/// 列出全部分享（分页 50）。
pub(crate) async fn list_shares(drive: &GuangyaDrive) -> Result<Vec<ShareInfo>> {
    let mut out = Vec::new();
    let mut page = 0u32;
    loop {
        let body = json!({ "page": page, "pageSize": 50 });
        let value = drive
            .api_post("/userres/v1/get_share_list", body, &[])
            .await?;
        let data = data_or_self(&value);
        let parsed = parse_share_list(data);
        let count = parsed.len();
        out.extend(parsed);
        let total = data.get("total").and_then(as_i64).unwrap_or(-1);
        if count < 50 || (total >= 0 && out.len() as i64 >= total) {
            break;
        }
        page += 1;
        if page > 100 {
            break;
        }
    }
    Ok(out)
}

/// 取消分享（`id` 允许带后缀，会自动取纯数字前缀）。
pub(crate) async fn cancel_shares(drive: &GuangyaDrive, ids: &[String]) -> Result<OpReport> {
    let clean: Vec<String> = ids
        .iter()
        .map(|s| share_numeric_id(s))
        .filter(|s| !s.is_empty())
        .collect();
    if clean.is_empty() {
        return Err(Error::other("没有要取消的分享 ID"));
    }
    let body = json!({ "ids": clean });
    drive
        .api_post("/userres/v1/delete_share", body, &[])
        .await?;
    let mut report = OpReport::default();
    for _ in 0..clean.len() {
        report.success();
    }
    Ok(report)
}

/// 创建分享请求体（纯逻辑，便于单测）。
pub(crate) fn build_share_body(
    ids: &[String],
    title: &str,
    expire_days: u32,
    password: Option<&str>,
) -> Value {
    let mut body = json!({
        "fileIds": ids,
        "title": title,
        "validateDuration": expire_days_to_secs(expire_days),
        "shareType": 0,
        "trafficLimit": "0",
        "maxRestoreCount": 0,
        "downloadType": 1,
        "enableShareCode": false,
        "shareCode": "",
    });
    let pwd = password.unwrap_or("").trim();
    if !pwd.is_empty() {
        body["shareType"] = json!(2);
        body["code"] = json!(pwd);
        body["autoFillCode"] = json!(false);
    }
    body
}

/// 有效期天数 → 秒；0 = 永久。
pub(crate) fn expire_days_to_secs(days: u32) -> i64 {
    if days == 0 {
        0
    } else {
        (days as i64).saturating_mul(86_400)
    }
}

/// 分享条目映射。
pub(crate) fn parse_share(v: &Value, fallback_title: &str, password: Option<&str>) -> ShareInfo {
    let raw_id = str_field(v, &["id"]);
    let share_id = if raw_id.is_empty() {
        str_field(v, &["shareId"])
    } else {
        raw_id
    };
    let share_url = str_field(v, &["shareUrl"]);
    let url = if !share_url.is_empty() {
        share_url
    } else {
        let sid = {
            let s = str_field(v, &["shareId"]);
            if s.is_empty() {
                share_id.clone()
            } else {
                s
            }
        };
        if sid.is_empty() {
            String::new()
        } else {
            format!("https://www.guangyapan.com/s/{sid}")
        }
    };
    let code = {
        let c = str_field(v, &["code"]);
        if !c.is_empty() {
            c
        } else {
            password.unwrap_or("").trim().to_string()
        }
    };
    let create_time = v.get("createTime").and_then(as_i64);
    let duration = v.get("validateDuration").and_then(as_i64).unwrap_or(0);
    let expire_at = if duration > 0 {
        Some(create_time.unwrap_or_else(now_ts) + duration)
    } else {
        None
    };
    let title = {
        let t = str_field(v, &["title"]);
        if t.is_empty() {
            fallback_title.to_string()
        } else {
            t
        }
    };
    ShareInfo {
        share_id,
        url,
        password: code,
        expire_at,
        title,
    }
}

/// 分享列表解析。
pub(crate) fn parse_share_list(data: &Value) -> Vec<ShareInfo> {
    data.get("list")
        .and_then(Value::as_array)
        .map(|list| list.iter().map(|v| parse_share(v, "", None)).collect())
        .unwrap_or_default()
}

/// 取消分享用的纯数字 ID：`123_u456` → `123`。
pub(crate) fn share_numeric_id(raw: &str) -> String {
    let s = raw.trim();
    let prefix = s.split('_').next().unwrap_or(s).trim();
    if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
        prefix.to_string()
    } else {
        s.to_string()
    }
}

fn str_field(v: &Value, keys: &[&str]) -> String {
    for k in keys {
        let Some(val) = v.get(*k) else { continue };
        let s = match val {
            Value::String(s) => s.trim().to_string(),
            Value::Number(n) => n.to_string(),
            _ => continue,
        };
        if !s.is_empty() {
            return s;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_body_password_and_expiry() {
        let ids = vec!["f1".to_string(), "f2".to_string()];
        let no_pwd = build_share_body(&ids, "标题", 0, None);
        assert_eq!(no_pwd["shareType"], 0);
        assert_eq!(no_pwd["validateDuration"], 0);
        assert_eq!(no_pwd["trafficLimit"], "0");
        assert!(no_pwd.get("code").is_none());
        assert!(no_pwd.get("autoFillCode").is_none());

        let pwd = build_share_body(&ids, "标题", 7, Some(" 8888 "));
        assert_eq!(pwd["shareType"], 2);
        assert_eq!(pwd["code"], "8888");
        assert_eq!(pwd["autoFillCode"], false);
        assert_eq!(pwd["validateDuration"], 7 * 86_400);
        assert_eq!(pwd["fileIds"].as_array().unwrap().len(), 2);

        // 空白密码按无密码处理
        assert_eq!(build_share_body(&ids, "", 1, Some("   "))["shareType"], 0);
        assert_eq!(expire_days_to_secs(30), 2_592_000);
    }

    #[test]
    fn share_parsing() {
        let data = json!({
            "id":"123456789",
            "shareId":"123456789_u777",
            "shareUrl":"https://www.guangyapan.com/s/123456789_u777",
            "title":"电影",
            "code":"8888",
            "createTime":1700000000,
            "validateDuration":86400,
            "shareStatus":1
        });
        let s = parse_share(&data, "fallback", Some("ignored"));
        assert_eq!(s.share_id, "123456789");
        assert_eq!(s.url, "https://www.guangyapan.com/s/123456789_u777");
        assert_eq!(s.password, "8888");
        assert_eq!(s.expire_at, Some(1_700_086_400));
        assert_eq!(s.title, "电影");

        let data = json!({"shareId":"abc","validateDuration":0});
        let s = parse_share(&data, "回退标题", None);
        assert_eq!(s.share_id, "abc");
        assert_eq!(s.url, "https://www.guangyapan.com/s/abc");
        assert_eq!(s.title, "回退标题");
        assert_eq!(s.expire_at, None);
        assert!(s.password.is_empty());
    }

    #[test]
    fn share_list_parsing() {
        let data =
            json!({"list":[{"id":"1","shareUrl":"u1"},{"id":"2","shareUrl":"u2"}],"total":2});
        let list = parse_share_list(&data);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].url, "u2");
        assert!(parse_share_list(&json!({"list":null})).is_empty());
    }

    #[test]
    fn numeric_id_cleaning() {
        assert_eq!(share_numeric_id("123_u456"), "123");
        assert_eq!(share_numeric_id(" 123 "), "123");
        assert_eq!(share_numeric_id("abc"), "abc");
        assert_eq!(share_numeric_id(""), "");
    }
}
