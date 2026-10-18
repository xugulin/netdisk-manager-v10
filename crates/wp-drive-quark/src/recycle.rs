//! 回收站：列表（分页）/ 恢复 / 彻底删除 / 清空。
//!
//! 端点与字段（见 `docs/research/夸克_协议.md` §4.6）：
//! * `GET  file/recycle/list`    参数 `_page/_size`，条目主键是 `record_id`；
//! * `POST file/recycle/recover` body `{"select_mode":2,"record_list":[...]}`；
//! * `POST file/recycle/remove`  同上（不可恢复）。
//!
//! 夸克没有「清空回收站」单接口：先分页取全部 `record_id`，再调用 `remove`。
//! 分页有硬上限（防止 `total` 异常时死循环）；超过上限时把「未清理完」如实写进
//! [`OpReport::errors`]，不假装成功。批量接口失败时会降级为逐条重试，把失败
//! 精确归因到具体 `record_id`。

use crate::{as_bool, as_i64, as_str, as_u64, data_of, pair, parse_timestamp_secs, QuarkDrive};
use serde_json::{json, Value};
use std::collections::HashSet;
use wp_core::error::ErrorKind;
use wp_core::model::{OpReport, RecycleEntry};
use wp_core::Result;

/// 每页条数。Python 参考实现默认 50，这里保持一致，避开未知服务端上限。
pub(crate) const RECYCLE_PAGE_SIZE: usize = 50;
/// 分页硬上限：200 页 × 50 = 10000 条。
pub(crate) const RECYCLE_MAX_PAGES: usize = 200;
/// 单次 `recover/remove` 的 `record_list` 批量上限。
pub(crate) const RECYCLE_BATCH_SIZE: usize = 100;

// ───────────────────────── 纯函数 ─────────────────────────

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

/// 回收站原始条目 → 统一模型。
///
/// * `id` 固定取 `record_id`（缺失时兼容 `fid/file_id`，否则该条无法恢复/删除）；
/// * 目录判断兼容 `dir=true` / `file_type==0`（夸克用 0 表示目录）；
/// * `deleted_at` 兼容秒/毫秒，并兼容 `deleted_time/delete_at/updated_at`；
/// * `original_path` 有则填（兼容 `source_path/pdir_path/path`）。
pub(crate) fn map_recycle_entry(raw: &Value) -> Option<RecycleEntry> {
    let id = text_of(raw, &["record_id", "fid", "file_id"])?;
    let original_path =
        text_of(raw, &["original_path", "source_path", "pdir_path", "path"]).unwrap_or_default();
    let name = text_of(raw, &["file_name", "name", "title"]).unwrap_or_else(|| {
        if original_path.is_empty() {
            id.clone()
        } else {
            wp_core::util::base_name(&original_path)
        }
    });
    let is_dir = as_bool(raw.get("dir")).unwrap_or(false)
        || as_bool(raw.get("is_dir")).unwrap_or(false)
        || as_i64(raw.get("file_type")) == Some(0);
    let size = if is_dir {
        0
    } else {
        as_u64(raw.get("size"))
            .or_else(|| as_u64(raw.get("file_size")))
            .unwrap_or(0)
    };
    let deleted_at = parse_timestamp_secs(
        raw.get("deleted_at")
            .or_else(|| raw.get("deleted_time"))
            .or_else(|| raw.get("delete_at"))
            .or_else(|| raw.get("updated_at")),
    );
    Some(RecycleEntry {
        id,
        name: name.clone(),
        path: if original_path.is_empty() {
            name
        } else {
            original_path.clone()
        },
        is_dir,
        size,
        deleted_at,
        original_path,
    })
}

/// `recover` / `remove` 的请求体。必须带 `select_mode`，否则报 14001。
pub(crate) fn recycle_record_body(ids: &[String]) -> Value {
    json!({
        "select_mode": 2,
        "record_list": ids,
    })
}

/// 清洗 record_id：去空白、去重；空 ID 直接计入失败（不能静默丢掉）。
pub(crate) fn normalize_record_ids(ids: &[String], report: &mut OpReport) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let id = id.trim();
        if id.is_empty() {
            report.fail("record_id 为空");
            continue;
        }
        if seen.insert(id.to_string()) {
            out.push(id.to_string());
        }
    }
    out
}

/// 是否继续翻页：空页停；`total` 已知且已取够停；到达硬上限停。
pub(crate) fn should_fetch_next_recycle_page(
    page: usize,
    page_len: usize,
    fetched: usize,
    total: u64,
) -> bool {
    page < RECYCLE_MAX_PAGES && page_len > 0 && (total == 0 || fetched < total as usize)
}

// ───────────────────────── 实现 ─────────────────────────

