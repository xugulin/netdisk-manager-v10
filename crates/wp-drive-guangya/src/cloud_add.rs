//! 云添加（离线下载）：直链解析、种子解析、默认目录、任务创建/查询/等待。
//!
//! ⚠️ `Drive` trait 目前没有云添加方法（GUI 的传输页也未接），所以这里是
//! **内部能力**，保留完整协议实现供后续在 GUI 扩展；模块整体 `allow(dead_code)`。
//!
//! 协议（§4.3/§7.1）：
//! * 直链：`resolve_res` → `create_dir("来自：云添加")` → `create_task`;
//! * BT：multipart `resolve_torrent`（字段 `torrent`）→ 默认目录 → magnet `create_task`;
//! * 任务列表：`list_task`，状态 0 等待 / 1 下载 / 2 完成 / 3 失败 / 4 暂停 / 5 删除。

use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};
use wp_core::model::ProgressFn;
use wp_core::{Error, Result};

use crate::{as_i64, data_or_self, snippet, GuangyaDrive, API_BASE};

/// 默认云添加目录（全角冒号，不能改）。
pub(crate) const DEFAULT_CLOUD_DIR: &str = "来自：云添加";
const STATUS_IN_PROGRESS: [i64; 4] = [0, 1, 3, 4];
const STATUS_ALL: [i64; 6] = [0, 1, 2, 3, 4, 5];

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
        data_or_self(&value)
            .get("fileId")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .ok_or_else(|| {
                Error::server(
                    0,
                    format!("确保默认目录失败：{}", snippet(&value.to_string(), 300)),
                )
            })
    }

    /// 创建直链任务，返回 taskId。
    pub(crate) async fn cloud_create_url_task(&self, url: &str, parent_id: &str) -> Result<String> {
        let value = self
            .api_post(
                "/cloudcollection/v1/create_task",
                json!({ "url": url, "parentId": parent_id }),
                &[],
            )
            .await?;
        task_id_of(&value)
    }

    /// 创建 BT 任务（magnet 只用 btih，与 Python 一致），返回 taskId。
    pub(crate) async fn cloud_create_bt_task(
        &self,
        info_hash: &str,
        parent_id: &str,
        new_name: &str,
    ) -> Result<String> {
        let magnet = magnet_for(info_hash);
        let mut body = json!({ "url": magnet, "parentId": parent_id });
        if !new_name.trim().is_empty() {
            body["newName"] = json!(new_name.trim());
        }
        let value = self
            .api_post("/cloudcollection/v1/create_task", body, &[])
            .await?;
        task_id_of(&value)
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
    data_or_self(value)
        .get("taskId")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
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
}
