//! 上传记录 / 操作记录：`POST /userres/v1/get_user_action`。
//!
//! 响应的 `data.list[]` 是「批次」，每条批次含 `actionType` 与
//! `actionDetails[]`；统一模型按明细扁平化。源码见
//! `核心/接口/文件接口.py:179-233`，字段与 UI 映射见
//! `界面/页面/上传记录页.py`。
//!
//! `excludeFileTypes:[2]` 排除目录；`cursor + hasMore` 翻页，`limit` 由调用方给。

use serde_json::{json, Value};
use wp_core::model::UploadRecord;
use wp_core::util::join_path;
use wp_core::Result;

use crate::files::{first_str, first_ts, first_u64};
use crate::{as_i64, data_or_self, GuangyaDrive, CLIENT_ID};

/// 服务端单页大小上限（源码 UI 默认 20）。
const MAX_PAGE_SIZE: usize = 100;
/// 防御性翻页上限（源码「迭代上传记录」最多 10 页）。
const MAX_PAGES: usize = 10;

/// 取最近 `limit` 条上传/操作记录（自动翻页后截断）。
pub(crate) async fn upload_records(
    drive: &GuangyaDrive,
    limit: usize,
) -> Result<Vec<UploadRecord>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let page_size = limit.clamp(1, MAX_PAGE_SIZE) as u32;
    let mut out = Vec::new();
    let mut cursor = String::new();
    for _ in 0..MAX_PAGES {
        let value = drive
            .api_post(
                "/userres/v1/get_user_action",
                records_body(page_size, &cursor),
                &[],
            )
            .await?;
        let data = data_or_self(&value);
        let page = records_from_data(data, limit - out.len());
        let count = page.len();
        out.extend(page);
        if out.len() >= limit {
            break;
        }
        let has_more = data
            .get("hasMore")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let next = data
            .get("cursor")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if !has_more || next.is_empty() || next == cursor || count == 0 {
            break;
        }
        cursor = next;
    }
    out.truncate(limit);
    Ok(out)
}

/// 上传记录请求体（`excludeFileTypes:[2]` 排除目录）。
pub(crate) fn records_body(page_size: u32, cursor: &str) -> Value {
    json!({
        "pageSize": page_size,
        "cursor": cursor,
        "excludeFileTypes": [2],
        "clientId": CLIENT_ID,
    })
}

/// 把 `data.list[]` 的批次结构扁平化，最多返回 `limit` 条。
pub(crate) fn records_from_data(data: &Value, limit: usize) -> Vec<UploadRecord> {
    let mut out = Vec::new();
    if limit == 0 {
        return out;
    }
    let Some(batches) = data.get("list").and_then(Value::as_array) else {
        return out;
    };
    'batches: for batch in batches {
        let action_type = as_i64(batch.get("actionType").unwrap_or(&Value::Null));
        let action = action_text(action_type);
        let batch_time = first_ts(batch, &["ctime", "createTime", "actionTime"]);
        let Some(details) = batch.get("actionDetails").and_then(Value::as_array) else {
            continue;
        };
        for detail in details {
            if let Some(record) = record_from_detail(detail, action, batch_time) {
                out.push(record);
                if out.len() >= limit {
                    break 'batches;
                }
            }
        }
    }
    out
}

/// `actionType` → 操作文本（0 未知 / 1 浏览 / 2 下载 / 3 上传 / 4 删除 / 5 分享）。
pub(crate) fn action_text(action_type: Option<i64>) -> &'static str {
    match action_type {
        Some(1) => "浏览",
        Some(2) => "下载",
        Some(3) => "上传",
        Some(4) => "删除",
        Some(5) => "分享",
        _ => "未知",
    }
}

fn record_from_detail(v: &Value, action: &str, batch_time: Option<i64>) -> Option<UploadRecord> {
    let name = first_str(v, &["fileName", "name"])?;
    let id = first_str(v, &["fileId", "id"]).unwrap_or_default();
    let parent = first_str(v, &["parentName", "parent"]).unwrap_or_default();
    // parentName 只是目录名、不是完整路径；没有父目录时留空。
    let path = if parent.is_empty() {
        String::new()
    } else {
        join_path(&join_path("/", &parent), &name)
    };
    Some(UploadRecord {
        id,
        name,
        path,
        size: first_u64(v, &["fileSize", "size"]).unwrap_or(0),
        time: first_ts(v, &["utime", "ctime", "actionTime", "time"]).or(batch_time),
        action: action.to_string(),
        parent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        json!({"list":[
            {"collectionId":"c1","ctime":1700000000,"actionType":3,"actionDetails":[
                {"fileId":123,"fileName":"a.mkv","fileSize":"1024",
                 "utime":1700000000123i64,"parentName":"电影"},
                {"fileId":"f2","fileName":"b.txt","fileSize":2,"utime":1700000200}
            ]},
            {"actionType":2,"ctime":1700000300,"actionDetails":[
                {"fileId":"f3","fileName":"c.bin","fileSize":3}
            ]},
            {"actionType":9,"actionDetails":[
                {"fileId":"f4","fileName":"d.zip"}
            ]}
        ]})
    }

    #[test]
    fn upload_record_mapping_flattens_batches() {
        let rows = records_from_data(&sample(), 100);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].id, "123", "数字 fileId 转字符串");
        assert_eq!(rows[0].name, "a.mkv");
        assert_eq!(rows[0].size, 1024);
        assert_eq!(rows[0].action, "上传");
        assert_eq!(rows[0].time, Some(1_700_000_000), "毫秒转秒");
        assert_eq!(rows[0].parent, "电影");
        assert_eq!(rows[0].path, "/电影/a.mkv");

        assert_eq!(rows[1].action, "上传");
        assert_eq!(rows[1].time, Some(1_700_000_200));
        assert_eq!(rows[1].path, "", "无 parentName 时路径留空");

        assert_eq!(rows[2].action, "下载");
        assert_eq!(
            rows[2].time,
            Some(1_700_000_300),
            "明细无时间时用批次 ctime"
        );
        assert_eq!(rows[3].action, "未知");
    }

    #[test]
    fn upload_record_limit_and_body() {
        assert_eq!(records_from_data(&sample(), 2).len(), 2);
        assert!(records_from_data(&sample(), 0).is_empty());
        assert!(records_from_data(&json!({"list":null}), 10).is_empty());

        let b = records_body(20, "abc");
        assert_eq!(b["pageSize"], 20);
        assert_eq!(b["cursor"], "abc");
        assert_eq!(b["excludeFileTypes"], json!([2]));
        assert_eq!(b["clientId"], crate::CLIENT_ID);
    }

    #[test]
    fn action_type_labels() {
        assert_eq!(action_text(Some(1)), "浏览");
        assert_eq!(action_text(Some(2)), "下载");
        assert_eq!(action_text(Some(3)), "上传");
        assert_eq!(action_text(Some(4)), "删除");
        assert_eq!(action_text(Some(5)), "分享");
        assert_eq!(action_text(Some(0)), "未知");
        assert_eq!(action_text(None), "未知");
    }
}
