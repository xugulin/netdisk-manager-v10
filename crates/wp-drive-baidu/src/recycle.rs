//! 回收站：列表 / 还原。
//!
//! 协议依据（`核心/接口/回收站接口.py` + HAR 实测样本）：
//! * 列表：`GET /api/recycle/list/`（**注意末尾斜杠**），
//!   query `num=100&page=1`，`带渠道=False`；响应 `{errno, list, timestamp}`。
//! * 还原：`POST /api/recycle/restore`，query `channel=chunlei&async=1`，
//!   form `fidlist=[<fs_id>,...]`（JSON 数组），需要 `bdstoken`。
//!   响应失败项在 **`faillist`**（不是 `info`），`taskid:0` 表示同步完成。
//! * 彻底删除：`POST /api/recycle/delete` 首次必回 `errno:132` 并强制短信验证，
//!   Python 源码刻意不提供自动化方法；本适配器同样不实现（caps=false）。
//!
//! 条目的 `fs_id` 是数字，但统一接口用字符串传 id；调用还原时才解析为 i64，
//! 解析失败给明确错误，绝不静默跳过。

use crate::http::ReqOpts;
use crate::{err_text, BaiduDrive};
use serde_json::Value;
use std::collections::HashSet;
use wp_core::model::{OpReport, RecycleEntry};
use wp_core::util::base_name;
use wp_core::{Error, Result};

/// 回收站单页条数（HAR/Python 均为 100）。
const PAGE_SIZE: usize = 100;
/// 最多翻页数（Python `取全部` 的上限是 50 页）。
const MAX_PAGES: usize = 50;

/// 把数字/字符串 id 统一成字符串。
fn id_text(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.trim().to_string();
    }
    if let Some(n) = v.as_i64() {
        return n.to_string();
    }
    if let Some(n) = v.as_u64() {
        return n.to_string();
    }
    String::new()
}

/// 把回收站条目（HAR 实测字段）映射成统一模型。
///
/// 实测字段：`fs_id / isdir / path / server_filename / size /
/// server_mtime / local_mtime / leftTime`。
/// `server_mtime` 是“进入回收站”的时间（原文件修改时间不再单独给出），
/// 因此作为 `deleted_at`；缺失时回退 `local_mtime`。
pub(crate) fn entry_from_recycle(v: &Value) -> Option<RecycleEntry> {
    let id = id_text(
        v.get("fs_id")
            .or_else(|| v.get("fid"))
            .or_else(|| v.get("id"))?,
    );
    if id.is_empty() {
        // 没有 fs_id 就无法还原/勾选，直接跳过比给一个空 id 更安全。
        return None;
    }
    let mut path = ["path", "original_path", "source_path"]
        .iter()
        .filter_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();
    let mut name = ["server_filename", "filename", "name"]
        .iter()
        .filter_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();
    if name.is_empty() && !path.is_empty() {
        name = base_name(&path);
    }
    if path.is_empty() && !name.is_empty() {
        path = format!("/{name}");
    }
    if name.is_empty() {
        return None;
    }
    let is_dir = v.get("isdir").and_then(Value::as_i64).unwrap_or(0) == 1;
    let size = v
        .get("size")
        .and_then(Value::as_u64)
        .or_else(|| {
            v.get("size")
                .and_then(Value::as_i64)
                .map(|n| n.max(0) as u64)
        })
        .unwrap_or(0);
    let deleted_at = ["server_mtime", "local_mtime", "delete_time", "dtime"]
        .iter()
        .filter_map(|k| v.get(*k).and_then(Value::as_i64))
        .find(|t| *t > 0);
    Some(RecycleEntry {
        id,
        name,
        // 回收站条目的 path 就是删除前的位置；统一模型两个字段都填，
        // 保证 GUI 的「原路径」列有值可显示。
        path: path.clone(),
        original_path: path,
        is_dir,
        size,
        deleted_at,
    })
}

/// 回收站列表（自动翻页，最多 50 页）。
pub(crate) async fn list(d: &BaiduDrive) -> Result<Vec<RecycleEntry>> {
    let mut out = Vec::new();
    for page in 1..=MAX_PAGES {
        let params = vec![
            ("num".to_string(), PAGE_SIZE.to_string()),
            ("page".to_string(), page.to_string()),
        ];
        let v = d
            .api_get(
                "/api/recycle/list/",
                &params,
                ReqOpts::authed().no_channel(),
            )
            .await?;
        let items = match v.get("list").and_then(Value::as_array) {
            Some(items) => items,
            // Python 用 `数据.get("list") or []`，这里同样容忍空响应。
            None => break,
        };
        let count = items.len();
        for item in items {
            if let Some(entry) = entry_from_recycle(item) {
                out.push(entry);
            }
        }
        if count < PAGE_SIZE {
            break;
        }
    }
    wp_core::log_debug!("百度", "回收站列表 → {} 项", out.len());
    Ok(out)
}

