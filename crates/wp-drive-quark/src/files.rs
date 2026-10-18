//! 文件接口 + 路径 ↔ fid 解析缓存。
//!
//! 夸克没有「完整路径 → fid」接口，只能逐级 `file/sort`。
//! 这里维护两级缓存（均 10 秒 TTL）：
//! * `paths`：逻辑路径 → (fid, 条目)；
//! * `dirs`：目录 fid → 名字索引。
//!
//! 写操作（建目录/上传/删除/移动/重命名）完成后清空缓存；`file/sort` 有秒级
//! 可见性延迟，空列表会重试一次，查路径也有重试。

use crate::{
    as_bool, as_i64, as_str, as_u64, data_of, is_retryable_not_found, now_ms, pair,
    parse_timestamp_secs, QuarkDrive,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};
use wp_core::model::{OpReport, RemoteEntry};
use wp_core::util::{is_under, join_path, norm_path, parent_path};
use wp_core::{Error, Result};

const PAGE_SIZE: usize = 200;

// ───────────────────────── 缓存 ─────────────────────────

#[derive(Debug, Clone)]
pub struct CachedPath {
    pub fid: String,
    pub entry: RemoteEntry,
    pub at: Instant,
}

#[derive(Debug, Clone)]
pub struct CachedDir {
    pub entries: BTreeMap<String, RemoteEntry>,
    pub at: Instant,
}

pub struct PathCache {
    paths: HashMap<String, CachedPath>,
    dirs: HashMap<String, CachedDir>,
    ttl: Duration,
}

impl PathCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            paths: HashMap::new(),
            dirs: HashMap::new(),
            ttl,
        }
    }

    fn fresh(&self, at: Instant) -> bool {
        at.elapsed() <= self.ttl
    }

    pub fn get_path(&self, path: &str) -> Option<CachedPath> {
        self.paths.get(path).filter(|c| self.fresh(c.at)).cloned()
    }

    pub fn put_path(&mut self, path: &str, entry: RemoteEntry) {
        if entry.id.is_empty() {
            return;
        }
        self.paths.insert(
            path.to_string(),
            CachedPath {
                fid: entry.id.clone(),
                entry,
                at: Instant::now(),
            },
        );
    }

    pub fn get_dir(&self, fid: &str) -> Option<CachedDir> {
        self.dirs.get(fid).filter(|c| self.fresh(c.at)).cloned()
    }

    pub fn put_dir(&mut self, fid: &str, entries: BTreeMap<String, RemoteEntry>) {
        self.dirs.insert(
            fid.to_string(),
            CachedDir {
                entries,
                at: Instant::now(),
            },
        );
    }

    /// 把新条目塞进已缓存的父目录（创建目录/上传后立刻可见）。
    pub fn insert_entry(&mut self, parent_fid: &str, entry: RemoteEntry) {
        if let Some(dir) = self.dirs.get_mut(parent_fid) {
            dir.entries.insert(entry.name.clone(), entry.clone());
            dir.at = Instant::now();
        }
        self.put_path(&entry.path.clone(), entry);
    }

    pub fn invalidate(&mut self) {
        self.paths.clear();
        self.dirs.clear();
    }

    #[cfg(test)]
    pub fn path_count(&self) -> usize {
        self.paths.len()
    }
}

// ───────────────────────── 条目映射 ─────────────────────────

pub(crate) fn root_entry() -> RemoteEntry {
    let mut e = RemoteEntry::dir("/", "/");
    e.id = "0".into();
    e
}

