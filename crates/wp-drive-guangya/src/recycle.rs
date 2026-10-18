//! 回收站：列表（`dirType=4`）与清空。
//!
//! 协议依据 `docs/research/光鸭_协议.md` §4.8 与 Python
//! `核心/接口/文件接口.py:246-283`：
//! * 列表与普通文件列表同端点，靠 `dirType=4 + orderBy=12 + sortType=1` 区分；
//! * 清空接口 `clear_recycle_bin` 明确不带 body，返回 `taskId`，
//!   再用 `get_task_status` 轮询到 `status==2`（源码 UI 最长 30s）。
//!
//! 源码没有单项恢复/彻底删除接口，所以本模块只提供列表与清空。

use serde_json::{json, Value};
use std::time::{Duration, Instant};
use wp_core::model::{OpReport, RecycleEntry};
use wp_core::{Error, Result};

use crate::files::{first_str, first_ts, first_u64};
use crate::{as_i64, data_or_self, GuangyaDrive, CLIENT_ID};

/// 回收站每页条数（源码默认 100）。
const PAGE_SIZE: u32 = 100;
/// 防御性上限，避免服务端 `hasMore` 异常导致死循环。
const MAX_PAGES: u32 = 200;
const DIR_TYPE_RECYCLE: i64 = 4;
const ORDER_BY_DELETE_TIME: i64 = 12;
const SORT_TYPE_DESC: i64 = 1;
/// 清空任务轮询（源码 UI：1s 间隔 / 最长 30s）。
const CLEAR_POLL_INTERVAL: Duration = Duration::from_secs(1);
const CLEAR_TIMEOUT: Duration = Duration::from_secs(30);

/// 拉取回收站全部条目（自动翻页）。
pub(crate) async fn list(drive: &GuangyaDrive) -> Result<Vec<RecycleEntry>> {
    let mut out = Vec::new();
    for page in 0..MAX_PAGES {
        let value = drive
            .api_post(
                "/userres/v1/file/get_file_list",
                list_body(page, PAGE_SIZE),
                &[],
            )
            .await?;
        let data = data_or_self(&value);
        let entries = entries_from_data(data);
        let count = entries.len();
        out.extend(entries);
        let has_more = data.get("hasMore").and_then(Value::as_bool);
        if count == 0 || has_more == Some(false) {
            break;
        }
        // 没有 hasMore 时按“短页即最后一页”判断。
        if has_more.is_none() && count < PAGE_SIZE as usize {
            break;
        }
        if let Some(total) = data.get("total").and_then(as_i64) {
            if total >= 0 && out.len() as i64 >= total {
                break;
            }
        }
    }
    Ok(out)
}

/// 清空回收站：POST 无 body；有 `taskId` 时轮询到完成。
///
/// 轮询失败（非 Auth）只记调试日志：清空请求已经被服务端接受，不把状态
/// 查询的网络抖动误报成“清空失败”；`status>=3` 才算任务失败。
pub(crate) async fn clear(drive: &GuangyaDrive) -> Result<OpReport> {
    let value = drive
        .api_post_no_body("/userres/v1/file/clear_recycle_bin", &[])
        .await?;
    let task_id = first_str(data_or_self(&value), &["taskId", "task_id"]).unwrap_or_default();
    if !task_id.is_empty() {
        wait_clear_task(drive, &task_id).await?;
    }
    let mut report = OpReport::default();
    report.success();
    Ok(report)
}

async fn wait_clear_task(drive: &GuangyaDrive, task_id: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        match drive
            .api_post(
                "/userres/v1/get_task_status",
                json!({ "taskId": task_id }),
                &[],
            )
            .await
        {
            Ok(value) => {
                let status = data_or_self(&value).get("status").and_then(as_i64);
                match status {
                    Some(2) => return Ok(()),
                    Some(s) if s >= 3 => {
                        return Err(Error::server(
                            s,
                            format!("清空回收站任务失败（status={s}）"),
                        ))
                    }
                    _ => {}
                }
            }
            // 鉴权错误必须上报；其它错误继续等，超时后按“已受理”返回成功。
            Err(e) if e.is_auth() => return Err(e),
            Err(e) => {
                wp_core::log_debug!("光鸭", "清空回收站状态查询失败（忽略）：{e}");
            }
        }
        if started.elapsed() >= CLEAR_TIMEOUT {
            wp_core::log_debug!("光鸭", "清空回收站任务 {task_id} 轮询超时，按已受理处理");
            return Ok(());
        }
        tokio::time::sleep(CLEAR_POLL_INTERVAL).await;
    }
}

