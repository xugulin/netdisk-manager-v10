//! 云添加（离线下载）任务：**只读** `method=list_task`。
//!
//! 为什么只做列表、不做“新建任务”：
//! * HAR 只抓到 `list_task` / `query_task` 两条**只读**请求，创建任务所需的
//!   `source_url / save_path / od_type / method=add_task …` 参数没有样本；
//!   Python 源项目也把「云添加」整块删除（PLAN.md：协议未摸清）。
//! * 因此 `cloud_add` 保持 trait 默认的 Unsupported（caps=false），
//!   不在生产代码里猜参数。
//!
//! 列表请求完全按 HAR 实测参数还原：
//! ```text
//! GET /rest/2.0/services/cloud_dl
//!     ?start=0&limit=100&need_task_info=1&status=255
//!     &method=list_task&t=<毫秒>&bdstoken=...
//! ```
//! 响应 `task_info` 为数组，元素字段：
//! `task_id / task_name / source_url / save_path / status / create_time / od_type`。
//!
//! ⚠️ 百度任务状态语义与 `wp_core::model::cloud_status_text()` 的**通用表相反**：
//! 百度 `0 = 已完成`，而通用表把 `"0"` 当作「等待中」。HAR 的 `query_task`
//! 样本可交叉验证：所有 `status=0` 的任务 `finished_size == file_size` 且
//! `finish_time > 0`；`status=4` 的任务 `finished_size=0`。所以这里用百度自己的
//! 映射，不能直接套通用函数。

use crate::http::ReqOpts;
use crate::{err_text, now_ms, BaiduDrive};
use serde_json::Value;
use wp_core::model::CloudTask;
use wp_core::util::base_name;
use wp_core::Result;

/// 单页任务数（HAR 为 100）。
const PAGE_SIZE: usize = 100;
/// 最多翻 10 页（1000 个任务），避免异常响应导致死循环。
const MAX_PAGES: usize = 10;

/// 任务 id / 名称等字段：字符串或数字都接受。
fn field_text(v: &Value, keys: &[&str]) -> String {
    for key in keys {
        let Some(raw) = v.get(*key) else { continue };
        if let Some(s) = raw.as_str() {
            let s = s.trim();
            if !s.is_empty() {
                return s.to_string();
            }
        } else if let Some(n) = raw.as_i64() {
            return n.to_string();
        } else if let Some(n) = raw.as_u64() {
            return n.to_string();
        }
    }
    String::new()
}

/// 宽松数字解析：HAR 里 `file_size` / `finished_size` 是字符串。
fn loose_u64(v: &Value, keys: &[&str]) -> u64 {
    for key in keys {
        let Some(raw) = v.get(*key) else { continue };
        if let Some(n) = raw.as_u64() {
            return n;
        }
        if let Some(n) = raw.as_i64() {
            return n.max(0) as u64;
        }
        if let Some(s) = raw.as_str() {
            if let Ok(n) = s.trim().parse::<u64>() {
                return n;
            }
        }
    }
    0
}

fn loose_i64(v: &Value, keys: &[&str]) -> Option<i64> {
    for key in keys {
        let Some(raw) = v.get(*key) else { continue };
        if let Some(n) = raw.as_i64() {
            return Some(n);
        }
        if let Some(s) = raw.as_str() {
            if let Ok(n) = s.trim().parse::<i64>() {
                return Some(n);
            }
        }
    }
    None
}

fn loose_f64(v: &Value, key: &str) -> Option<f64> {
    let raw = v.get(key)?;
    if let Some(n) = raw.as_f64() {
        return Some(n);
    }
    raw.as_str().and_then(|s| s.trim().parse::<f64>().ok())
}

/// 百度任务状态 → 中文。
///
/// 0..=4 依百度云离线下载的公开状态约定；HAR 样本验证了 0（完成）与
/// 4（未开始下载）。未知值原样带回，方便真机联调时发现新状态。
pub(crate) fn status_text(code: &str) -> String {
    match code.trim() {
        "0" => "已完成".into(),
        "1" => "失败".into(),
        "2" => "已取消".into(),
        "3" => "下载中".into(),
        "4" => "等待中".into(),
        "" => "未知".into(),
        other => format!("状态 {other}"),
    }
}

/// 单个 `task_info` 元素 → 统一模型。
///
/// `fallback_id` 用于 `query_task` 那种「以 task_id 为键的对象」响应。
pub(crate) fn task_from_value(v: &Value, fallback_id: Option<&str>) -> Option<CloudTask> {
    let id = {
        let text = field_text(v, &["task_id", "taskId", "id"]);
        if text.is_empty() {
            fallback_id.unwrap_or("").trim().to_string()
        } else {
            text
        }
    };
    if id.is_empty() {
        return None;
    }
    let status = field_text(v, &["status"]);
    let save_path = field_text(v, &["save_path", "savePath", "path"]);
    let url = field_text(v, &["source_url", "sourceUrl", "url"]);
    let mut name = field_text(v, &["task_name", "taskName", "name", "server_filename"]);
    if name.is_empty() {
        name = base_name(&save_path);
    }
    if name.is_empty() {
        name = format!("任务 {id}");
    }

    let total = loose_u64(v, &["file_size", "fileSize", "total", "size"]);
    let done = loose_u64(v, &["finished_size", "finishedSize", "done", "downloaded"]);
    let progress = match loose_f64(v, "progress") {
        Some(raw) => {
            // query_task 的 progress 是 0..100；本机没有见过 0..1 的写法，
            // 但兼容一下，避免把 50 当成 5000%。
            if raw > 1.0 {
                raw / 100.0
            } else {
                raw
            }
        }
        None if total > 0 => done as f64 / total as f64,
        None => 0.0,
    }
    .clamp(0.0, 1.0) as f32;

    let created = loose_i64(v, &["create_time", "createTime", "ctime"]).filter(|t| *t > 0);
    let error = {
        let text = err_text(v, "");
        // err_text 在没有错误字段时会回退空串；只有真的写了 message 才展示，
        // 避免把正常任务的 show_msg="success" 当错误。
        if text.is_empty() || text.eq_ignore_ascii_case("success") {
            String::new()
        } else {
            text
        }
    };

    Some(CloudTask {
        id,
        name,
        url,
        status: status.clone(),
        status_text: status_text(&status),
        progress,
        total,
        done,
        created,
        save_path,
        error,
    })
}

