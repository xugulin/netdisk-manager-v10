//! 云添加（离线下载）：直链/magnet 任务创建、任务列表、直链解析、种子解析。
//!
//! `Drive::cloud_add / cloud_tasks` 的实现也在本模块；源码里已有但 trait 尚未
//! 暴露的种子解析/等待任务等辅助能力保留，模块整体 `allow(dead_code)`。
//!
//! 协议（§4.3/§7.1）：
//! * 直链：`resolve_res`（预解析，尽力而为）→ `create_task`；
//! * magnet：直接 `create_task`，`url` 原样保留（含 dn/tr），`dn` 作为可选 `newName`；
//! * 任务列表：`list_task`，状态 0 等待 / 1 下载 / 2 完成 / 3 失败 / 4 暂停 / 5 删除。

use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};
use wp_core::model::{cloud_status_text, CloudTask, ProgressFn};
use wp_core::{Error, Result};

use crate::files::{first_str, first_ts, first_u64};
use crate::{as_i64, data_or_self, snippet, GuangyaDrive, API_BASE};

/// 默认云添加目录（全角冒号，不能改）。
pub(crate) const DEFAULT_CLOUD_DIR: &str = "来自：云添加";
const STATUS_IN_PROGRESS: [i64; 4] = [0, 1, 3, 4];
const STATUS_ALL: [i64; 6] = [0, 1, 2, 3, 4, 5];
/// 任务列表防御性翻页上限。
const MAX_TASK_PAGES: usize = 20;