/// 解析 `recycle_list` 给回的字符串 id 列表。
///
/// 百度还原接口要的是数字 fid，这里做一次显式转换；空列表 / 非数字都给
/// 可读错误（GUI 会直接展示），不去猜测调用方意图。
pub(crate) fn parse_fids(ids: &[String]) -> Result<Vec<i64>> {
    if ids.is_empty() {
        return Err(Error::other("回收站 fid 列表不能为空"));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(ids.len());
    for raw in ids {
        let text = raw.trim();
        if text.is_empty() {
            return Err(Error::other("回收站 fid 不能为空字符串"));
        }
        let n = text.parse::<i64>().map_err(|_| {
            Error::other(format!(
                "回收站 fid 必须是数字：{raw:?}（应为 recycle_list 返回的 fs_id）"
            ))
        })?;
        if n <= 0 {
            return Err(Error::other(format!("回收站 fid 必须是正数：{raw:?}")));
        }
        if seen.insert(n) {
            out.push(n);
        }
    }
    Ok(out)
}

/// 单个失败项的展示文本。
fn failure_text(v: &Value) -> String {
    match v {
        Value::String(s) => format!("还原失败：fid={s}"),
        Value::Number(n) => format!("还原失败：fid={n}"),
        Value::Object(_) => {
            let id = v
                .get("fs_id")
                .or_else(|| v.get("fid"))
                .or_else(|| v.get("id"))
                .map(id_text)
                .unwrap_or_default();
            let why = err_text(v, "");
            if id.is_empty() {
                format!("还原失败：{why}")
            } else if why.is_empty() {
                format!("还原失败：fid={id}")
            } else {
                format!("还原失败：fid={id} {why}")
            }
        }
        other => format!("还原失败：{other}"),
    }
}

/// `restore` 响应 → `OpReport`：成功数 = 提交数 - `faillist` 数。
pub(crate) fn report_from_response(total: usize, v: &Value) -> OpReport {
    let failures: Vec<Value> = v
        .get("faillist")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let failed = failures.len().min(total);
    let mut report = OpReport {
        ok: total - failed,
        failed,
        errors: Vec::new(),
    };
    for item in failures.iter().take(failed) {
        report.errors.push(failure_text(item));
    }
    report
}

/// 从回收站还原（`taskid:0` 为同步完成，与 Python 一致，不轮询）。
pub(crate) async fn restore(d: &BaiduDrive, ids: &[String]) -> Result<OpReport> {
    let fids = parse_fids(ids)?;
    let body = serde_json::to_string(&fids)
        .map_err(|e| Error::other(format!("fidlist 序列化失败：{e}")))?;
    let params = vec![("async".to_string(), "1".to_string())];
    let form = vec![("fidlist".to_string(), body)];
    let v = d
        .api_post_form(
            "/api/recycle/restore",
            &params,
            &form,
            ReqOpts::authed().with_bdstoken(),
        )
        .await?;
    let report = report_from_response(fids.len(), &v);
    let taskid = v.get("taskid").and_then(Value::as_i64).unwrap_or(0);
    wp_core::log_info!(
        "百度",
        "回收站还原 {} 项 → 成功 {} / 失败 {}（taskid={taskid}）",
        fids.len(),
        report.ok,
        report.failed
    );
    for why in &report.errors {
        wp_core::log_warn!("百度", "{why}");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recycle_entry_mapping_uses_fs_id_and_server_mtime() {
        // 字段形状取自 HAR `/api/recycle/list/` 实测响应。
        let v = json!({
            "category": 6,
            "fs_id": 270178286089923i64,
            "isdir": 0,
            "path": "/test_10mb.bin",
            "server_filename": "test_10mb.bin",
            "size": 10485760,
            "local_mtime": 1789317725,
            "server_mtime": 1789325603,
            "leftTime": 75
        });
        let e = entry_from_recycle(&v).unwrap();
        assert_eq!(e.id, "270178286089923");
        assert_eq!(e.name, "test_10mb.bin");
        assert_eq!(e.path, "/test_10mb.bin");
        assert_eq!(e.original_path, "/test_10mb.bin");
        assert!(!e.is_dir);
        assert_eq!(e.size, 10 * 1024 * 1024);
        assert_eq!(e.deleted_at, Some(1789325603));
    }

    #[test]
    fn recycle_entry_mapping_handles_dir_and_missing_fields() {
        let v = json!({
            "fs_id": "828379621778212",
            "isdir": 1,
            "path": "/测试",
            "server_filename": "测试"
        });
        let e = entry_from_recycle(&v).unwrap();
        assert!(e.is_dir);
        assert_eq!(e.id, "828379621778212");
        assert_eq!(e.deleted_at, None);
        // 缺 fs_id 的条目无法还原，直接丢弃。
        assert!(entry_from_recycle(&json!({"server_filename": "x"})).is_none());
        // 只有 path 时从 path 推文件名。
        let e = entry_from_recycle(&json!({"fs_id": 1, "path": "/a/b.txt"})).unwrap();
        assert_eq!(e.name, "b.txt");
    }

    #[test]
    fn recycle_fid_parsing_is_explicit() {
        let ids = vec![" 123 ".to_string(), "456".to_string(), "123".to_string()];
        assert_eq!(parse_fids(&ids).unwrap(), vec![123, 456]);
        assert!(parse_fids(&[]).is_err());
        assert!(parse_fids(&["abc".to_string()]).is_err());
        assert!(parse_fids(&["".to_string()]).is_err());
        assert!(parse_fids(&["-1".to_string()]).is_err());
        assert!(parse_fids(&["1.5".to_string()]).is_err());
    }

    #[test]
    fn recycle_restore_report_uses_faillist() {
        let v = json!({"errno": 0, "faillist": [584299975796218i64], "taskid": 0});
        let r = report_from_response(3, &v);
        assert_eq!(r.ok, 2);
        assert_eq!(r.failed, 1);
        assert!(r.errors[0].contains("584299975796218"));

        let v = json!({"errno": 0, "faillist": []});
        let r = report_from_response(2, &v);
        assert_eq!((r.ok, r.failed), (2, 0));
        assert!(r.is_ok());

        // 服务端给的失败项是对象时，尽量带出错误消息。
        let v = json!({"faillist": [{"fs_id": 9, "errmsg": "不存在"}]});
        let r = report_from_response(1, &v);
        assert_eq!(r.failed, 1);
        assert!(r.errors[0].contains("不存在"));
    }
}
