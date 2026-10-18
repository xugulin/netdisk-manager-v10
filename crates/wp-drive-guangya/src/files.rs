//! 文件接口：分页列目录、路径解析（逐级列目录模拟）、建目录、删除。
//!
//! 光鸭没有“路径/树”接口，路径解析只能从根开始逐段 `get_file_list` 找目录
//! （协议 §4.8）；本模块把这层模拟收拢在 `resolve_*` 里，上传/分享/下载复用。
//!
//! 业务码 159 = 目录已存在，创建目录时传入白名单并复用返回的 `fileId`。

use serde_json::{json, Value};
use wp_core::model::{OpReport, RemoteEntry};
use wp_core::util::{join_path, norm_path, parent_path};
use wp_core::{Error, Result};

use crate::{as_i64, data_or_self, GuangyaDrive, CLIENT_ID};

/// 普通列表每页大小（服务端可能更小，循环以 `hasMore`/短页为终止条件）。
const PAGE_SIZE: u32 = 50;
/// 防御性上限，避免服务端异常导致死循环。
const MAX_PAGES: u32 = 200;

/// 列目录（自动翻页）。
pub(crate) async fn list(drive: &GuangyaDrive, path: &str) -> Result<Vec<RemoteEntry>> {
    let dir = resolve_dir(drive, path).await?;
    list_all(drive, &dir.id, &dir.path).await
}

/// 取单个条目信息。
pub(crate) async fn stat(drive: &GuangyaDrive, path: &str) -> Result<RemoteEntry> {
    resolve_entry(drive, path).await
}

/// 递归创建目录（已存在则复用，159 兼容）。
pub(crate) async fn ensure_dir(drive: &GuangyaDrive, path: &str) -> Result<RemoteEntry> {
    let path = norm_path(path);
    if path == "/" {
        return Ok(root_entry());
    }
    let mut parent_id = String::new();
    let mut current = String::from("/");
    let mut last = root_entry();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        let children = list_all(drive, &parent_id, &current).await?;
        if let Some(found) = children.into_iter().find(|e| e.name == seg) {
            if !found.is_dir {
                return Err(Error::other(format!(
                    "路径 {} 已存在同名文件，无法创建目录",
                    join_path(&current, seg)
                )));
            }
            parent_id = found.id.clone();
            current = found.path.clone();
            last = found;
            continue;
        }
        let created = create_dir(drive, &parent_id, &current, seg).await?;
        parent_id = created.id.clone();
        current = created.path.clone();
        last = created;
    }
    Ok(last)
}

/// 批量删除（移入回收站）；逐项解析 ID，部分失败写入 OpReport。
pub(crate) async fn delete(drive: &GuangyaDrive, paths: &[String]) -> Result<OpReport> {
    let mut report = OpReport::default();
    let mut ids: Vec<String> = Vec::new();
    for path in paths {
        match resolve_entry(drive, path).await {
            Ok(e) => {
                if e.id.is_empty() && e.path == "/" {
                    report.fail("不能删除根目录");
                } else {
                    ids.push(e.id);
                }
            }
            Err(e) => report.fail(format!("{path}：{e}")),
        }
    }
    if ids.is_empty() {
        return Ok(report);
    }
    let body = json!({ "fileIds": ids, "clientId": CLIENT_ID });
    drive
        .api_post("/userres/v1/file/delete_file", body, &[])
        .await?;
    for _ in 0..ids.len() {
        report.success();
    }
    Ok(report)
}

/// 列某个 `parentId` 下所有页；`parent_path` 用于拼统一逻辑路径。
pub(crate) async fn list_all(
    drive: &GuangyaDrive,
    parent_id: &str,
    parent_path: &str,
) -> Result<Vec<RemoteEntry>> {
    let mut out = Vec::new();
    let mut page = 0u32;
    loop {
        let body = json!({
            "pageSize": PAGE_SIZE,
            "orderBy": 3,
            "sortType": 1,
            "parentId": parent_id,
            "page": page,
            "clientId": CLIENT_ID,
        });
        let value = drive
            .api_post("/userres/v1/file/get_file_list", body, &[])
            .await?;
        let data = data_or_self(&value);
        let page_entries = entries_from_data(data, parent_path);
        let count = page_entries.len();
        out.extend(page_entries);
        let has_more = data.get("hasMore").and_then(Value::as_bool);
        if count == 0 || has_more == Some(false) {
            break;
        }
        // 没有 hasMore 时按“短页即最后一页”判断；有 hasMore=true 则必须继续。
        if has_more.is_none() && count < PAGE_SIZE as usize {
            break;
        }
        page += 1;
        if page > MAX_PAGES {
            return Err(Error::server(0, "文件列表分页超过上限，已中止"));
        }
    }
    Ok(out)
}

