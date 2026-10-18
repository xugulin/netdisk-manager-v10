//! 文件浏览与管理：list / stat / ensure_dir / delete / rename / move / copy / search。
//!
//! 文档 §4 的关键差异：
//! * `/api/list` 返回 `list`，`/api/filemetas`、`/api/categorylist` 返回 `info`；
//! * 判目录只看 `isdir == 1`（目录的 `category` 恒为 6，不能拿来判目录）；
//! * `/api/list` 没有 `total`，只能按“本页是否满”翻页；
//! * `filemanager` 的 `filelist` 结构随 opera 变化：
//!   delete 是纯字符串数组；rename 带 id；move/copy 不带 id。
//! * 删除/移动/复制返回 `taskid`，用 `/share/taskquery` 轮询。

use crate::http::ReqOpts;
use crate::{err_text, BaiduDrive};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use wp_core::model::{OpReport, RemoteEntry};
use wp_core::util::{base_name, join_path, norm_path, parent_path};
use wp_core::{Error, Result};

fn p(key: &str, value: impl Into<String>) -> (String, String) {
    (key.to_string(), value.into())
}

fn json_text(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())
}

/// 根目录条目。
pub(crate) fn root_entry() -> RemoteEntry {
    RemoteEntry {
        id: "/".into(),
        name: "/".into(),
        path: "/".into(),
        is_dir: true,
        size: 0,
        modified: None,
        created: None,
        raw: None,
    }
}

/// 是否含 `.` / `..` 路径段。
///
/// 百度对 `/测试/.` 不报错，会默默建出垃圾目录（文档 §9.2 第 18 条），
/// 因此必须在规范化**之前**拒绝。
pub(crate) fn has_dot_segment(path: &str) -> bool {
    path.split('/').any(|seg| seg == "." || seg == "..")
}

/// 把 `/api/list`、`/api/filemetas` 的文件对象映射成统一模型。
pub(crate) fn entry_from_value(v: &Value, fallback_dir: &str) -> Option<RemoteEntry> {
    let is_dir = v.get("isdir").and_then(Value::as_i64).unwrap_or(0) == 1;
    let mut name = v
        .get("server_filename")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let mut path = v
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if path.is_empty() && !name.is_empty() {
        path = join_path(fallback_dir, &name);
    }
    if name.is_empty() && !path.is_empty() {
        name = base_name(&path);
    }
    if name.is_empty() {
        return None;
    }
    let id = if let Some(n) = v.get("fs_id").and_then(Value::as_i64) {
        n.to_string()
    } else if let Some(s) = v.get("fs_id").and_then(Value::as_str) {
        s.to_string()
    } else {
        String::new()
    };
    let size = v
        .get("size")
        .and_then(Value::as_u64)
        .or_else(|| {
            v.get("size")
                .and_then(Value::as_i64)
                .map(|n| n.max(0) as u64)
        })
        .unwrap_or(0);
    let modified = v
        .get("server_mtime")
        .and_then(Value::as_i64)
        .or_else(|| v.get("mtime").and_then(Value::as_i64))
        .filter(|t| *t > 0);
    let created = v
        .get("server_ctime")
        .and_then(Value::as_i64)
        .or_else(|| v.get("ctime").and_then(Value::as_i64))
        .filter(|t| *t > 0);
    Some(RemoteEntry {
        id,
        name,
        path,
        is_dir,
        size,
        modified,
        created,
        raw: Some(v.clone()),
    })
}

/// 列目录（自动翻页，最多 200 页 / 20000 条，与 Python 版一致）。
pub(crate) async fn list(d: &BaiduDrive, path: &str) -> Result<Vec<RemoteEntry>> {
    let dir = norm_path(path);
    let mut out: Vec<RemoteEntry> = Vec::new();
    let mut page = 1u32;
    const NUM: u32 = 1000;
    loop {
        if page > 200 || out.len() >= 20_000 {
            wp_core::log_warn!("百度", "目录 {dir} 达到翻页上限，结果可能不完整");
            break;
        }
        let params = vec![
            p("dir", dir.clone()),
            p("order", "time"),
            p("desc", "1"),
            p("num", NUM.to_string()),
            p("page", page.to_string()),
        ];
        let v = d
            .api_get("/api/list", &params, ReqOpts::authed().no_channel())
            .await?;
        let Some(items) = v.get("list").and_then(Value::as_array) else {
            break;
        };
        if items.is_empty() {
            break;
        }
        for item in items {
            if let Some(entry) = entry_from_value(item, &dir) {
                out.push(entry);
            }
        }
        if items.len() < NUM as usize {
            break;
        }
        page += 1;
    }
    wp_core::log_debug!("百度", "列目录 {dir} → {} 项", out.len());
    Ok(out)
}