/// 回收站列表请求体（`page>0` 才带 page，与源码一致）。
pub(crate) fn list_body(page: u32, page_size: u32) -> Value {
    let mut body = json!({
        "parentId": "",
        "pageSize": page_size,
        "dirType": DIR_TYPE_RECYCLE,
        "orderBy": ORDER_BY_DELETE_TIME,
        "sortType": SORT_TYPE_DESC,
        "clientId": CLIENT_ID,
    });
    if page > 0 {
        body["page"] = json!(page);
    }
    body
}

/// 从 `data.list` 映射回收站条目。
pub(crate) fn entries_from_data(data: &Value) -> Vec<RecycleEntry> {
    data.get("list")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(entry_from_value).collect())
        .unwrap_or_default()
}

/// 单条回收站记录 → 统一模型。
///
/// 时间字段源码 UI 依次取 `dtime / deleteTime / utime`，这里再加 `mtime/ctime`
/// 兜底；单位秒/毫秒都交给 [`first_ts`] 归一化。
pub(crate) fn entry_from_value(v: &Value) -> Option<RecycleEntry> {
    let name = first_str(v, &["fileName", "name"])?;
    let id = first_str(v, &["fileId", "id", "file_id", "recordId"]).unwrap_or_default();
    let res_type = as_i64(v.get("resType").unwrap_or(&Value::Null));
    let file_type = as_i64(v.get("fileType").unwrap_or(&Value::Null));
    let is_dir = res_type == Some(2)
        || file_type == Some(2)
        || v.get("isDir").and_then(Value::as_bool) == Some(true);
    let size = if is_dir {
        0
    } else {
        first_u64(v, &["fileSize", "size"]).unwrap_or(0)
    };
    let deleted_at = first_ts(v, &["dtime", "deleteTime", "utime", "mtime", "ctime"]);
    // 光鸭只给 fullParentIds（纯 ID 列表），没有目录名，无法还原逻辑路径；
    // 若服务端以后补 originalPath/sourcePath 则直接使用。
    let original_path = first_str(v, &["originalPath", "sourcePath"]).unwrap_or_default();
    Some(RecycleEntry {
        id,
        name,
        path: original_path.clone(),
        is_dir,
        size,
        deleted_at,
        original_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recycle_entry_mapping_file_and_dir() {
        let file = json!({
            "fileId":"f1","fileName":"a.mkv","resType":1,"fileSize":"12345",
            "utime":1700000000123i64,"fullParentIds":["1","2"]
        });
        let e = entry_from_value(&file).unwrap();
        assert_eq!(e.id, "f1");
        assert_eq!(e.name, "a.mkv");
        assert!(!e.is_dir);
        assert_eq!(e.size, 12345, "字符串大小要能解析");
        assert_eq!(e.deleted_at, Some(1_700_000_000), "毫秒要转秒");
        assert!(
            e.original_path.is_empty(),
            "fullParentIds 无法还原路径时留空"
        );

        let dir = json!({
            "fileId": 99, "fileName":"电影","resType":2,"fileSize":888,
            "dtime":1700000100,"originalPath":"/电影"
        });
        let e = entry_from_value(&dir).unwrap();
        assert_eq!(e.id, "99", "数字 id 转字符串");
        assert!(e.is_dir);
        assert_eq!(e.size, 0, "目录大小映射为 0");
        assert_eq!(e.deleted_at, Some(1_700_000_100));
        assert_eq!(e.original_path, "/电影");
        assert_eq!(e.path, "/电影");

        // 部分接口用 fileType 而不是 resType 表示目录。
        let by_file_type = json!({"fileId":"d2","fileName":"x","fileType":2});
        assert!(entry_from_value(&by_file_type).unwrap().is_dir);

        assert!(entry_from_value(&json!({"fileId":"x"})).is_none());
        assert!(entry_from_value(&json!({"fileName":"  "})).is_none());
        assert!(entries_from_data(&json!({"list":null})).is_empty());
    }

    #[test]
    fn recycle_list_body_shape() {
        let b0 = list_body(0, 100);
        assert_eq!(b0["dirType"], 4);
        assert_eq!(b0["orderBy"], 12);
        assert_eq!(b0["sortType"], 1);
        assert_eq!(b0["pageSize"], 100);
        assert_eq!(b0["parentId"], "");
        assert_eq!(b0["clientId"], crate::CLIENT_ID);
        assert!(b0.get("page").is_none(), "第一页不带 page");

        let b2 = list_body(2, 50);
        assert_eq!(b2["page"], 2);
        assert_eq!(b2["pageSize"], 50);
    }
}