pub(crate) fn map_entry(raw: &Value, dir_path: &str) -> Option<RemoteEntry> {
    let name = as_str(raw.get("file_name"))?;
    if name.is_empty() {
        return None;
    }
    let fid = as_str(raw.get("fid")).unwrap_or_default();
    let is_dir = as_bool(raw.get("dir")) == Some(true) || as_i64(raw.get("file_type")) == Some(0);
    let size = if is_dir {
        0
    } else {
        as_u64(raw.get("size")).unwrap_or(0)
    };
    let modified = parse_timestamp_secs(raw.get("updated_at").or_else(|| raw.get("created_at")));
    let created = parse_timestamp_secs(raw.get("created_at"));
    let thumbnail = crate::thumbnail::thumbnail_from_raw(raw);
    Some(RemoteEntry {
        id: fid,
        name: name.clone(),
        path: join_path(dir_path, &name),
        is_dir,
        size,
        modified,
        created,
        thumbnail,
        raw: Some(raw.clone()),
    })
}

/// 解析结果。
pub(crate) struct Resolved {
    pub fid: String,
    pub is_dir: bool,
    pub entry: Option<RemoteEntry>,
    pub path: String,
}

// ───────────────────────── 文件操作 ─────────────────────────

impl QuarkDrive {
    pub(crate) fn invalidate_cache(&self) {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invalidate();
    }

    async fn get_dir_entries(
        &self,
        fid: &str,
        dir_path: &str,
    ) -> Result<BTreeMap<String, RemoteEntry>> {
        let fid = if fid.is_empty() { "0" } else { fid };
        {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(dir) = cache.get_dir(fid) {
                return Ok(dir.entries);
            }
        }
        let (list, _) = self.fetch_dir_raw(fid).await?;
        let mut entries: BTreeMap<String, RemoteEntry> = BTreeMap::new();
        for raw in &list {
            if let Some(entry) = map_entry(raw, dir_path) {
                entries.entry(entry.name.clone()).or_insert(entry);
            }
        }
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .put_dir(fid, entries.clone());
        Ok(entries)
    }

    /// 拉取一页或多页目录列表。首屏为空时等待 1.5 秒再取一次（可见性延迟）。
    async fn fetch_dir_raw(&self, fid: &str) -> Result<(Vec<Value>, u64)> {
        let mut page = 1usize;
        let mut out: Vec<Value> = Vec::new();
        let mut total = 0u64;
        let mut retried_empty = false;
        loop {
            let params = vec![
                pair("pdir_fid", fid),
                pair("_page", page.to_string()),
                pair("_size", PAGE_SIZE.to_string()),
                pair("_sort", "file_name:asc"),
                pair("_fetch_total", "1"),
            ];
            let resp = self.api.get("file/sort", params).await?;
            let data = data_of(&resp);
            let list = data
                .get("list")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if page == 1 {
                total = as_u64(
                    resp.get("metadata")
                        .and_then(|m| m.get("_total"))
                        .or_else(|| data.get("total")),
                )
                .unwrap_or(list.len() as u64);
            }
            let n = list.len();
            out.extend(list);

            if out.is_empty() && !retried_empty {
                retried_empty = true;
                page = 1;
                total = 0;
                tokio::time::sleep(Duration::from_millis(1500)).await;
                continue;
            }
            if n == 0 || out.len() as u64 >= total || page >= 100 {
                break;
            }
            page += 1;
        }
        Ok((out, total))
    }

    /// 路径 → fid/条目。
    pub(crate) async fn resolve(&self, path: &str) -> Result<Resolved> {
        let p = norm_path(path);
        if p == "/" {
            return Ok(Resolved {
                fid: "0".into(),
                is_dir: true,
                entry: Some(root_entry()),
                path: p,
            });
        }
        if let Some(cached) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_path(&p)
        {
            return Ok(Resolved {
                fid: cached.fid,
                is_dir: cached.entry.is_dir,
                entry: Some(cached.entry),
                path: p,
            });
        }

        let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
        let mut fid = "0".to_string();
        let mut cur = "/".to_string();
        let mut last: Option<RemoteEntry> = None;
        for (i, seg) in segs.iter().enumerate() {
            let entries = self.get_dir_entries(&fid, &cur).await?;
            let Some(mut entry) = entries.get(*seg).cloned() else {
                return Err(Error::not_found(format!(
                    "路径不存在：{cur}/{seg}（在 fid={fid} 下未找到）"
                )));
            };
            let child_path = join_path(&cur, seg);
            entry.path = child_path.clone();
            if !entry.is_dir && i + 1 < segs.len() {
                return Err(Error::not_found(format!(
                    "不是目录：{child_path}（路径中还有后续段）"
                )));
            }
            self.cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .put_path(&child_path, entry.clone());
            fid = entry.id.clone();
            cur = child_path;
            last = Some(entry);
        }

        Ok(Resolved {
            fid,
            is_dir: last.as_ref().map(|e| e.is_dir).unwrap_or(true),
            entry: last,
            path: cur,
        })
    }