/// `filemetas` 单条查询；返回 `info[0]`。
pub(crate) async fn filemeta_value(d: &BaiduDrive, path: &str, dlink: bool) -> Result<Value> {
    let target = json_text(&json!([path]));
    let params = vec![
        p("target", target),
        p("dlink", if dlink { "1" } else { "0" }),
        p("text", "1"),
    ];
    let v = d
        .api_get("/api/filemetas", &params, ReqOpts::authed().no_channel())
        .await?;
    let info = v
        .get("info")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| Error::not_found(format!("路径不存在：{path}")))?;
    Ok(info[0].clone())
}

/// 查询单个路径的元信息。
pub(crate) async fn stat(d: &BaiduDrive, path: &str) -> Result<RemoteEntry> {
    let full = norm_path(path);
    if full == "/" {
        return Ok(root_entry());
    }
    let info = match filemeta_value(d, &full, false).await {
        Ok(v) => v,
        // ⚠️ 真机实测（2026-10-18）：`/api/filemetas` 对**不存在**的路径返回
        //   errno:12（响应里没有 errmsg），而不是 -9/404。不把 12 归一成
        //   NotFound，`ensure_dir` 的"stat 探测再创建"就会直接失败。
        Err(Error::Server { code: 12, .. }) => {
            return Err(Error::not_found(format!("路径不存在：{full}")));
        }
        Err(e) => return Err(e),
    };
    entry_from_value(&info, &parent_path(&full))
        .ok_or_else(|| Error::not_found(format!("路径不存在：{full}")))
}

/// `POST /api/create?isdir=1` 新建目录。
async fn create_dir(d: &BaiduDrive, path: &str) -> Result<Value> {
    let parent = parent_path(path);
    let params = vec![p("isdir", "1"), p("rtype", "1")];
    let form = vec![
        p("path", path),
        p("isdir", "1"),
        p("size", "0"),
        p("block_list", "[]"),
        p("target_path", parent),
    ];
    let v = d
        .api_post_form(
            "/api/create",
            &params,
            &form,
            ReqOpts::authed().with_bdstoken(),
        )
        .await?;
    wp_core::log_info!(
        "百度",
        "新建远端目录 {path}（fs_id={}）",
        v.get("fs_id").unwrap_or(&Value::Null)
    );
    Ok(v)
}

/// 递归确保远端目录存在（逐级 stat → 缺失再 create）。
///
/// 注意：百度对“新建已存在目录”返回 errno:0，却会悄悄建出带时间戳后缀的副本，
/// 所以这里必须先探测存在性，不能靠 create 报错。
pub(crate) async fn ensure_dir(d: &BaiduDrive, path: &str) -> Result<RemoteEntry> {
    if has_dot_segment(path) {
        return Err(Error::other(format!(
            "远端路径含非法路径段（. / ..）：{path:?}"
        )));
    }
    let full = norm_path(path);
    if full == "/" {
        return Ok(root_entry());
    }
    if d.cached_dir(&full) {
        return Ok(RemoteEntry::dir(base_name(&full), full));
    }

    let mut last_created: Option<Value> = None;
    let mut current = String::new();
    for seg in full.split('/').filter(|s| !s.is_empty()) {
        current.push('/');
        current.push_str(seg);
        if d.cached_dir(&current) {
            continue;
        }
        match stat(d, &current).await {
            Ok(entry) if entry.is_dir => {
                d.mark_dir(&current);
            }
            Ok(_) => {
                return Err(Error::server(
                    -1,
                    format!("{current} 已存在但不是目录，无法创建目录"),
                ));
            }
            Err(Error::NotFound(_)) => {
                let v = create_dir(d, &current).await?;
                d.mark_dir(&current);
                last_created = Some(v);
            }
            Err(e) => return Err(e),
        }
    }

    if let Some(v) = last_created {
        if let Some(entry) = entry_from_value(&v, &parent_path(&full)) {
            if entry.path == full {
                return Ok(entry);
            }
        }
    }
    match stat(d, &full).await {
        Ok(entry) => Ok(entry),
        Err(Error::NotFound(_)) => Ok(RemoteEntry::dir(base_name(&full), full)),
        Err(e) => Err(e),
    }
}