/// 从 `data.list` 解析条目（纯逻辑，便于单测）。
pub(crate) fn entries_from_data(data: &Value, parent_path: &str) -> Vec<RemoteEntry> {
    data.get("list")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|v| entry_from_value(v, parent_path))
                .collect()
        })
        .unwrap_or_default()
}

/// 把一个文件列表条目映射成统一模型。
pub(crate) fn entry_from_value(v: &Value, parent_path: &str) -> Option<RemoteEntry> {
    let name = first_str(v, &["fileName", "name"])?;
    if name.trim().is_empty() {
        return None;
    }
    let id = first_str(v, &["fileId", "id", "file_id"]).unwrap_or_default();
    let res_type = as_i64(v.get("resType").unwrap_or(&Value::Null)).unwrap_or(0);
    let is_dir = res_type == 2;
    let size = if is_dir {
        0
    } else {
        first_u64(v, &["fileSize", "size", "fileSizeBytes"]).unwrap_or(0)
    };
    let modified = first_ts(v, &["updateTime", "utime", "modifyTime", "mtime", "ctime"]);
    let created = first_ts(v, &["ctime", "createTime", "createdAt"]);
    Some(RemoteEntry {
        id,
        name: name.clone(),
        path: join_path(parent_path, &name),
        is_dir,
        size,
        modified,
        created,
        thumbnail: thumbnail_from_value(v),
        raw: Some(v.clone()),
    })
}

/// 取条目里的缩略图 URL（HAR 确认字段名 `thumbnail`，见 Python
/// `文件接口.py:287-294`）。
///
/// 光鸭返回的 URL 里常出现**字面量** `\u0026`（反斜杠 + `u0026` 文本，不是 JSON
/// 转义），必须还原成 `&`，否则查询参数会整段粘在第一个 `\u0026` 后面。
/// 空串/纯空白视为没有缩略图（与 Python 源的真值判断一致）。
pub(crate) fn thumbnail_from_value(v: &Value) -> Option<String> {
    let raw = v.get("thumbnail").and_then(Value::as_str)?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(raw.replace("\\u0026", "&"))
}

/// 解析目录：返回带 `id` 的目录条目；根目录 id 为空串。
pub(crate) async fn resolve_dir(drive: &GuangyaDrive, path: &str) -> Result<RemoteEntry> {
    let path = norm_path(path);
    if path == "/" {
        return Ok(root_entry());
    }
    let mut parent_id = String::new();
    let mut current = root_entry();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        let children = list_all(drive, &parent_id, &current.path).await?;
        let found = children
            .into_iter()
            .find(|e| e.name == seg && e.is_dir)
            .ok_or_else(|| Error::not_found(format!("目录不存在：{seg}")))?;
        parent_id = found.id.clone();
        current = found;
    }
    Ok(current)
}

/// 解析文件或目录。
pub(crate) async fn resolve_entry(drive: &GuangyaDrive, path: &str) -> Result<RemoteEntry> {
    let path = norm_path(path);
    if path == "/" {
        return Ok(root_entry());
    }
    let parent = parent_path(&path);
    let name = wp_core::util::base_name(&path);
    let dir = resolve_dir(drive, &parent).await?;
    let children = list_all(drive, &dir.id, &dir.path).await?;
    children
        .into_iter()
        .find(|e| e.name == name)
        .ok_or_else(|| Error::not_found(format!("文件不存在：{path}")))
}

/// 创建目录（159 = 已存在也放行，并复用其 fileId）。
pub(crate) async fn create_dir(
    drive: &GuangyaDrive,
    parent_id: &str,
    parent_path: &str,
    name: &str,
) -> Result<RemoteEntry> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::other("目录名不能为空"));
    }
    let body = json!({
        "parentId": parent_id,
        "dirName": name,
        "failIfNameExist": true,
    });
    // 159：目录已存在（云添加默认目录就依赖这个分支）。
    let value = drive
        .api_post("/userres/v1/file/create_dir", body, &[159])
        .await?;
    let data = data_or_self(&value);
    let id = first_str(data, &["fileId", "file_id"])
        .ok_or_else(|| Error::server(0, format!("创建目录响应缺少 fileId：{}", value)))?;
    let parent_path = norm_path(parent_path);
    let mut entry = entry_from_value(data, &parent_path)
        .unwrap_or_else(|| RemoteEntry::dir(name, join_path(&parent_path, name)));
    entry.id = id;
    entry.is_dir = true;
    entry.size = 0;
    if entry.name.trim().is_empty() {
        entry.name = name.to_string();
    }
    Ok(entry)
}

fn root_entry() -> RemoteEntry {
    RemoteEntry {
        id: String::new(),
        name: "/".into(),
        path: "/".into(),
        is_dir: true,
        ..Default::default()
    }
}