impl GuangyaDrive {
    /// `resolve_res`：解析直链，返回 `urlResInfo{url,fileName,fileSize}`。
    pub(crate) async fn cloud_resolve_url(&self, url: &str) -> Result<Value> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(Error::other("只支持 http/https 直链"));
        }
        let value = self
            .api_post(
                "/cloudcollection/v1/resolve_res",
                json!({ "url": url }),
                &[],
            )
            .await?;
        parse_url_resolve(data_or_self(&value))
    }

    /// `resolve_torrent`（multipart/form-data，字段名 `torrent`），返回 `btResInfo`。
    pub(crate) async fn cloud_resolve_torrent(&self, path: &Path) -> Result<Value> {
        let bytes = tokio::fs::read(path).await.map_err(Error::Io)?;
        let file_name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "a.torrent".to_string());
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str("application/octet-stream")
            .map_err(|e| Error::other(format!("构造种子表单失败：{e}")))?;
        let form = reqwest::multipart::Form::new().part("torrent", part);

        let token = self.bearer().await?;
        let mut headers = self.api_headers(None)?;
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| Error::other(format!("Authorization 头非法：{e}")))?,
        );
        let resp = self
            .api
            .post(format!("{API_BASE}/cloudcollection/v1/resolve_torrent"))
            .headers(headers)
            .multipart(form)
            .send()
            .await
            .map_err(wp_net::client::net_err)?;
        let status = resp.status().as_u16();
        let request_id = resp
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let text = resp.text().await.map_err(wp_net::client::net_err)?;
        let value = crate::parse_api_body(status, &text, request_id.as_deref(), &[])?;
        parse_torrent_resolve(data_or_self(&value))
    }

    /// 幂等创建默认目录；159 时复用服务端返回的 `fileId`。
    pub(crate) async fn cloud_ensure_default_dir(&self, parent_id: &str) -> Result<String> {
        let value = self
            .api_post(
                "/userres/v1/file/create_dir",
                json!({
                    "dirName": DEFAULT_CLOUD_DIR,
                    "parentId": parent_id,
                    "failIfNameExist": true,
                }),
                &[159],
            )
            .await?;
        // fileId 可能是 JSON number；统一转字符串。
        first_str(data_or_self(&value), &["fileId", "file_id"]).ok_or_else(|| {
            Error::server(
                0,
                format!("确保默认目录失败：{}", snippet(&value.to_string(), 300)),
            )
        })
    }

    /// 创建直链任务，返回 taskId。
    pub(crate) async fn cloud_create_url_task(&self, url: &str, parent_id: &str) -> Result<String> {
        self.cloud_create_task(url, parent_id, None).await
    }

    /// 创建 BT 任务（magnet 只用 btih，与 Python 一致），返回 taskId。
    pub(crate) async fn cloud_create_bt_task(
        &self,
        info_hash: &str,
        parent_id: &str,
        new_name: &str,
    ) -> Result<String> {
        self.cloud_create_task(&magnet_for(info_hash), parent_id, Some(new_name))
            .await
    }

    /// 通用 `create_task`：`newName` 为空时不下发。
    pub(crate) async fn cloud_create_task(
        &self,
        url: &str,
        parent_id: &str,
        new_name: Option<&str>,
    ) -> Result<String> {
        let mut body = json!({ "url": url, "parentId": parent_id });
        if let Some(name) = new_name.map(str::trim).filter(|n| !n.is_empty()) {
            body["newName"] = json!(name);
        }
        let value = self
            .api_post("/cloudcollection/v1/create_task", body, &[])
            .await?;
        task_id_of(&value)
    }

    /// 查询全部云任务（`status:[0..5]`，按 cursor 翻页）。
    pub(crate) async fn cloud_tasks_all(&self) -> Result<Vec<CloudTask>> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        for _ in 0..MAX_TASK_PAGES {
            let data = self
                .cloud_list_tasks(None, Some(&STATUS_ALL), 100, &cursor)
                .await?;
            let page = cloud_tasks_from_data(&data);
            let count = page.len();
            out.extend(page);
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
        Ok(out)
    }

    /// 目标目录 → parentId；空/根目录落到「来自：云添加」默认目录。
    async fn cloud_dest_parent(&self, dest_dir: &str) -> Result<String> {
        let dest = dest_dir.trim();
        if dest.is_empty() || wp_core::util::norm_path(dest) == "/" {
            return self.cloud_ensure_default_dir("").await;
        }
        let entry = crate::files::ensure_dir(self, dest).await?;
        if entry.id.is_empty() {
            // 防御性兜底：ensure_dir("/") 才会返回空 id，上面的分支已拦截。
            self.cloud_ensure_default_dir("").await
        } else {
            Ok(entry.id)
        }
    }

    /// 查询云任务：优先按 taskIds；否则按状态分页。
    pub(crate) async fn cloud_list_tasks(
        &self,
        task_ids: Option<&[String]>,
        statuses: Option<&[i64]>,
        page_size: u32,
        cursor: &str,
    ) -> Result<Value> {
        let body = if let Some(ids) = task_ids.filter(|ids| !ids.is_empty()) {
            json!({ "taskIds": ids })
        } else {
            let statuses = statuses.unwrap_or(&STATUS_ALL);
            json!({
                "pageSize": page_size,
                "status": statuses,
                "cursor": cursor,
            })
        };
        let value = self
            .api_post("/cloudcollection/v1/list_task", body, &[])
            .await?;
        Ok(data_or_self(&value).clone())
    }

    /// 轮询单个云任务直到完成（2）；失败/暂停/删除（3/4/5）立即报错。
    pub(crate) async fn cloud_wait_task(&self, task_id: &str) -> Result<Value> {
        let started = Instant::now();
        loop {
            let data = self
                .cloud_list_tasks(Some(&[task_id.to_string()]), None, 100, "")
                .await?;
            let detail = data
                .get("list")
                .and_then(Value::as_array)
                .and_then(|l| l.first())
                .cloned();
            if let Some(detail) = detail {
                let status = detail.get("status").and_then(as_i64).unwrap_or(-1);
                if status == 2 {
                    return Ok(detail);
                }
                if matches!(status, 3 | 4 | 5) {
                    return Err(Error::server(
                        status,
                        format!("云添加任务 {task_id} 终止：{}", cloud_status_label(status)),
                    ));
                }
            }
            if started.elapsed() >= Duration::from_secs(3600) {
                return Err(Error::server(
                    0,
                    format!("云添加任务 {task_id} 等待超时（3600s）"),
                ));
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    /// 完整直链云添加流程：解析 → 默认目录 → 建任务。
    pub(crate) async fn cloud_add_url(&self, url: &str) -> Result<(String, Value)> {
        let info = self.cloud_resolve_url(url).await?;
        let dir = self.cloud_ensure_default_dir("").await?;
        let task_id = self.cloud_create_url_task(url, &dir).await?;
        Ok((task_id, info))
    }

    /// 完整 BT 云添加流程：解析种子 → 默认目录 → 建任务。
    pub(crate) async fn cloud_add_torrent(&self, path: &Path) -> Result<(String, Value)> {
        let info = self.cloud_resolve_torrent(path).await?;
        let hash = info
            .get("infoHash")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = info
            .get("fileName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let dir = self.cloud_ensure_default_dir("").await?;
        let task_id = self.cloud_create_bt_task(&hash, &dir, &name).await?;
        Ok((task_id, info))
    }

    /// 进度回调版本（可选；GUI 未来接云添加时可复用）。
    #[allow(dead_code)]
    pub(crate) async fn cloud_wait_task_with_progress(
        &self,
        task_id: &str,
        mut progress: ProgressFn,
    ) -> Result<Value> {
        let detail = self.cloud_wait_task(task_id).await?;
        let total = detail.get("totalSize").and_then(as_i64).unwrap_or(0).max(0) as u64;
        let _ = progress(total, total);
        Ok(detail)
    }
}

/// `Drive::cloud_add` 实现：直链与 magnet 都走 `create_task`。
///
/// * 直链先尽力调 `resolve_res` 预解析（失败只记日志，仍允许提交）；
/// * magnet 保留原始 `dn/tr`，`dn` 作为可选的 `newName`；
/// * 目标目录先 `ensure_dir` 拿 parentId，空/根目录用「来自：云添加」。
pub(crate) async fn add(drive: &GuangyaDrive, raw_url: &str, dest_dir: &str) -> Result<String> {
    let source = classify_cloud_url(raw_url)?;
    let parent_id = drive.cloud_dest_parent(dest_dir).await?;
    match &source {
        CloudSource::Url { url } => {
            match drive.cloud_resolve_url(url).await {
                Ok(info) => wp_core::log_debug!(
                    "光鸭",
                    "云添加直链预解析成功：{}",
                    info.get("fileName").and_then(Value::as_str).unwrap_or("?")
                ),
                Err(e) => {
                    wp_core::log_debug!("光鸭", "云添加直链预解析失败（不阻断提交）：{e}")
                }
            }
            drive.cloud_create_task(url, &parent_id, None).await
        }
        CloudSource::Magnet { url, name, .. } => {
            drive
                .cloud_create_task(url, &parent_id, name.as_deref())
                .await
        }
    }
}

/// `Drive::cloud_tasks` 实现。
pub(crate) async fn tasks(drive: &GuangyaDrive) -> Result<Vec<CloudTask>> {
    drive.cloud_tasks_all().await
}

/// 校验后的云添加链接。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloudSource {
    /// http/https 直链。
    Url { url: String },
    /// magnet 链接；`url` 原样保留，`dn` 可选。
    Magnet {
        url: String,
        info_hash: String,
        name: Option<String>,
    },
}

/// 校验并分类云添加链接：http/https 直链或 magnet（必须有 btih）。
///
/// 只做协议层校验，不做 DNS/网络探测；非法输入返回可读错误。
pub(crate) fn classify_cloud_url(raw: &str) -> Result<CloudSource> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(Error::other("云添加链接不能为空"));
    }
    let lower = text.to_ascii_lowercase();
    if lower.starts_with("magnet:?") {
        let parsed =
            url::Url::parse(text).map_err(|e| Error::other(format!("magnet 链接格式非法：{e}")))?;
        let mut info_hash = String::new();
        let mut name = None;
        for (key, value) in parsed.query_pairs() {
            if info_hash.is_empty()
                && key.eq_ignore_ascii_case("xt")
                && value.to_ascii_lowercase().starts_with("urn:btih:")
            {
                info_hash = value["urn:btih:".len()..].trim().to_string();
            } else if key.eq_ignore_ascii_case("dn") {
                let name_value = value.trim().to_string();
                if !name_value.is_empty() {
                    name = Some(name_value);
                }
            }
        }
        if info_hash.is_empty() {
            return Err(Error::other(
                "magnet 链接缺少 infoHash（需要 xt=urn:btih:<hash>）",
            ));
        }
        if !is_valid_btih(&info_hash) {
            return Err(Error::other(format!(
                "magnet 的 infoHash 非法（应为 32 位 base32 或 40 位十六进制）：{}",
                snippet(&info_hash, 48)
            )));
        }
        return Ok(CloudSource::Magnet {
            url: text.to_string(),
            info_hash,
            name,
        });
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        let parsed =
            url::Url::parse(text).map_err(|e| Error::other(format!("直链 URL 格式非法：{e}")))?;
        if parsed.host_str().map(str::is_empty).unwrap_or(true) {
            return Err(Error::other(format!(
                "直链 URL 缺少主机名：{}",
                snippet(text, 120)
            )));
        }
        return Ok(CloudSource::Url {
            url: text.to_string(),
        });
    }
    Err(Error::other(
        "只支持 http/https 直链或 magnet 链接（magnet:?xt=urn:btih:...）",
    ))
}