/// filemanager 提交；返回原始响应（含 taskid）。
async fn submit_management(
    d: &BaiduDrive,
    opera: &str,
    body: &Value,
    extra: &[(&str, &str)],
) -> Result<Value> {
    let mut params = vec![p("opera", opera), p("async", "2"), p("onnest", "fail")];
    for (k, v) in extra {
        params.push(p(k, *v));
    }
    let form = vec![p("filelist", json_text(body))];
    d.api_post_form(
        "/api/filemanager",
        &params,
        &form,
        ReqOpts::authed().with_bdstoken(),
    )
    .await
}

/// 提取 taskid；0 / 空视为同步完成。
pub(crate) fn task_id_of(v: &Value) -> Option<String> {
    let raw = v.get("taskid").or_else(|| v.get("task_id"))?;
    let text = if let Some(s) = raw.as_str() {
        s.trim().to_string()
    } else if let Some(n) = raw.as_i64() {
        n.to_string()
    } else {
        raw.as_u64()?.to_string()
    };
    if text.is_empty() || text == "0" {
        None
    } else {
        Some(text)
    }
}

/// 轮询 `/share/taskquery` 直到终态。
pub(crate) async fn wait_task(
    d: &BaiduDrive,
    taskid: &str,
    interval: Duration,
    timeout: Duration,
) -> Result<Value> {
    let start = Instant::now();
    loop {
        let params = vec![p("taskid", taskid)];
        let v = d
            .api_get("/share/taskquery", &params, ReqOpts::authed())
            .await?;
        let task_errno = v.get("task_errno").and_then(Value::as_i64).unwrap_or(0);
        if task_errno != 0 {
            return Err(Error::server(
                task_errno,
                format!("任务失败（task_errno={task_errno}）：{}", err_text(&v, "")),
            ));
        }
        let status = v.get("status").and_then(Value::as_str).unwrap_or("");
        match status {
            "success" => return Ok(v),
            "failed" => {
                return Err(Error::server(
                    -1,
                    format!("任务失败：{}", err_text(&v, "状态 failed")),
                ));
            }
            _ => {}
        }
        if start.elapsed() >= timeout {
            return Err(Error::server(
                -1,
                format!("任务 {taskid} 等待超时（{status}），请稍后刷新"),
            ));
        }
        tokio::time::sleep(interval).await;
    }
}

async fn report_after_task(d: &BaiduDrive, count: usize, v: &Value) -> Result<OpReport> {
    let mut report = OpReport::default();
    match task_id_of(v) {
        None => report.ok = count,
        Some(taskid) => {
            match wait_task(d, &taskid, Duration::from_secs(2), Duration::from_secs(120)).await {
                Ok(_) => report.ok = count,
                Err(e) => {
                    report.failed = count;
                    report.errors.push(e.to_string());
                }
            }
        }
    }
    Ok(report)
}

/// delete 的 filelist：纯字符串数组。
pub(crate) fn delete_filelist(paths: &[String]) -> Value {
    Value::Array(paths.iter().cloned().map(Value::String).collect())
}

/// rename 的 filelist 元素：带 id（能转数字就转）与 path + newname。
pub(crate) fn rename_item(entry: &RemoteEntry, new_name: &str) -> Value {
    let mut obj = serde_json::Map::new();
    if let Ok(id) = entry.id.parse::<i64>() {
        obj.insert("id".into(), json!(id));
    } else if !entry.id.is_empty() {
        obj.insert("id".into(), json!(entry.id));
    }
    obj.insert("path".into(), json!(entry.path));
    obj.insert("newname".into(), json!(new_name));
    Value::Object(obj)
}