/// 取第一个非空字符串字段（数字也转成字符串）；供回收站/记录/云任务映射复用。
pub(crate) fn first_str(v: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        let Some(val) = v.get(*k) else { continue };
        let s = match val {
            Value::String(s) => s.trim().to_string(),
            Value::Number(n) => n.to_string(),
            _ => continue,
        };
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// 取第一个非负整数字段（数字/字符串都认）。
pub(crate) fn first_u64(v: &Value, keys: &[&str]) -> Option<u64> {
    for k in keys {
        let Some(val) = v.get(*k) else { continue };
        if let Some(n) = val.as_u64() {
            return Some(n);
        }
        if let Some(n) = val.as_i64() {
            if n >= 0 {
                return Some(n as u64);
            }
        }
        if let Some(s) = val.as_str() {
            if let Ok(n) = s.trim().parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

/// 服务端时间可能是秒、毫秒、浮点或字符串；统一成 Unix 秒。
pub(crate) fn first_ts(v: &Value, keys: &[&str]) -> Option<i64> {
    for k in keys {
        let Some(val) = v.get(*k) else { continue };
        let mut n = match val {
            Value::Number(n) => match n.as_f64() {
                Some(x) => x,
                None => continue,
            },
            Value::String(s) => match s.trim().parse::<f64>() {
                Ok(x) => x,
                Err(_) => continue,
            },
            _ => continue,
        };
        if !n.is_finite() || n <= 0.0 {
            continue;
        }
        if n > 1e12 {
            n /= 1000.0;
        }
        return Some(n as i64);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_mapping_file_and_dir() {
        let dir = json!({
            "fileId":"d1","fileName":"电影","resType":2,"fileSize":123,
            "ctime":1700000000,"utime":1700000100
        });
        let e = entry_from_value(&dir, "/").unwrap();
        assert!(e.is_dir);
        assert_eq!(e.id, "d1");
        assert_eq!(e.path, "/电影");
        assert_eq!(e.size, 0, "目录大小映射为 0");
        assert_eq!(e.modified, Some(1_700_000_100));
        assert_eq!(e.created, Some(1_700_000_000));

        let file = json!({
            "fileId":"f1","fileName":"a.mkv","resType":1,"fileSize":"12345",
            "utime":1700000000123i64
        });
        let e = entry_from_value(&file, "/电影").unwrap();
        assert!(!e.is_dir);
        assert_eq!(e.path, "/电影/a.mkv");
        assert_eq!(e.size, 12345, "字符串大小要能解析");
        assert_eq!(e.modified, Some(1_700_000_000), "毫秒要转秒");
        assert!(e.raw.is_some());
    }

    #[test]
    fn entry_mapping_thumbnail() {
        // HAR 里是字面量 \u0026（反斜杠 + u0026），要还原成 &。
        let v = json!({
            "fileId":"img1","fileName":"封面.jpg","resType":1,
            "thumbnail":"https://cdn.guangyapan.com/t/abc.jpg?x=1\\u0026y=2\\u0026z=3"
        });
        let e = entry_from_value(&v, "/相册").unwrap();
        assert_eq!(
            e.thumbnail.as_deref(),
            Some("https://cdn.guangyapan.com/t/abc.jpg?x=1&y=2&z=3")
        );

        // 没有字段 -> None
        let e = entry_from_value(&json!({"fileName":"a.txt","resType":1}), "/").unwrap();
        assert!(e.thumbnail.is_none());

        // 空串 / 纯空白 -> None
        for bad in ["", "   ", "\t\n"] {
            let e = entry_from_value(
                &json!({"fileName":"b.jpg","resType":1,"thumbnail":bad}),
                "/",
            )
            .unwrap();
            assert!(e.thumbnail.is_none(), "空缩略图 {bad:?} 应按无处理");
        }
    }

    #[test]
    fn entries_from_list_response() {
        let data = json!({
            "list": [
                {"fileId":"1","fileName":"a","resType":2},
                {"fileId":"2","fileName":"b.txt","resType":1,"fileSize":3},
                {"fileId":"3","resType":1},
                {"fileId":"4","fileName":"","resType":1}
            ]
        });
        let entries = entries_from_data(&data, "/");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "a");
        assert_eq!(entries[1].size, 3);
        assert_eq!(entries_from_data(&json!({"list": null}), "/").len(), 0);
    }

    #[test]
    fn timestamp_normalization() {
        assert_eq!(
            first_ts(&json!({"ctime": 1700000000}), &["ctime"]),
            Some(1_700_000_000)
        );
        assert_eq!(
            first_ts(&json!({"ctime": "1700000000"}), &["ctime"]),
            Some(1_700_000_000)
        );
        assert_eq!(
            first_ts(&json!({"ctime": 1700000000123i64}), &["ctime"]),
            Some(1_700_000_000)
        );
        assert_eq!(first_ts(&json!({"ctime": 0}), &["ctime"]), None);
        assert_eq!(first_ts(&json!({}), &["ctime"]), None);
    }

    #[test]
    fn root_entry_shape() {
        let r = root_entry();
        assert_eq!(r.path, "/");
        assert_eq!(r.id, "");
        assert!(r.is_dir);
    }
}