fn is_valid_btih(hash: &str) -> bool {
    let len = hash.len();
    (len == 32 || len == 40) && hash.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// 云任务 → 统一模型（纯逻辑）。
pub(crate) fn cloud_task_from_value(v: &Value) -> Option<CloudTask> {
    let id = first_str(v, &["taskId", "task_id", "id"])?;
    let status = raw_status(v);
    let status_code = as_i64(v.get("status").unwrap_or(&Value::Null));
    let total = first_u64(v, &["fileSize", "totalSize", "size"]).unwrap_or(0);
    let done = first_u64(v, &["finishedSize", "doneSize", "downloadedSize"]).unwrap_or(0);
    let percent = first_f64(v, &["progress"]);
    Some(CloudTask {
        id,
        name: first_str(v, &["fileName", "name", "title"]).unwrap_or_default(),
        url: first_str(v, &["url", "res", "sourceUrl"]).unwrap_or_default(),
        status: status.clone(),
        status_text: cloud_status_text(&status),
        progress: progress_fraction(total, done, status_code, percent),
        total,
        done,
        created: first_ts(v, &["createTime", "ctime", "createdAt"]),
        save_path: first_str(v, &["savePath", "parentName", "path"]).unwrap_or_default(),
        error: first_str(v, &["errorMsg", "errMsg", "error", "message"]).unwrap_or_default(),
    })
}

/// 从 `data.list` 映射云任务列表。
pub(crate) fn cloud_tasks_from_data(data: &Value) -> Vec<CloudTask> {
    data.get("list")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(cloud_task_from_value).collect())
        .unwrap_or_default()
}