/// move/copy 的 filelist 元素：path + dest + newname，不带 id。
pub(crate) fn move_item(path: &str, dest: &str) -> Value {
    json!({
        "path": path,
        "dest": dest,
        "newname": base_name(path),
    })
}

/// 批量删除（进回收站）。
pub(crate) async fn delete(d: &BaiduDrive, paths: &[String]) -> Result<OpReport> {
    if paths.is_empty() {
        return Err(Error::other("删除路径列表不能为空"));
    }
    let body = delete_filelist(paths);
    let v = submit_management(d, "delete", &body, &[("newVerify", "1")]).await?;
    wp_core::log_info!(
        "百度",
        "删除 {} 项，taskid={:?}",
        paths.len(),
        task_id_of(&v)
    );
    report_after_task(d, paths.len(), &v).await
}

/// 同目录内重命名。
pub(crate) async fn rename(d: &BaiduDrive, path: &str, new_name: &str) -> Result<RemoteEntry> {
    let new_name = new_name.trim();
    if new_name.is_empty() || new_name.contains('/') || new_name == "." || new_name == ".." {
        return Err(Error::other(format!("非法的新文件名：{new_name:?}")));
    }
    let full = norm_path(path);
    if full == "/" {
        return Err(Error::other("不能重命名根目录"));
    }
    let old = stat(d, &full).await?;
    let new_path = join_path(&parent_path(&full), new_name);
    let body = Value::Array(vec![rename_item(&old, new_name)]);
    let v = submit_management(d, "rename", &body, &[]).await?;
    if let Some(taskid) = task_id_of(&v) {
        wait_task(d, &taskid, Duration::from_secs(2), Duration::from_secs(120)).await?;
    }
    match stat(d, &new_path).await {
        Ok(entry) => Ok(entry),
        Err(Error::NotFound(_)) => Ok(RemoteEntry {
            id: old.id,
            name: new_name.to_string(),
            path: new_path,
            is_dir: old.is_dir,
            size: old.size,
            modified: old.modified,
            created: old.created,
            raw: old.raw,
        }),
        Err(e) => Err(e),
    }
}

/// 批量移动（`copy = false`）或复制（`copy = true`）。
pub(crate) async fn move_or_copy(
    d: &BaiduDrive,
    paths: &[String],
    dest_dir: &str,
    copy: bool,
) -> Result<OpReport> {
    if paths.is_empty() {
        return Err(Error::other("路径列表不能为空"));
    }
    let dest = norm_path(dest_dir);
    if has_dot_segment(dest_dir) {
        return Err(Error::other(format!("目标目录含非法路径段：{dest_dir:?}")));
    }
    // 目标目录不存在时逐级补齐，避免 filemanager 报目标不存在。
    ensure_dir(d, &dest).await?;
    let body = Value::Array(paths.iter().map(|p| move_item(p, &dest)).collect());
    let opera = if copy { "copy" } else { "move" };
    let v = submit_management(d, opera, &body, &[]).await?;
    wp_core::log_info!(
        "百度",
        "{opera} {} 项 → {dest}，taskid={:?}",
        paths.len(),
        task_id_of(&v)
    );
    report_after_task(d, paths.len(), &v).await
}