    /// 带重试的解析：`file/sort` 有秒级可见性延迟，NotFound 时清缓存重试。
    pub(crate) async fn resolve_retry(
        &self,
        path: &str,
        attempts: u32,
        base_delay_ms: u64,
    ) -> Result<RemoteEntry> {
        let attempts = attempts.max(1);
        let mut last_err: Option<Error> = None;
        for i in 0..attempts {
            match self.resolve(path).await {
                Ok(r) => {
                    return Ok(r.entry.unwrap_or_else(root_entry));
                }
                Err(e) if is_retryable_not_found(&e) => {
                    last_err = Some(e);
                    if i + 1 < attempts {
                        self.invalidate_cache();
                        let delay = base_delay_ms.saturating_mul(i as u64 + 1);
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| Error::not_found(format!("路径不存在：{path}"))))
    }

    pub(crate) async fn stat_entry(&self, path: &str) -> Result<RemoteEntry> {
        self.api.ensure_valid()?;
        self.resolve_retry(path, 2, 500).await
    }

    pub(crate) async fn list_entries(&self, path: &str) -> Result<Vec<RemoteEntry>> {
        self.api.ensure_valid()?;
        let dir = self.resolve_retry(path, 2, 500).await?;
        if !dir.is_dir {
            return Err(Error::other(format!("不是目录：{path}")));
        }
        let dir_path = norm_path(path);
        let entries = self.get_dir_entries(&dir.id, &dir_path).await?;
        Ok(entries.into_values().collect())
    }

    async fn create_folder(&self, parent_fid: &str, name: &str) -> Result<String> {
        let body = json!({
            "pdir_fid": parent_fid,
            "file_name": name,
            "dir_init_lock": false,
            "dir_path": "",
        });
        let resp = self.api.post("file", body).await?;
        let data = data_of(&resp);
        let fid = as_str(data.get("fid"))
            .or_else(|| as_str(resp.get("fid")))
            .unwrap_or_default();
        if fid.is_empty() {
            return Err(Error::other(format!(
                "创建目录 '{name}' 未返回 fid：{}",
                crate::compact_json(&resp)
            )));
        }
        Ok(fid)
    }

    /// 逐级创建目录；整个流程用 tokio Mutex 串行化，避免并发同名 create。
    pub(crate) async fn ensure_dir_entry(&self, path: &str) -> Result<RemoteEntry> {
        self.api.ensure_valid()?;
        let p = norm_path(path);
        if p == "/" {
            return Ok(root_entry());
        }
        let _guard = self.dir_lock.lock().await;
        let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
        let mut fid = "0".to_string();
        let mut cur = "/".to_string();
        let mut last = root_entry();
        let mut created_any = false;

        for seg in segs {
            let entries = self.get_dir_entries(&fid, &cur).await?;
            if let Some(entry) = entries.get(seg) {
                if !entry.is_dir {
                    return Err(Error::other(format!("路径中 '{seg}' 是文件，不能作为目录")));
                }
                fid = entry.id.clone();
                cur = entry.path.clone();
                last = entry.clone();
                continue;
            }
            let new_fid = self.create_folder(&fid, seg).await?;
            let child_path = join_path(&cur, seg);
            let mut entry = RemoteEntry::dir(seg, child_path.clone());
            entry.id = new_fid.clone();
            entry.created = Some(now_ms() / 1000);
            entry.modified = entry.created;
            {
                let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
                if fid != "0" || cur != "/" {
                    cache.insert_entry(&fid, entry.clone());
                }
                cache.put_path(&child_path, entry.clone());
            }
            fid = new_fid;
            cur = child_path;
            last = entry;
            created_any = true;
        }
        if created_any {
            // 目录结构变了，清掉沿途缓存，后续读取走可见性重试。
            self.invalidate_cache();
        }
        Ok(last)
    }

    pub(crate) async fn delete_paths(&self, paths: &[String]) -> Result<OpReport> {
        self.api.ensure_valid()?;
        let mut report = OpReport::default();
        if paths.is_empty() {
            return Ok(report);
        }
        let mut fids = Vec::new();
        for path in paths {
            match self.resolve(path).await {
                Ok(r) => {
                    if r.fid == "0" || norm_path(path) == "/" {
                        report.fail("不能删除根目录");
                    } else {
                        fids.push(r.fid);
                    }
                }
                Err(e) => report.fail(format!("{path}：{}", wp_core::error::err_line(&e))),
            }
        }
        if fids.is_empty() {
            return Ok(report);
        }
        let resp = self.api.post("file/delete", delete_body(&fids)).await?;
        let data = data_of(&resp);
        let task_id = as_str(data.get("task_id")).unwrap_or_default();
        if task_id.is_empty() {
            for _ in &fids {
                report.success();
            }
        } else {
            match self.api.wait_task(&task_id, "删除").await {
                Ok(_) => {
                    for _ in &fids {
                        report.success();
                    }
                }
                Err(e) => {
                    let why = wp_core::error::err_line(&e);
                    for fid in &fids {
                        report.fail(format!("fid={fid}：{why}"));
                    }
                }
            }
        }
        self.invalidate_cache();
        Ok(report)
    }

    pub(crate) async fn rename_path(&self, path: &str, new_name: &str) -> Result<RemoteEntry> {
        self.api.ensure_valid()?;
        let new_name = new_name.trim();
        if new_name.is_empty() {
            return Err(Error::other("新名称为空"));
        }
        if new_name.contains('/') || new_name == "." || new_name == ".." {
            return Err(Error::other("新名称不能包含 '/'"));
        }
        let old = self.resolve_retry(path, 2, 400).await?;
        if old.id.is_empty() || old.id == "0" {
            return Err(Error::other("不能重命名根目录"));
        }
        if old.name == new_name {
            return Ok(old);
        }
        let parent = parent_path(path);
        let new_path = join_path(&parent, new_name);
        let resp = self
            .api
            .post(
                "file/rename",
                json!({"fid": &old.id, "file_name": new_name}),
            )
            .await?;
        let data = data_of(&resp);
        if let Some(task_id) = as_str(data.get("task_id")) {
            if !task_id.is_empty() {
                self.api.wait_task(&task_id, "重命名").await?;
            }
        }
        self.invalidate_cache();
        match self.resolve_retry(&new_path, 4, 600).await {
            Ok(mut entry) => {
                if entry.id.is_empty() {
                    entry.id = old.id;
                }
                Ok(entry)
            }
            Err(_) => {
                // 服务端任务已完成但列表还没刷新：返回合成条目，避免 GUI 误报失败。
                Ok(RemoteEntry {
                    id: old.id,
                    name: new_name.to_string(),
                    path: new_path,
                    is_dir: old.is_dir,
                    size: old.size,
                    modified: old.modified,
                    created: old.created,
                    thumbnail: old.thumbnail,
                    raw: old.raw,
                })
            }
        }
    }

    pub(crate) async fn move_paths(&self, paths: &[String], dest_dir: &str) -> Result<OpReport> {
        self.api.ensure_valid()?;
        let mut report = OpReport::default();
        if paths.is_empty() {
            return Ok(report);
        }
        let dest = self.resolve_retry(dest_dir, 2, 400).await?;
        if !dest.is_dir {
            return Err(Error::other(format!("目标不是目录：{dest_dir}")));
        }
        let dest_norm = norm_path(dest_dir);
        let mut fids = Vec::new();
        for path in paths {
            match self.resolve(path).await {
                Ok(r) => {
                    if r.fid == "0" || r.fid == dest.id {
                        report.fail(format!("{path}：不能移动根目录或到自身"));
                    } else if r.is_dir && is_under(&r.path, &dest_norm) {
                        report.fail(format!("{path}：不能移动到自己的子目录"));
                    } else {
                        fids.push(r.fid);
                    }
                }
                Err(e) => report.fail(format!("{path}：{}", wp_core::error::err_line(&e))),
            }
        }
        if fids.is_empty() {
            return Ok(report);
        }
        let resp = self
            .api
            .post(
                "file/move",
                json!({"filelist": &fids, "to_pdir_fid": &dest.id, "exclude_fids": []}),
            )
            .await?;
        let data = data_of(&resp);
        let task_id = as_str(data.get("task_id")).unwrap_or_default();
        if task_id.is_empty() {
            for _ in &fids {
                report.success();
            }
        } else {
            match self.api.wait_task(&task_id, "移动").await {
                Ok(_) => {
                    for _ in &fids {
                        report.success();
                    }
                }
                Err(e) => {
                    let why = wp_core::error::err_line(&e);
                    for fid in &fids {
                        report.fail(format!("fid={fid}：{why}"));
                    }
                }
            }
        }
        self.invalidate_cache();
        Ok(report)
    }

    pub(crate) async fn search_entries(
        &self,
        keyword: &str,
        dir: &str,
    ) -> Result<Vec<RemoteEntry>> {
        self.api.ensure_valid()?;
        let dir_entry = self.resolve_retry(dir, 2, 400).await?;
        if !dir_entry.is_dir {
            return Err(Error::other(format!("不是目录：{dir}")));
        }
        let dir_norm = norm_path(dir);
        let mut out: Vec<RemoteEntry> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for page in 1..=5usize {
            let mut params = vec![
                pair("q", keyword),
                pair("_page", page.to_string()),
                pair("_size", "100".to_string()),
                pair("_fetch_total", "1"),
                pair("_is_hl", "1"),
            ];
            if dir_entry.id != "0" {
                // 服务端支持 pdir_fid 时按目录过滤；不支持时下面再按 pdir_fid 二次过滤。
                params.push(pair("pdir_fid", dir_entry.id.clone()));
            }
            let resp = self.api.get("file/search", params).await?;
            let data = data_of(&resp);
            let list = data
                .get("list")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let n = list.len();
            for raw in &list {
                if dir_entry.id != "0" {
                    if let Some(pdir) = as_str(raw.get("pdir_fid")) {
                        if !pdir.is_empty() && pdir != dir_entry.id {
                            continue;
                        }
                    }
                }
                let Some(mut entry) = map_entry(raw, &dir_norm) else {
                    continue;
                };
                // 服务端如果直接给了 path，优先使用。
                if let Some(p) = as_str(raw.get("path")) {
                    if p.starts_with('/') {
                        entry.path = norm_path(&p);
                    }
                }
                if !seen.insert(entry.id.clone()) {
                    continue;
                }
                out.push(entry);
            }
            if n < 100 {
                break;
            }
        }
        Ok(out)
    }
}

/// 删除请求体：**绝不包含 `current_dir_fid`**。
pub(crate) fn delete_body(fids: &[String]) -> Value {
    json!({
        "action_type": 2,
        "filelist": fids,
        "exclude_fids": [],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, path: &str) -> RemoteEntry {
        RemoteEntry::file(name, path, 10).with_id(format!("fid-{name}"))
    }

    #[test]
    fn cache_ttl_and_invalidate() {
        let mut cache = PathCache::new(Duration::from_secs(10));
        cache.put_path("/a.txt", entry("a.txt", "/a.txt"));
        assert!(cache.get_path("/a.txt").is_some());
        assert_eq!(cache.path_count(), 1);

        // 手动把时间拨到 11 秒前 → 过期。
        cache.paths.get_mut("/a.txt").unwrap().at = Instant::now() - Duration::from_secs(11);
        assert!(cache.get_path("/a.txt").is_none());

        cache.put_path("/a.txt", entry("a.txt", "/a.txt"));
        let mut entries = BTreeMap::new();
        entries.insert("a.txt".to_string(), entry("a.txt", "/a.txt"));
        cache.put_dir("0", entries);
        assert_eq!(cache.get_dir("0").unwrap().entries.len(), 1);
        let new = RemoteEntry::dir("b", "/b").with_id("fid-b");
        cache.insert_entry("0", new);
        assert!(cache.get_path("/b").is_some());
        assert_eq!(cache.get_dir("0").unwrap().entries.len(), 2);
        cache.invalidate();
        assert!(cache.get_path("/a.txt").is_none());
        assert!(cache.get_dir("0").is_none());
    }

    #[test]
    fn delete_body_never_has_current_dir_fid() {
        let body = delete_body(&["f1".into(), "f2".into()]);
        assert_eq!(body["action_type"], 2);
        assert_eq!(body["filelist"].as_array().unwrap().len(), 2);
        assert_eq!(body["exclude_fids"].as_array().unwrap().len(), 0);
        assert!(body.get("current_dir_fid").is_none());
        assert!(body.get("fid_list").is_none());
    }

    #[test]
    fn map_entry_dir_and_timestamp() {
        let raw = json!({
            "fid": "f1",
            "file_name": "电影",
            "file_type": 0,
            "dir": false,
            "size": 123,
            "updated_at": 1789322627000i64,
        });
        let e = map_entry(&raw, "/").unwrap();
        assert!(e.is_dir);
        assert_eq!(e.id, "f1");
        assert_eq!(e.path, "/电影");
        assert_eq!(e.size, 0);
        assert_eq!(e.modified, Some(1789322627));

        let raw = json!({
            "fid": "f2",
            "file_name": "a.bin",
            "dir": true,
            "file_type": 1,
            "created_at": 1789322627i64,
        });
        let e = map_entry(&raw, "/文档").unwrap();
        assert!(e.is_dir);
        assert_eq!(e.path, "/文档/a.bin");
        assert_eq!(e.created, Some(1789322627));

        let raw = json!({"fid":"f3","file_name":"b.bin","file_type":1,"size":"42"});
        let e = map_entry(&raw, "/").unwrap();
        assert!(!e.is_dir);
        assert_eq!(e.size, 42);
        assert_eq!(map_entry(&json!({"fid":"x"}), "/"), None);
    }

    #[test]
    fn map_entry_extracts_thumbnail() {
        let raw = json!({
            "fid": "f9",
            "file_name": "p.jpg",
            "file_type": 1,
            "size": "1024",
            "thumbnail": "https://cdn.example/p.jpg?sign=1\\u0026t=2",
        });
        let e = map_entry(&raw, "/图片").unwrap();
        assert_eq!(
            e.thumbnail.as_deref(),
            Some("https://cdn.example/p.jpg?sign=1&t=2")
        );

        // 空串 / 缺失都当作没有缩略图。
        let blank = json!({"fid":"f10","file_name":"a.bin","thumbnail":""});
        assert_eq!(map_entry(&blank, "/").unwrap().thumbnail, None);
        let missing = json!({"fid":"f11","file_name":"b.bin"});
        assert_eq!(map_entry(&missing, "/").unwrap().thumbnail, None);
    }
}