/// 进度统一为 0.0–1.0：优先 `finishedSize/fileSize`，否则把服务端
/// `progress` 百分比除以 100；已完成（status=2）一律 1.0（与源码 UI 一致）。
pub(crate) fn progress_fraction(
    total: u64,
    done: u64,
    status: Option<i64>,
    percent: Option<f64>,
) -> f32 {
    if status == Some(2) {
        return 1.0;
    }
    if total > 0 {
        return (done.min(total) as f64 / total as f64).clamp(0.0, 1.0) as f32;
    }
    if let Some(p) = percent {
        return (p / 100.0).clamp(0.0, 1.0) as f32;
    }
    0.0
}

fn raw_status(v: &Value) -> String {
    match v.get("status") {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(n @ Value::Number(_)) => n.to_string(),
        _ => String::new(),
    }
}

fn first_f64(v: &Value, keys: &[&str]) -> Option<f64> {
    for key in keys {
        let Some(value) = v.get(*key) else { continue };
        let n = match value {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        };
        if let Some(n) = n {
            if n.is_finite() && n >= 0.0 {
                return Some(n);
            }
        }
    }
    None
}

/// 解析直链结果。
pub(crate) fn parse_url_resolve(data: &Value) -> Result<Value> {
    let info = if data
        .get("urlResInfo")
        .map(Value::is_object)
        .unwrap_or(false)
    {
        data["urlResInfo"].clone()
    } else {
        json!({
            "url": data.get("url").cloned().unwrap_or(Value::Null),
            "fileName": data.get("fileName").cloned().unwrap_or(Value::Null),
            "fileSize": data.get("fileSize").cloned().unwrap_or(Value::Null),
        })
    };
    if info
        .get("fileName")
        .and_then(Value::as_str)
        .map(|s| s.trim().is_empty())
        .unwrap_or(true)
    {
        return Err(Error::server(
            0,
            format!(
                "解析直链失败，未获取 fileName：{}",
                snippet(&data.to_string(), 300)
            ),
        ));
    }
    Ok(info)
}