/// 按关键字搜索。
///
/// ⚠️ 研究文档未覆盖 `/api/search`（Python 源项目未实现搜索），这里按百度
/// Web 的公开接口形态实现：`key/dir/recursion/num/page`，返回 `list` 数组。
/// 真机可用性待确认；若接口形态变化会映射为 Server 错误。
pub(crate) async fn search(d: &BaiduDrive, keyword: &str, dir: &str) -> Result<Vec<RemoteEntry>> {
    let keyword = keyword.trim();
    if keyword.is_empty() {
        return Ok(Vec::new());
    }
    let dir = norm_path(dir);
    let mut out = Vec::new();
    let mut page = 1u32;
    while page <= 50 {
        let params = vec![
            p("key", keyword),
            p("dir", dir.clone()),
            p("recursion", "1"),
            p("order", "time"),
            p("desc", "1"),
            p("num", "100"),
            p("page", page.to_string()),
        ];
        let v = match d
            .api_get("/api/search", &params, ReqOpts::authed().no_channel())
            .await
        {
            Ok(v) => v,
            // 没有结果时部分接口返回 -9；按空结果处理。
            Err(Error::NotFound(_)) => break,
            Err(e) => return Err(e),
        };
        let Some(items) = v.get("list").and_then(Value::as_array) else {
            break;
        };
        if items.is_empty() {
            break;
        }
        for item in items {
            if let Some(entry) = entry_from_value(item, &dir) {
                out.push(entry);
            }
        }
        if items.len() < 100 {
            break;
        }
        page += 1;
    }
    wp_core::log_debug!("百度", "搜索“{keyword}” → {} 项", out.len());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_mapping_uses_isdir_not_category() {
        let dir = json!({
            "fs_id": 411921386376817i64,
            "path": "/我的资源/电影",
            "server_filename": "电影",
            "isdir": 1,
            "category": 6,
            "size": 0,
            "server_mtime": 1789325407i64,
            "server_ctime": 1789325000i64,
            "md5": "8e23f7635tb64136eddb0719602bc477"
        });
        let entry = entry_from_value(&dir, "/我的资源").unwrap();
        assert!(entry.is_dir);
        assert_eq!(entry.name, "电影");
        assert_eq!(entry.path, "/我的资源/电影");
        assert_eq!(entry.id, "411921386376817");
        assert_eq!(entry.modified, Some(1789325407));
        assert_eq!(entry.created, Some(1789325000));

        // 文件也可能 category=6，但 isdir=0
        let file = json!({
            "fs_id": 1,
            "path": "/a.bin",
            "server_filename": "a.bin",
            "isdir": 0,
            "category": 6,
            "size": 123,
            "server_mtime": 0i64
        });
        let entry = entry_from_value(&file, "/").unwrap();
        assert!(!entry.is_dir);
        assert_eq!(entry.size, 123);
        assert_eq!(entry.modified, None, "0 时间应视为未知");
    }

    #[test]
    fn entry_mapping_falls_back_to_dir_and_name() {
        let v = json!({"fs_id": "abc", "path": "/x/y.txt", "isdir": 0, "size": 1});
        let e = entry_from_value(&v, "/x").unwrap();
        assert_eq!(e.name, "y.txt");
        assert_eq!(e.path, "/x/y.txt");
        assert_eq!(e.id, "abc");

        let v = json!({"server_filename": "z.txt", "isdir": 0});
        let e = entry_from_value(&v, "/d").unwrap();
        assert_eq!(e.path, "/d/z.txt");
        assert!(entry_from_value(&json!({}), "/d").is_none());
    }

    #[test]
    fn filemanager_payload_shapes() {
        let paths = vec!["/a.png".to_string(), "/b.png".to_string()];
        assert_eq!(
            json_text(&delete_filelist(&paths)),
            r#"["/a.png","/b.png"]"#
        );

        let entry = RemoteEntry::file("IMG.jpg", "/IMG.jpg", 10).with_id("411921386376817");
        let item = rename_item(&entry, "看海.jpg");
        assert_eq!(item["id"], json!(411921386376817i64));
        assert_eq!(item["path"], "/IMG.jpg");
        assert_eq!(item["newname"], "看海.jpg");

        let item = move_item("/a.jpg", "/我的资源");
        assert_eq!(item["path"], "/a.jpg");
        assert_eq!(item["dest"], "/我的资源");
        assert_eq!(item["newname"], "a.jpg");
        assert!(item.get("id").is_none(), "move/copy 不应带 id");
    }

    #[test]
    fn dot_segments_are_rejected_before_normalization() {
        assert!(has_dot_segment("/a/./b"));
        assert!(has_dot_segment("/a/../b"));
        assert!(!has_dot_segment("/a/b...c"));
        assert_eq!(norm_path("/a/./b"), "/a/b");
    }

    #[test]
    fn task_id_parsing() {
        assert_eq!(task_id_of(&json!({"taskid": 0})), None);
        assert_eq!(task_id_of(&json!({"taskid": "0"})), None);
        assert_eq!(task_id_of(&json!({"taskid": 12345})), Some("12345".into()));
        assert_eq!(task_id_of(&json!({"task_id": "abc"})), Some("abc".into()));
        assert_eq!(task_id_of(&json!({})), None);
    }
}