/// 解析 `task_info`：既支持 list_task 的数组，也支持 query_task 的对象。
pub(crate) fn tasks_from_response(v: &Value) -> Vec<CloudTask> {
    match v.get("task_info") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| task_from_value(item, None))
            .collect(),
        Some(Value::Object(map)) => map
            .iter()
            .filter_map(|(id, item)| task_from_value(item, Some(id)))
            .collect(),
        _ => Vec::new(),
    }
}

/// 云任务列表（只读；`status=255` = 全部状态）。
pub(crate) async fn list_tasks(d: &BaiduDrive) -> Result<Vec<CloudTask>> {
    let mut out: Vec<CloudTask> = Vec::new();
    let mut start: u64 = 0;
    for _ in 0..MAX_PAGES {
        let params = vec![
            ("start".to_string(), start.to_string()),
            ("limit".to_string(), PAGE_SIZE.to_string()),
            ("need_task_info".to_string(), "1".to_string()),
            ("status".to_string(), "255".to_string()),
            ("method".to_string(), "list_task".to_string()),
            ("t".to_string(), now_ms().to_string()),
        ];
        let v = d
            .api_get(
                "/rest/2.0/services/cloud_dl",
                &params,
                ReqOpts::authed().no_channel().with_bdstoken(),
            )
            .await?;
        let items = v.get("task_info").and_then(Value::as_array);
        let count = items.map(|a| a.len()).unwrap_or(0);
        let parsed = tasks_from_response(&v);
        let fetched = parsed.len();
        out.extend(parsed);
        let total = v.get("total").and_then(Value::as_u64).unwrap_or(0);
        if count < PAGE_SIZE {
            break;
        }
        if total > 0 && start + fetched as u64 >= total {
            break;
        }
        start += PAGE_SIZE as u64;
    }
    wp_core::log_debug!("百度", "云任务列表 → {} 项", out.len());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn list_task_entry_mapping_from_har() {
        // 字段形状取自 HAR `cloud_dl?method=list_task` 实测响应。
        let v = json!({
            "task_id": "6273246743",
            "od_type": "0",
            "source_url": "https://example.invalid/mabox.iso",
            "save_path": "/我的资源/mabox.iso",
            "status": "0",
            "create_time": "1767525807",
            "task_name": "mabox.iso",
            "orgin_web_url": ""
        });
        let t = task_from_value(&v, None).unwrap();
        assert_eq!(t.id, "6273246743");
        assert_eq!(t.name, "mabox.iso");
        assert_eq!(t.url, "https://example.invalid/mabox.iso");
        assert_eq!(t.status, "0");
        assert_eq!(t.status_text, "已完成");
        assert_eq!(t.created, Some(1767525807));
        assert_eq!(t.save_path, "/我的资源/mabox.iso");
        assert!(t.error.is_empty());
        assert_eq!(t.progress, 0.0);
    }

    #[test]
    fn query_task_object_form_and_progress() {
        let v = json!({
            "task_info": {
                "1647018570": {
                    "status": "0",
                    "file_size": "4919256312",
                    "finished_size": "4919256312",
                    "create_time": "1478781414",
                    "save_path": "/一般视频/电影/",
                    "source_url": "magnet:?xt=urn:btih:abc",
                    "task_name": "电影"
                },
                "1681710176": {
                    "status": "4",
                    "file_size": "1066791369",
                    "finished_size": "0",
                    "create_time": "1481124027",
                    "save_path": "/图片-文本/",
                    "source_url": "magnet:?xt=urn:btih:def",
                    "task_name": "借贷宝"
                }
            }
        });
        let tasks = tasks_from_response(&v);
        assert_eq!(tasks.len(), 2);
        let done = tasks.iter().find(|t| t.id == "1647018570").unwrap();
        assert_eq!(done.status_text, "已完成");
        assert_eq!(done.total, 4919256312);
        assert_eq!(done.done, done.total);
        assert_eq!(done.progress, 1.0);
        let waiting = tasks.iter().find(|t| t.id == "1681710176").unwrap();
        assert_eq!(waiting.status_text, "等待中");
        assert_eq!(waiting.progress, 0.0);
    }

    #[test]
    fn progress_percent_and_unknown_status_are_safe() {
        let v = json!({
            "task_id": 1,
            "status": "3",
            "progress": "25",
            "file_size": "1000",
            "finished_size": "250"
        });
        let t = task_from_value(&v, None).unwrap();
        assert_eq!(t.status_text, "下载中");
        assert!((t.progress - 0.25).abs() < 1e-6);
        assert_eq!(status_text("9"), "状态 9");
        assert_eq!(status_text(""), "未知");
        // 没有 task_id 的脏条目丢弃。
        assert!(task_from_value(&json!({"status": "0"}), None).is_none());
    }
}