/// 解析种子结果，返回 `btResInfo`。
pub(crate) fn parse_torrent_resolve(data: &Value) -> Result<Value> {
    let info = data.get("btResInfo").cloned().unwrap_or(Value::Null);
    if info
        .get("infoHash")
        .and_then(Value::as_str)
        .map(|s| s.trim().is_empty())
        .unwrap_or(true)
    {
        return Err(Error::server(
            0,
            format!(
                "解析种子失败，未获取 infoHash：{}",
                snippet(&data.to_string(), 300)
            ),
        ));
    }
    Ok(info)
}

/// magnet 链接：只带 btih（与 Python 一致，无 dn/tr）。
pub(crate) fn magnet_for(info_hash: &str) -> String {
    format!("magnet:?xt=urn:btih:{}", info_hash.trim())
}

/// 云任务状态中文。
pub(crate) fn cloud_status_label(status: i64) -> &'static str {
    match status {
        0 => "等待中",
        1 => "下载中",
        2 => "已完成",
        3 => "失败",
        4 => "已暂停",
        5 => "已删除",
        _ => "未知状态",
    }
}

fn task_id_of(value: &Value) -> Result<String> {
    // taskId 可能是大整数（JSON number），统一转字符串。
    first_str(data_or_self(value), &["taskId", "task_id"]).ok_or_else(|| {
        Error::server(
            0,
            format!("创建云任务失败：{}", snippet(&value.to_string(), 300)),
        )
    })
}