impl QuarkDrive {
    async fn fetch_recycle_page(&self, page: usize) -> Result<(Vec<Value>, u64)> {
        let params = vec![
            pair("_page", page.to_string()),
            pair("_size", RECYCLE_PAGE_SIZE.to_string()),
        ];
        let resp = self.api.get("file/recycle/list", params).await?;
        let data = data_of(&resp);
        let list = if data.is_array() {
            data.as_array().cloned().unwrap_or_default()
        } else {
            data.get("list")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let total = as_u64(
            resp.get("metadata")
                .and_then(|m| m.get("_total"))
                .or_else(|| data.get("total")),
        )
        .unwrap_or(0);
        Ok((list, total))
    }

    /// 分页拉全量。返回 `(条目, 是否因分页上限/总数不符而不完整)`。
    async fn collect_recycle_entries(&self) -> Result<(Vec<RecycleEntry>, bool)> {
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        let mut fetched = 0usize;
        let mut total = 0u64;
        let mut page = 1usize;
        let mut cap_hit = false;

        loop {
            if page > RECYCLE_MAX_PAGES {
                cap_hit = true;
                break;
            }
            let (list, page_total) = self.fetch_recycle_page(page).await?;
            if page == 1 {
                total = page_total;
            }
            let page_len = list.len();
            if page_len == 0 {
                break;
            }
            fetched += page_len;
            for raw in &list {
                if let Some(entry) = map_recycle_entry(raw) {
                    if seen.insert(entry.id.clone()) {
                        entries.push(entry);
                    }
                }
            }
            if !should_fetch_next_recycle_page(page, page_len, fetched, total) {
                if page >= RECYCLE_MAX_PAGES && total == 0 {
                    cap_hit = true;
                }
                break;
            }
            page += 1;
        }

        // 服务端声称还有，但翻页已结束（空页/上限）→ 列表不完整。
        let truncated = cap_hit || (total > 0 && fetched < total as usize);
        if truncated {
            wp_core::log_warn!(
                "夸克",
                "回收站分页未取全（已取 {} 条，total={}，上限 {} 页）",
                fetched,
                total,
                RECYCLE_MAX_PAGES
            );
        }
        Ok((entries, truncated))
    }

    pub(crate) async fn recycle_list_impl(&self) -> Result<Vec<RecycleEntry>> {
        self.api.ensure_valid()?;
        let (entries, _truncated) = self.collect_recycle_entries().await?;
        Ok(entries)
    }

    /// 批量执行回收站操作；批量失败且错误可归因时降级逐条，保证失败汇总到具体 ID。
    async fn apply_recycle_op(
        &self,
        endpoint: &str,
        ids: &[String],
        action: &str,
    ) -> Result<OpReport> {
        let mut report = OpReport::default();
        let ids = normalize_record_ids(ids, &mut report);
        for chunk in ids.chunks(RECYCLE_BATCH_SIZE) {
            match self.api.post(endpoint, recycle_record_body(chunk)).await {
                Ok(_) => report.ok += chunk.len(),
                Err(e) => {
                    let can_isolate = chunk.len() > 1
                        && !e.is_auth()
                        && !matches!(e.kind(), ErrorKind::Network | ErrorKind::Io);
                    if can_isolate {
                        for id in chunk {
                            match self
                                .api
                                .post(endpoint, recycle_record_body(std::slice::from_ref(id)))
                                .await
                            {
                                Ok(_) => report.success(),
                                Err(e) => {
                                    report.fail(format!("{action}记录 {id} 失败：{e}"));
                                }
                            }
                        }
                    } else {
                        report.failed += chunk.len();
                        report.errors.push(format!(
                            "{action} {} 项失败（record_id 批量提交）：{e}",
                            chunk.len()
                        ));
                    }
                }
            }
        }
        Ok(report)
    }

    pub(crate) async fn recycle_restore_impl(&self, ids: &[String]) -> Result<OpReport> {
        self.api.ensure_valid()?;
        let report = self
            .apply_recycle_op("file/recycle/recover", ids, "恢复")
            .await?;
        if report.ok > 0 {
            // 恢复会把条目放回原目录，路径缓存必须失效。
            self.invalidate_cache();
        }
        Ok(report)
    }

    pub(crate) async fn recycle_delete_impl(&self, ids: &[String]) -> Result<OpReport> {
        self.api.ensure_valid()?;
        self.apply_recycle_op("file/recycle/remove", ids, "彻底删除")
            .await
    }

    pub(crate) async fn recycle_clear_impl(&self) -> Result<OpReport> {
        self.api.ensure_valid()?;
        let (entries, truncated) = self.collect_recycle_entries().await?;
        if entries.is_empty() {
            let mut report = OpReport::default();
            if truncated {
                report.fail("回收站分页达到上限，未能读取到可清理条目");
            }
            return Ok(report);
        }
        let ids: Vec<String> = entries.into_iter().map(|e| e.id).collect();
        let mut report = self
            .apply_recycle_op("file/recycle/remove", &ids, "彻底删除")
            .await?;
        if truncated {
            report.fail(format!(
                "回收站超过分页上限（{} 页 × {} 条），仅清理了已读取的条目；请再次执行清空",
                RECYCLE_MAX_PAGES, RECYCLE_PAGE_SIZE
            ));
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_recycle_file_entry() {
        let raw = json!({
            "record_id": "rec-1",
            "file_name": "影片.mp4",
            "file_type": 1,
            "size": 12345,
            "deleted_at": 1789322627000i64,
            "original_path": "/影视/影片.mp4"
        });
        let e = map_recycle_entry(&raw).unwrap();
        assert_eq!(e.id, "rec-1");
        assert_eq!(e.name, "影片.mp4");
        assert!(!e.is_dir);
        assert_eq!(e.size, 12345);
        assert_eq!(e.deleted_at, Some(1789322627));
        assert_eq!(e.original_path, "/影视/影片.mp4");
        assert_eq!(e.path, "/影视/影片.mp4");
    }

    #[test]
    fn map_recycle_dir_aliases_and_record_id_priority() {
        let raw = json!({
            "record_id": "rec-dir",
            "fid": "fid-should-not-win",
            "file_name": "相册",
            "dir": true,
            "size": 999,
            "deleted_time": "1789322627",
            "pdir_path": "/图片"
        });
        let e = map_recycle_entry(&raw).unwrap();
        assert_eq!(e.id, "rec-dir", "id 必须优先取 record_id，而不是 fid");
        assert!(e.is_dir);
        assert_eq!(e.size, 0, "目录大小归零");
        assert_eq!(e.deleted_at, Some(1789322627));
        assert_eq!(e.original_path, "/图片");

        let file = json!({
            "fid": "fid-1",
            "file_name": "a.txt",
            "file_type": 1,
            "size": "7"
        });
        let e = map_recycle_entry(&file).unwrap();
        assert_eq!(e.id, "fid-1");
        assert_eq!(e.size, 7);
        assert!(e.deleted_at.is_none());

        // 没有 record_id/fid/file_id：无法恢复/删除，直接丢弃。
        assert!(map_recycle_entry(&json!({"file_name": "无主"})).is_none());
    }

    #[test]
    fn map_recycle_name_falls_back_to_original_path() {
        let raw = json!({
            "record_id": "r",
            "original_path": "/目录/子目录/说明.txt"
        });
        let e = map_recycle_entry(&raw).unwrap();
        assert_eq!(e.name, "说明.txt");
        assert_eq!(e.path, "/目录/子目录/说明.txt");
    }

    #[test]
    fn recycle_record_body_contract() {
        let ids = vec!["rec-1".to_string(), "rec-2".to_string()];
        let body = recycle_record_body(&ids);
        assert_eq!(body["select_mode"], 2);
        assert_eq!(body["record_list"].as_array().unwrap().len(), 2);
        // 回收站接口只认 record_list，不能混入 file/delete 的字段。
        assert!(body.get("current_dir_fid").is_none());
        assert!(body.get("filelist").is_none());
    }

    #[test]
    fn normalize_record_ids_marks_blank_and_dedupes() {
        let mut report = OpReport::default();
        let ids = vec![
            " rec-1 ".to_string(),
            "rec-1".to_string(),
            "  ".to_string(),
            "rec-2".to_string(),
        ];
        let out = normalize_record_ids(&ids, &mut report);
        assert_eq!(out, vec!["rec-1".to_string(), "rec-2".to_string()]);
        assert_eq!(report.ok, 0);
        assert_eq!(report.failed, 1, "空 record_id 必须计入失败而不是静默丢弃");
        assert!(report.errors[0].contains("record_id"));
    }

    #[test]
    fn recycle_pagination_decision() {
        assert!(should_fetch_next_recycle_page(1, 50, 50, 100));
        assert!(!should_fetch_next_recycle_page(2, 50, 100, 100));
        assert!(!should_fetch_next_recycle_page(1, 0, 0, 0));
        assert!(!should_fetch_next_recycle_page(
            RECYCLE_MAX_PAGES,
            RECYCLE_PAGE_SIZE,
            RECYCLE_MAX_PAGES * RECYCLE_PAGE_SIZE,
            0
        ));
        // total 未知时，满页继续、空页停止。
        assert!(should_fetch_next_recycle_page(1, 50, 50, 0));
    }
}