/// 便捷：当前进行中的任务列表（内部调试/未来 GUI）。
#[allow(dead_code)]
pub(crate) async fn list_in_progress(drive: &GuangyaDrive) -> Result<Vec<Value>> {
    let data = drive
        .cloud_list_tasks(None, Some(&STATUS_IN_PROGRESS), 100, "")
        .await?;
    Ok(data
        .get("list")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnet_and_status() {
        assert_eq!(magnet_for("ABCDEF123"), "magnet:?xt=urn:btih:ABCDEF123");
        assert_eq!(magnet_for("  abc  "), "magnet:?xt=urn:btih:abc");
        assert_eq!(cloud_status_label(0), "等待中");
        assert_eq!(cloud_status_label(2), "已完成");
        assert_eq!(cloud_status_label(3), "失败");
        assert_eq!(cloud_status_label(5), "已删除");
        assert_eq!(cloud_status_label(99), "未知状态");
        assert_eq!(DEFAULT_CLOUD_DIR, "来自：云添加");
    }

    #[test]
    fn resolve_payloads() {
        let data = json!({
            "urlResInfo": {"url":"https://x/a.mp4","fileName":"a.mp4","fileSize":1024}
        });
        let info = parse_url_resolve(&data).unwrap();
        assert_eq!(info["fileName"], "a.mp4");
        assert_eq!(info["fileSize"], 1024);

        let flat = json!({"url":"https://x/b","fileName":"b.iso","fileSize":"5"});
        assert_eq!(parse_url_resolve(&flat).unwrap()["fileName"], "b.iso");
        assert!(parse_url_resolve(&json!({"urlResInfo":{}})).is_err());

        let torrent = json!({"btResInfo":{"infoHash":"HASH","fileName":"x.mkv","fileSize":9}});
        assert_eq!(parse_torrent_resolve(&torrent).unwrap()["infoHash"], "HASH");
        assert!(parse_torrent_resolve(&json!({"btResInfo":{}})).is_err());
    }

    #[test]
    fn classify_cloud_add_url() {
        match classify_cloud_url("  https://example.com/a.mp4?token=1  ").unwrap() {
            CloudSource::Url { url } => assert_eq!(url, "https://example.com/a.mp4?token=1"),
            other => panic!("直链被分类成 {other:?}"),
        }
        // 32 位 base32 与 40 位十六进制 btih 都合法；dn 作为 newName。
        match classify_cloud_url(
            "magnet:?xt=urn:btih:ABCDEF0123456789ABCDEF0123456789ABCDEF01&dn=Movie.mkv",
        )
        .unwrap()
        {
            CloudSource::Magnet {
                info_hash, name, ..
            } => {
                assert_eq!(info_hash, "ABCDEF0123456789ABCDEF0123456789ABCDEF01");
                assert_eq!(name.as_deref(), Some("Movie.mkv"));
            }
            other => panic!("magnet 被分类成 {other:?}"),
        }
        let base32 = classify_cloud_url("magnet:?xt=urn:btih:ABCDEFGHIJKLMNOPQRSTUVWXYZ234567");
        assert!(matches!(base32, Ok(CloudSource::Magnet { .. })));

        for bad in [
            "",
            "   ",
            "ftp://example.com/a",
            "magnet:?dn=no-hash",
            "magnet:?xt=urn:btih:short",
            "magnet:?xt=urn:sha1:ABCDEF",
            "not-a-url",
            "https://",
        ] {
            assert!(classify_cloud_url(bad).is_err(), "应拒绝：{bad}");
        }
    }

    #[test]
    fn cloud_task_mapping_status_and_progress() {
        let running = json!({
            "taskId": 123,
            "fileName": "a.mkv",
            "url": "https://x/a.mkv",
            "status": 1,
            "fileSize": 1000,
            "finishedSize": 250,
            "createTime": 1700000000123i64,
            "savePath": "/电影",
            "errorMsg": ""
        });
        let t = cloud_task_from_value(&running).unwrap();
        assert_eq!(t.id, "123", "数字 taskId 转字符串");
        assert_eq!(t.name, "a.mkv");
        assert_eq!(t.status, "1");
        assert_eq!(t.status_text, "下载中");
        assert_eq!(t.total, 1000);
        assert_eq!(t.done, 250);
        assert_eq!(t.progress, 0.25);
        assert_eq!(t.created, Some(1_700_000_000), "毫秒要转秒");
        assert_eq!(t.save_path, "/电影");

        // 完成态强制 100%，即使服务端 done 字段没跟上。
        let done = json!({"taskId":"t2","status":2,"fileName":"b","finishedSize":1});
        let t2 = cloud_task_from_value(&done).unwrap();
        assert_eq!(t2.status_text, "已完成");
        assert_eq!(t2.progress, 1.0);

        // 没有大小时回退服务端百分比（0–100），字符串也认。
        let percent = json!({"taskId":"t3","status":1,"progress":"45"});
        let t3 = cloud_task_from_value(&percent).unwrap();
        assert!((t3.progress - 0.45).abs() < 1e-6);
        assert_eq!(t3.total, 0);

        // totalSize 是源码 UI 用的字段名。
        let legacy = json!({"taskId":"t4","status":0,"totalSize":200,"finishedSize":50});
        assert_eq!(cloud_task_from_value(&legacy).unwrap().progress, 0.25);

        assert!(cloud_task_from_value(&json!({"fileName":"no-id"})).is_none());
        assert_eq!(
            progress_fraction(0, 0, Some(3), Some(0.0)),
            0.0,
            "失败任务不因 percent 缺失而变成 100%"
        );
    }

    #[test]
    fn cloud_tasks_from_list_data() {
        let data = json!({
            "statusCounts": {"0":1,"2":1},
            "list": [
                {"taskId":"1","status":0,"fileName":"waiting"},
                {"noTaskId":true,"status":1},
                {"taskId":"2","status":5,"fileName":"deleted"}
            ]
        });
        let list = cloud_tasks_from_data(&data);
        assert_eq!(list.len(), 2, "缺 taskId 的脏数据丢弃");
        assert_eq!(list[0].status_text, "等待中");
        assert_eq!(list[1].status_text, "已删除");
        assert!(cloud_tasks_from_data(&json!({"list": null})).is_empty());
    }

    #[test]
    fn task_id_accepts_number_and_string() {
        assert_eq!(
            task_id_of(&json!({"data":{"taskId":1946179562994634836i64}})).unwrap(),
            "1946179562994634836",
            "大整数 taskId 不能丢精度"
        );
        assert_eq!(task_id_of(&json!({"taskId":"abc"})).unwrap(), "abc");
        assert!(task_id_of(&json!({"data":{}})).is_err());
        assert!(task_id_of(&json!({"taskId":"  "})).is_err());
    }
}
