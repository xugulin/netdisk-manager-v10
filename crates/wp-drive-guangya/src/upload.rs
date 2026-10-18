//! 上传：MD5 → 取上传令牌 → OSS V1 直传（单 PUT / multipart）→ 业务确认。
//!
//! 协议要点（文档 §5）：
//! * 取令牌 `POST /userres/v2/get_res_center_token`；`code=156`/`msg=上传已完成` 即秒传，
//!   直接轮询 `get_info_by_task_id`；
//! * OSS 用 **V1 签名（STS）**：`x-oss-date` 填 StringToSign 的 Date 行，只发
//!   `x-oss-date`/`x-oss-security-token`（不发 `x-oss-user-agent`，减少签名变量）；
//! * 小文件一次 PUT；大文件 multipart（Initiate → UploadPart 并发 → Complete →
//!   `/userres/v1/multipart/complete`），失败时尽量 Abort 清理；
//! * 最后轮询 `get_info_by_task_id`（147=上传中，msg=success+data=完成）。

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use wp_core::model::{ProgressFn, RemoteEntry, UploadOptions};
use wp_core::util::{join_path, norm_path, sanitize_file_name};
use wp_core::{Error, Result};

use crate::sign::{self, OssSignRequest, OssSubResource};
use crate::{as_i64, data_or_self, files, snippet, GuangyaDrive};

/// 超过该大小的文件不再一次性读入内存。
const SINGLE_PUT_LIMIT: u64 = 32 * 1024 * 1024;
/// 默认分片 16 MiB（oss2 原实现 1 MiB 偏小；OSS 最小 100 KiB）。
const DEFAULT_PART_SIZE: u64 = 16 * 1024 * 1024;
/// 用户自定义分片的上限，避免一次读入过多内存。
const MAX_PART_SIZE: u64 = 32 * 1024 * 1024;
/// OSS 最多 10000 片。
const MAX_PARTS: u64 = 10_000;
/// 分片上传默认并发。
const DEFAULT_CONCURRENCY: usize = 4;
/// 轮询上传任务：1s / 300s。
const POLL_INTERVAL_SECS: u64 = 1;
const POLL_TIMEOUT_SECS: u64 = 300;

/// STS 临时凭证。
#[derive(Debug, Clone, Default)]
struct OssCreds {
    access_key_id: String,
    secret_access_key: String,
    session_token: String,
}

/// `get_res_center_token` 返回的上传令牌。
#[derive(Debug, Clone, Default)]
struct UploadToken {
    task_id: String,
    gcid: String,
    bucket_name: String,
    object_path: String,
    full_end_point: String,
    end_point: String,
    creds: Option<OssCreds>,
}

impl UploadToken {
    fn from_data(data: &Value) -> Self {
        let creds = data
            .get("creds")
            .filter(|c| c.is_object())
            .map(|c| OssCreds {
                access_key_id: str_field(c, &["accessKeyID", "accessKeyId", "access_key_id"]),
                secret_access_key: str_field(c, &["secretAccessKey", "secret_access_key"]),
                session_token: str_field(c, &["sessionToken", "securityToken", "session_token"]),
            });
        Self {
            task_id: str_field(data, &["taskId", "task_id"]),
            gcid: str_field(data, &["gcid"]),
            bucket_name: str_field(data, &["bucketName", "bucket_name"]),
            object_path: str_field(data, &["objectPath", "object_path"]),
            full_end_point: str_field(data, &["fullEndPoint", "full_end_point"]),
            end_point: str_field(data, &["endPoint", "endpoint", "end_point"]),
            creds,
        }
    }

    /// OSS 基地址：优先 `fullEndPoint`，否则 `https://{bucket}.{endPoint}`。
    fn endpoint_base(&self) -> Result<String> {
        if !self.full_end_point.trim().is_empty() {
            let ep = self.full_end_point.trim().trim_end_matches('/');
            return Ok(if ep.starts_with("http://") || ep.starts_with("https://") {
                ep.to_string()
            } else {
                format!("https://{ep}")
            });
        }
        if self.end_point.trim().is_empty() {
            return Err(Error::server(0, "上传令牌缺少 OSS endPoint/fullEndPoint"));
        }
        if self.bucket_name.trim().is_empty() {
            return Err(Error::server(0, "上传令牌缺少 bucketName"));
        }
        let host = self
            .end_point
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/');
        if host.is_empty() {
            return Err(Error::server(0, "上传令牌的 endPoint 为空"));
        }
        // endPoint 通常只是 host；若已经带 bucket 前缀则不要重复拼接。
        let bucket = self.bucket_name.trim();
        if host.starts_with(&format!("{bucket}.")) {
            Ok(format!("https://{host}"))
        } else {
            Ok(format!("https://{bucket}.{host}"))
        }
    }
}

/// 上传入口。
pub(crate) async fn upload(
    drive: &GuangyaDrive,
    local: &Path,
    remote_dir: &str,
    opts: &UploadOptions,
    mut progress: ProgressFn,
) -> Result<RemoteEntry> {
    let meta = tokio::fs::metadata(local).await.map_err(Error::Io)?;
    if !meta.is_file() {
        return Err(Error::other(format!("不是文件：{}", local.display())));
    }
    let size = meta.len();

    let dir_path = norm_path(remote_dir);
    let dir_entry = match files::resolve_dir(drive, &dir_path).await {
        Ok(e) => e,
        Err(e) if e.kind() == wp_core::error::ErrorKind::NotFound => {
            files::ensure_dir(drive, &dir_path).await?
        }
        Err(e) => return Err(e),
    };
    let raw_name = opts
        .rename_to
        .clone()
        .or_else(|| local.file_name().map(|s| s.to_string_lossy().to_string()))
        .ok_or_else(|| Error::other("无法确定上传文件名"))?;
    let file_name = sanitize_file_name(&raw_name);
    if file_name.trim().is_empty() {
        return Err(Error::other("上传文件名不能为空"));
    }

    report_progress(&mut progress, 0, size)?;

    // 1) MD5（2 MiB 分块，进度 0→5%）
    let md5 = md5_file(local, size, &mut progress).await?;
    report_progress(&mut progress, pct(size, 50), size)?;

    // 2) 取上传令牌 / 秒传判断
    let token_body = json!({
        "capacity": 30,
        "name": file_name,
        "res": { "fileSize": size, "md5": md5 },
        "parentId": dir_entry.id,
    });
    let token_value = drive
        .api_post("/userres/v2/get_res_center_token", token_body, &[156])
        .await?;
    let code = token_value.get("code").and_then(as_i64).unwrap_or(0);
    let msg = token_value
        .get("msg")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let token_data = data_or_self(&token_value);
    let token = UploadToken::from_data(token_data);
    if token.task_id.trim().is_empty() {
        return Err(Error::server(
            code,
            format!(
                "取上传令牌响应缺少 taskId：{}",
                snippet(&token_value.to_string(), 300)
            ),
        ));
    }

    // 秒传命中：跳过 OSS，直接等文件信息。
    if code == 156 || msg == "上传已完成" {
        report_progress(&mut progress, pct(size, 900), size)?;
        let info = wait_task(drive, &token.task_id, size, &mut progress).await?;
        report_progress(&mut progress, size, size).ok();
        return Ok(entry_from_upload_info(&info, &dir_path, &file_name, size));
    }

    let creds = token
        .creds
        .clone()
        .ok_or_else(|| Error::server(code, "取上传令牌失败：响应缺少 creds（STS 临时凭证）"))?;
    if creds.access_key_id.is_empty()
        || creds.secret_access_key.is_empty()
        || creds.session_token.is_empty()
    {
        return Err(Error::server(code, "上传令牌的 STS 凭证不完整"));
    }
    let base = token.endpoint_base()?;
    let content_type = sign::content_type_for(&file_name);
    let (chunk_size, part_count) = part_plan(size, opts.chunk_size);
    let use_multipart = size > SINGLE_PUT_LIMIT || (part_count > 1);

    let mut task_id = token.task_id.clone();
    let etag = if use_multipart {
        let concurrency = if opts.concurrency == 0 {
            DEFAULT_CONCURRENCY
        } else {
            opts.concurrency.clamp(1, 8)
        };
        multipart_upload(
            drive,
            &token,
            &creds,
            &base,
            &content_type,
            local,
            size,
            chunk_size,
            part_count,
            concurrency,
            &mut progress,
            &mut task_id,
        )
        .await?
    } else {
        let etag = put_single(drive, &token, &creds, &base, &content_type, local, size).await?;
        report_progress(&mut progress, pct(size, 900), size)?;
        etag
    };

    // 3) 秒传检查（可选；失败只记日志，不影响上传；multipart 已由业务完成接口确认）
    let gcid = if etag.is_empty() {
        token.gcid.clone()
    } else {
        sign::strip_etag_quotes(&etag)
    };
    if !use_multipart {
        if let Ok(v) = drive
            .api_post(
                "/userres/v1/check_can_flash_upload",
                json!({ "taskId": task_id, "gcid": gcid, "cid": "" }),
                &[],
            )
            .await
        {
            if let Some(new_id) = data_or_self(&v).get("taskId").and_then(Value::as_str) {
                if !new_id.is_empty() {
                    task_id = new_id.to_string();
                }
            }
        } else if drive.verbose {
            wp_core::log_debug!("光鸭", "秒传检查失败（忽略）task={task_id}");
        }
    }

    // 4) 轮询文件信息
    let info = wait_task(drive, &task_id, size, &mut progress).await?;
    report_progress(&mut progress, size, size).ok();
    Ok(entry_from_upload_info(&info, &dir_path, &file_name, size))
}

// ==================== 单 PUT ====================

async fn put_single(
    drive: &GuangyaDrive,
    token: &UploadToken,
    creds: &OssCreds,
    base: &str,
    content_type: &str,
    local: &Path,
    size: u64,
) -> Result<String> {
    let data = tokio::fs::read(local).await.map_err(Error::Io)?;
    if data.len() as u64 != size {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("读取文件失败：期望 {size} 字节，实际 {}", data.len()),
        )));
    }
    let url = object_url(base, &token.object_path, &[])?;
    let date = wp_net::crypto::rfc1123_now();
    let mut oss_headers = BTreeMap::new();
    oss_headers.insert("x-oss-date".to_string(), date.clone());
    oss_headers.insert(
        "x-oss-security-token".to_string(),
        creds.session_token.clone(),
    );
    let auth = sign::oss_v1_authorization(&OssSignRequest {
        method: "PUT",
        content_md5: "",
        content_type,
        date: &date,
        oss_headers: &oss_headers,
        bucket: &token.bucket_name,
        object_path: &token.object_path,
        sub_resource: OssSubResource::None,
        access_key_id: &creds.access_key_id,
        access_key_secret: &creds.secret_access_key,
    });

    for attempt in 0..3u32 {
        let resp = drive
            .xfer
            .put(&url)
            .header("x-oss-date", &date)
            .header("x-oss-security-token", &creds.session_token)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .header(reqwest::header::AUTHORIZATION, &auth)
            .body(data.clone())
            .send()
            .await;
        match resp {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let etag = header_string(&resp, "etag").unwrap_or_default();
                let request_id = header_string(&resp, "x-oss-request-id");
                let text = if status >= 400 {
                    resp.text().await.unwrap_or_default()
                } else {
                    String::new()
                };
                if status >= 400 {
                    let err = classify_oss_error(status, &text, request_id.as_deref());
                    if attempt < 2 && transient_status(status) {
                        retry_sleep(attempt).await;
                        continue;
                    }
                    return Err(err);
                }
                return Ok(etag);
            }
            Err(e) => {
                let err = wp_net::client::net_err(e);
                if attempt < 2 {
                    retry_sleep(attempt).await;
                    continue;
                }
                return Err(err);
            }
        }
    }
    Err(Error::network("OSS 直传重试耗尽"))
}

// ==================== multipart ====================

struct PartResult {
    number: u32,
    etag: String,
    len: u64,
}

#[allow(clippy::too_many_arguments)]
async fn multipart_upload(
    drive: &GuangyaDrive,
    token: &UploadToken,
    creds: &OssCreds,
    base: &str,
    content_type: &str,
    local: &Path,
    size: u64,
    chunk_size: u64,
    part_count: u32,
    concurrency: usize,
    progress: &mut ProgressFn,
    task_id: &mut String,
) -> Result<String> {
    let date = wp_net::crypto::rfc1123_now();
    let upload_id = initiate_multipart(drive, token, creds, base, content_type, &date).await?;
    if drive.verbose {
        wp_core::log_debug!(
            "光鸭",
            "分片上传开始 object={} size={size} parts={part_count} chunk={chunk_size}",
            token.object_path
        );
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let sem = Arc::new(Semaphore::new(concurrency));
    let mut set: JoinSet<Result<PartResult>> = JoinSet::new();
    for number in 1..=part_count {
        let offset = (number as u64 - 1) * chunk_size;
        let len = chunk_size.min(size.saturating_sub(offset));
        let local: PathBuf = local.to_path_buf();
        let base = base.to_string();
        let object_path = token.object_path.clone();
        let bucket = token.bucket_name.clone();
        let creds = creds.clone();
        let upload_id = upload_id.clone();
        let cancel = cancel.clone();
        let sem = sem.clone();
        let client = drive.xfer.clone();
        set.spawn(async move {
            let _permit = sem
                .acquire_owned()
                .await
                .map_err(|e| Error::other(format!("分片并发控制失败：{e}")))?;
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Canceled("上传已取消".into()));
            }
            upload_one_part(
                &client,
                &base,
                &object_path,
                &bucket,
                &creds,
                &upload_id,
                &local,
                number as u32,
                offset,
                len,
                &cancel,
            )
            .await
        });
    }

    let mut parts: Vec<PartResult> = Vec::new();
    let mut uploaded: u64 = 0;
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok(part)) => {
                uploaded += part.len;
                parts.push(part);
                let done = oss_done(size, uploaded);
                if let Err(e) = report_progress(progress, done, size) {
                    cancel.store(true, Ordering::Relaxed);
                    set.abort_all();
                    while set.join_next().await.is_some() {}
                    abort_multipart(drive, token, creds, base, &upload_id).await;
                    return Err(e);
                }
            }
            Ok(Err(e)) => {
                cancel.store(true, Ordering::Relaxed);
                set.abort_all();
                while set.join_next().await.is_some() {}
                abort_multipart(drive, token, creds, base, &upload_id).await;
                return Err(e);
            }
            Err(e) => {
                cancel.store(true, Ordering::Relaxed);
                set.abort_all();
                while set.join_next().await.is_some() {}
                abort_multipart(drive, token, creds, base, &upload_id).await;
                return Err(Error::other(format!("分片任务异常：{e}")));
            }
        }
    }
    if cancel.load(Ordering::Relaxed) {
        abort_multipart(drive, token, creds, base, &upload_id).await;
        return Err(Error::Canceled("上传已取消".into()));
    }
    parts.sort_by_key(|p| p.number);
    if parts.len() as u32 != part_count {
        abort_multipart(drive, token, creds, base, &upload_id).await;
        return Err(Error::other(format!(
            "分片数量不一致：期望 {part_count}，实际 {}",
            parts.len()
        )));
    }

    match complete_multipart(
        drive,
        token,
        creds,
        base,
        &upload_id,
        &wp_net::crypto::rfc1123_now(),
        &parts,
    )
    .await
    {
        Ok(etag) => {
            // 通知光鸭业务服务端：taskId/gcid/parts（etag 原样带引号）。
            let parts_json: Vec<Value> = parts
                .iter()
                .map(|p| json!({ "number": p.number, "etag": p.etag }))
                .collect();
            let notify = json!({
                "taskId": task_id,
                "gcid": token.gcid,
                "parts": parts_json,
            });
            if let Err(e) = drive
                .api_post("/userres/v1/multipart/complete", notify, &[])
                .await
            {
                abort_multipart(drive, token, creds, base, &upload_id).await;
                return Err(e);
            }
            report_progress(progress, pct(size, 950), size)?;
            Ok(etag)
        }
        Err(e) => {
            abort_multipart(drive, token, creds, base, &upload_id).await;
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn upload_one_part(
    client: &reqwest::Client,
    base: &str,
    object_path: &str,
    bucket: &str,
    creds: &OssCreds,
    upload_id: &str,
    local: &Path,
    number: u32,
    offset: u64,
    len: u64,
    cancel: &AtomicBool,
) -> Result<PartResult> {
    let mut file = tokio::fs::File::open(local).await.map_err(Error::Io)?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(Error::Io)?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf).await.map_err(Error::Io)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Canceled("上传已取消".into()));
    }
    let url = object_url(
        base,
        object_path,
        &[
            ("partNumber", number.to_string()),
            ("uploadId", upload_id.to_string()),
        ],
    )?;
    // 分片只重试网络错误/5xx/429；每次尝试重新生成日期并签名（长传不会超 15 分钟窗口）。
    for attempt in 0..3u32 {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Canceled("上传已取消".into()));
        }
        let date = wp_net::crypto::rfc1123_now();
        let mut oss_headers = BTreeMap::new();
        oss_headers.insert("x-oss-date".to_string(), date.clone());
        oss_headers.insert(
            "x-oss-security-token".to_string(),
            creds.session_token.clone(),
        );
        let auth = sign::oss_v1_authorization(&OssSignRequest {
            method: "PUT",
            content_md5: "",
            content_type: "application/octet-stream",
            date: &date,
            oss_headers: &oss_headers,
            bucket,
            object_path,
            sub_resource: OssSubResource::Part { number, upload_id },
            access_key_id: &creds.access_key_id,
            access_key_secret: &creds.secret_access_key,
        });
        let resp = client
            .put(&url)
            .header("x-oss-date", &date)
            .header("x-oss-security-token", &creds.session_token)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header(reqwest::header::AUTHORIZATION, &auth)
            .body(buf.clone())
            .send()
            .await;
        match resp {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let etag = header_string(&resp, "etag").unwrap_or_default();
                let request_id = header_string(&resp, "x-oss-request-id");
                let text = if status >= 400 {
                    resp.text().await.unwrap_or_default()
                } else {
                    String::new()
                };
                if status >= 400 {
                    if attempt < 2 && transient_status(status) {
                        retry_sleep(attempt).await;
                        continue;
                    }
                    return Err(classify_oss_error(status, &text, request_id.as_deref()));
                }
                if etag.is_empty() {
                    return Err(Error::server(0, format!("分片 {number} 响应缺少 ETag")));
                }
                return Ok(PartResult { number, etag, len });
            }
            Err(e) => {
                if attempt < 2 {
                    retry_sleep(attempt).await;
                    continue;
                }
                return Err(wp_net::client::net_err(e));
            }
        }
    }
    Err(Error::network(format!("分片 {number} 重试耗尽")))
}

async fn initiate_multipart(
    drive: &GuangyaDrive,
    token: &UploadToken,
    creds: &OssCreds,
    base: &str,
    content_type: &str,
    date: &str,
) -> Result<String> {
    let url = format!("{}?uploads", object_url(base, &token.object_path, &[])?);
    let mut oss_headers = BTreeMap::new();
    oss_headers.insert("x-oss-date".to_string(), date.to_string());
    oss_headers.insert(
        "x-oss-security-token".to_string(),
        creds.session_token.clone(),
    );
    let auth = sign::oss_v1_authorization(&OssSignRequest {
        method: "POST",
        content_md5: "",
        content_type,
        date,
        oss_headers: &oss_headers,
        bucket: &token.bucket_name,
        object_path: &token.object_path,
        sub_resource: OssSubResource::Uploads,
        access_key_id: &creds.access_key_id,
        access_key_secret: &creds.secret_access_key,
    });
    let resp = drive
        .xfer
        .post(&url)
        .header("x-oss-date", date)
        .header("x-oss-security-token", &creds.session_token)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .header(reqwest::header::AUTHORIZATION, &auth)
        .send()
        .await
        .map_err(wp_net::client::net_err)?;
    let status = resp.status().as_u16();
    let request_id = header_string(&resp, "x-oss-request-id");
    let text = resp.text().await.unwrap_or_default();
    if status >= 400 {
        return Err(classify_oss_error(status, &text, request_id.as_deref()));
    }
    if let Some((code, msg)) = xml_error(&text) {
        return Err(Error::server(
            0,
            format!("InitiateMultipartUpload 失败：{code} {msg}"),
        ));
    }
    xml_tag(&text, "UploadId").ok_or_else(|| {
        Error::server(
            0,
            format!(
                "InitiateMultipartUpload 响应缺少 UploadId：{}",
                snippet(&text, 300)
            ),
        )
    })
}

async fn complete_multipart(
    drive: &GuangyaDrive,
    token: &UploadToken,
    creds: &OssCreds,
    base: &str,
    upload_id: &str,
    date: &str,
    parts: &[PartResult],
) -> Result<String> {
    let url = object_url(
        base,
        &token.object_path,
        &[("uploadId", upload_id.to_string())],
    )?;
    let xml = complete_xml(parts);
    let mut oss_headers = BTreeMap::new();
    oss_headers.insert("x-oss-date".to_string(), date.to_string());
    oss_headers.insert(
        "x-oss-security-token".to_string(),
        creds.session_token.clone(),
    );
    let auth = sign::oss_v1_authorization(&OssSignRequest {
        method: "POST",
        content_md5: "",
        content_type: "application/xml",
        date,
        oss_headers: &oss_headers,
        bucket: &token.bucket_name,
        object_path: &token.object_path,
        sub_resource: OssSubResource::UploadId { upload_id },
        access_key_id: &creds.access_key_id,
        access_key_secret: &creds.secret_access_key,
    });
    let resp = drive
        .xfer
        .post(&url)
        .header("x-oss-date", date)
        .header("x-oss-security-token", &creds.session_token)
        .header(reqwest::header::CONTENT_TYPE, "application/xml")
        .header(reqwest::header::AUTHORIZATION, &auth)
        .body(xml)
        .send()
        .await
        .map_err(wp_net::client::net_err)?;
    let status = resp.status().as_u16();
    let request_id = header_string(&resp, "x-oss-request-id");
    let text = resp.text().await.unwrap_or_default();
    if status >= 400 {
        return Err(classify_oss_error(status, &text, request_id.as_deref()));
    }
    // OSS 可能 200 + <Error>（文档 §5.7.3 第 8 条）。
    if let Some((code, msg)) = xml_error(&text) {
        return Err(Error::server(
            0,
            format!("CompleteMultipartUpload 失败：{code} {msg}"),
        ));
    }
    Ok(xml_tag(&text, "ETag").unwrap_or_default())
}

/// best-effort 清理未完成分片（Python 没有做；失败忽略）。
async fn abort_multipart(
    drive: &GuangyaDrive,
    token: &UploadToken,
    creds: &OssCreds,
    base: &str,
    upload_id: &str,
) {
    let Ok(url) = object_url(
        base,
        &token.object_path,
        &[("uploadId", upload_id.to_string())],
    ) else {
        return;
    };
    // 清理也重新生成日期，避免长传超过 OSS 的 15 分钟时钟窗口。
    let date = wp_net::crypto::rfc1123_now();
    let mut oss_headers = BTreeMap::new();
    oss_headers.insert("x-oss-date".to_string(), date.to_string());
    oss_headers.insert(
        "x-oss-security-token".to_string(),
        creds.session_token.clone(),
    );
    let auth = sign::oss_v1_authorization(&OssSignRequest {
        method: "DELETE",
        content_md5: "",
        content_type: "",
        date: &date,
        oss_headers: &oss_headers,
        bucket: &token.bucket_name,
        object_path: &token.object_path,
        sub_resource: OssSubResource::UploadId { upload_id },
        access_key_id: &creds.access_key_id,
        access_key_secret: &creds.secret_access_key,
    });
    let _ = drive
        .xfer
        .delete(&url)
        .header("x-oss-date", &date)
        .header("x-oss-security-token", &creds.session_token)
        .header(reqwest::header::AUTHORIZATION, &auth)
        .send()
        .await;
}

// ==================== 任务轮询 ====================

async fn wait_task(
    drive: &GuangyaDrive,
    task_id: &str,
    total: u64,
    progress: &mut ProgressFn,
) -> Result<Value> {
    let started = Instant::now();
    report_progress(progress, pct(total, 950), total)?;
    loop {
        let value = drive
            .api_post(
                "/userres/v1/file/get_info_by_task_id",
                json!({ "taskId": task_id }),
                &[],
            )
            .await?;
        let msg = value.get("msg").and_then(Value::as_str).unwrap_or("");
        if msg == "success" {
            if let Some(data) = value.get("data").filter(|d| !d.is_null()) {
                return Ok(data.clone());
            }
        }
        let code = value.get("code").and_then(as_i64).unwrap_or(0);
        if code == 147 {
            if started.elapsed() >= Duration::from_secs(POLL_TIMEOUT_SECS) {
                return Err(Error::server(
                    147,
                    format!("上传任务 {task_id} 等待超时（{POLL_TIMEOUT_SECS}s）"),
                ));
            }
            // 轮询期间也响应取消（同一进度值重复上报，回调仍可返回 false）。
            report_progress(progress, pct(total, 950), total)?;
            tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
            continue;
        }
        return Err(Error::server(
            code,
            format!(
                "上传任务 {task_id} 异常：msg={msg}；{}",
                snippet(&value.to_string(), 300)
            ),
        ));
    }
}

/// 上传任务 data → 统一条目。
pub(crate) fn entry_from_upload_info(
    info: &Value,
    dir_path: &str,
    fallback_name: &str,
    fallback_size: u64,
) -> RemoteEntry {
    let mut entry = files::entry_from_value(info, dir_path).unwrap_or_else(|| {
        RemoteEntry::file(
            fallback_name,
            join_path(dir_path, fallback_name),
            fallback_size,
        )
    });
    if entry.name.trim().is_empty() {
        entry.name = fallback_name.to_string();
        entry.path = join_path(dir_path, fallback_name);
    }
    entry.is_dir = false;
    if entry.size == 0 && fallback_size > 0 {
        entry.size = fallback_size;
    }
    entry
}

// ==================== 纯逻辑工具 ====================

fn str_field(v: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string()
}

fn pct(size: u64, permille: u64) -> u64 {
    ((size as u128) * permille as u128 / 1000) as u64
}

fn oss_done(size: u64, uploaded: u64) -> u64 {
    if size == 0 {
        return 0;
    }
    let uploaded = uploaded.min(size);
    ((size as u128 * 50 + size as u128 * 900 * uploaded as u128 / size as u128) / 1000) as u64
}

fn report_progress(progress: &mut ProgressFn, done: u64, total: u64) -> Result<()> {
    if !progress(done, total) {
        return Err(Error::Canceled("用户取消上传".into()));
    }
    Ok(())
}

/// 计算分片大小与数量：默认 16 MiB、最少 1 MiB、最多 10000 片。
fn part_plan(size: u64, requested: u64) -> (u64, u32) {
    let mut chunk = if requested == 0 {
        DEFAULT_PART_SIZE
    } else {
        requested.clamp(1024 * 1024, MAX_PART_SIZE)
    };
    if size == 0 {
        return (chunk, 0);
    }
    let mut count = size.div_ceil(chunk);
    if count > MAX_PARTS {
        chunk = size.div_ceil(MAX_PARTS).max(1024 * 1024);
        count = size.div_ceil(chunk);
    }
    (chunk, count as u32)
}

fn complete_xml(parts: &[PartResult]) -> String {
    let mut out = String::from("<CompleteMultipartUpload>");
    for p in parts {
        out.push_str(&format!(
            "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
            p.number, p.etag
        ));
    }
    out.push_str("</CompleteMultipartUpload>");
    out
}

fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let rest = &xml[start..];
    let end = rest.find(&close)?;
    Some(rest[..end].trim().to_string())
}

fn xml_error(xml: &str) -> Option<(String, String)> {
    if !xml.contains("<Error>") {
        return None;
    }
    Some((
        xml_tag(xml, "Code").unwrap_or_else(|| "Unknown".into()),
        xml_tag(xml, "Message").unwrap_or_default(),
    ))
}

fn object_url(base: &str, object_path: &str, query: &[(&str, String)]) -> Result<String> {
    let mut url =
        url::Url::parse(base).map_err(|e| Error::other(format!("OSS 地址非法（{base}）：{e}")))?;
    {
        let mut segs = url
            .path_segments_mut()
            .map_err(|_| Error::other(format!("OSS 地址不能作为 base：{base}")))?;
        segs.pop_if_empty();
        for part in object_path.trim_start_matches('/').split('/') {
            if !part.is_empty() {
                segs.push(part);
            }
        }
    }
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (k, v) in query {
            pairs.append_pair(k, v);
        }
    }
    Ok(url.to_string())
}

fn transient_status(status: u16) -> bool {
    status == 429 || status >= 500
}

async fn retry_sleep(attempt: u32) {
    let ms = 300u64.saturating_mul(1u64 << attempt.min(4));
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

fn classify_oss_error(status: u16, body: &str, request_id: Option<&str>) -> Error {
    let (code, message) =
        xml_error(body).unwrap_or_else(|| ("OSS".to_string(), snippet(body, 240)));
    let rid = request_id
        .map(|r| format!("（x-oss-request-id: {r}）"))
        .unwrap_or_default();
    let text = format!("OSS HTTP {status}：{code} {message}{rid}");
    match status {
        404 => Error::not_found(text),
        _ => Error::server(status as i64, text),
    }
}

fn header_string(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

async fn md5_file(path: &Path, size: u64, progress: &mut ProgressFn) -> Result<String> {
    use md5::Digest;
    let mut file = tokio::fs::File::open(path).await.map_err(Error::Io)?;
    let mut hasher = md5::Md5::new();
    let mut buf = vec![0u8; 2 * 1024 * 1024];
    let mut read = 0u64;
    loop {
        let n = file.read(&mut buf).await.map_err(Error::Io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        read += n as u64;
        if size > 0 {
            let done = pct(size, 50u64.saturating_mul(read) / size);
            report_progress(progress, done, size)?;
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_token_parsing() {
        let data = json!({
            "taskId":"t-1","gcid":"g-1","bucketName":"gy-bucket",
            "objectPath":"/up/a b.bin","endPoint":"oss-cn-hangzhou.aliyuncs.com",
            "fullEndPoint":"https://gy-bucket.oss-cn-hangzhou.aliyuncs.com",
            "creds":{"accessKeyID":"AK","secretAccessKey":"SK","sessionToken":"STS"}
        });
        let t = UploadToken::from_data(&data);
        assert_eq!(t.task_id, "t-1");
        assert_eq!(t.bucket_name, "gy-bucket");
        let c = t.creds.clone().unwrap();
        assert_eq!(c.access_key_id, "AK");
        assert_eq!(c.session_token, "STS");
        assert_eq!(
            t.endpoint_base().unwrap(),
            "https://gy-bucket.oss-cn-hangzhou.aliyuncs.com"
        );

        let t2 = UploadToken {
            bucket_name: "b".into(),
            end_point: "https://oss.example.com/".into(),
            ..Default::default()
        };
        assert_eq!(t2.endpoint_base().unwrap(), "https://b.oss.example.com");
        let t3 = UploadToken {
            bucket_name: "b".into(),
            end_point: "https://b.oss.example.com".into(),
            ..Default::default()
        };
        assert_eq!(t3.endpoint_base().unwrap(), "https://b.oss.example.com");
        assert!(UploadToken::default().endpoint_base().is_err());
    }

    #[test]
    fn part_plan_sizes() {
        assert_eq!(part_plan(0, 0), (DEFAULT_PART_SIZE, 0));
        assert_eq!(part_plan(10 * 1024 * 1024, 0).1, 1);
        let (chunk, count) = part_plan(100 * 1024 * 1024, 0);
        assert_eq!(chunk, DEFAULT_PART_SIZE);
        assert_eq!(count, 7);
        // 请求过小的分片会被抬到 1 MiB，过大的分片会被压到 32 MiB
        let (chunk, _) = part_plan(5 * 1024 * 1024, 1024);
        assert_eq!(chunk, 1024 * 1024);
        let (chunk, count) = part_plan(100 * 1024 * 1024, 100 * 1024 * 1024);
        assert_eq!(chunk, MAX_PART_SIZE);
        assert_eq!(count, 4);
        // 超大文件强制压到 10000 片以内
        let (chunk, count) = part_plan(200 * 1024 * 1024 * 1024u64, 0);
        assert!(count <= 10000);
        assert!(chunk >= 1024 * 1024);
    }

    #[test]
    fn object_url_encoding_and_queries() {
        let base = "https://bucket.oss-cn-hangzhou.aliyuncs.com";
        let url = object_url(base, "/目录/a b.bin", &[]).unwrap();
        assert!(url.starts_with("https://bucket.oss-cn-hangzhou.aliyuncs.com/"));
        assert!(url.contains('%'), "中文/空格应被 URL 编码：{url}");
        assert!(!url.contains("/up//"));
        let part = object_url(
            base,
            "up/a.bin",
            &[("partNumber", "2".into()), ("uploadId", "abc+/=".into())],
        )
        .unwrap();
        assert!(part.contains("partNumber=2"));
        assert!(part.contains("uploadId=abc%2B%2F%3D"));
    }

    #[test]
    fn complete_xml_and_parsers() {
        let parts = vec![
            PartResult {
                number: 1,
                etag: "\"e1\"".into(),
                len: 10,
            },
            PartResult {
                number: 2,
                etag: "\"e2\"".into(),
                len: 5,
            },
        ];
        let xml = complete_xml(&parts);
        assert!(xml.starts_with("<CompleteMultipartUpload>"));
        assert!(xml.contains("<PartNumber>1</PartNumber><ETag>\"e1\"</ETag>"));
        assert!(xml.ends_with("</CompleteMultipartUpload>"));

        let init = "<InitiateMultipartUploadResult><UploadId>U-123</UploadId></InitiateMultipartUploadResult>";
        assert_eq!(xml_tag(init, "UploadId").as_deref(), Some("U-123"));
        assert_eq!(xml_error(init), None);
        let err = "<Error><Code>AccessDenied</Code><Message>denied</Message></Error>";
        assert_eq!(
            xml_error(err),
            Some(("AccessDenied".to_string(), "denied".to_string()))
        );
        assert_eq!(xml_tag("<a><b> </b></a>", "b").as_deref(), Some(""));
    }

    #[test]
    fn progress_mapping() {
        assert_eq!(pct(1000, 50), 50);
        assert_eq!(oss_done(1000, 0), 50);
        assert_eq!(oss_done(1000, 1000), 950);
        assert_eq!(oss_done(0, 0), 0);

        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(u64, u64)>::new()));
        let sink = calls.clone();
        let mut cb: ProgressFn = Box::new(move |d, t| {
            sink.lock().unwrap().push((d, t));
            true
        });
        report_progress(&mut cb, 10, 100).unwrap();
        assert_eq!(*calls.lock().unwrap(), vec![(10, 100)]);

        let mut cancel: ProgressFn = Box::new(|_, _| false);
        let e = report_progress(&mut cancel, 1, 2).unwrap_err();
        assert!(e.is_canceled());
    }

    #[test]
    fn upload_info_maps_entry() {
        let info = json!({
            "fileId":"f-9","fileName":"服务端改名.mkv","fileSize":42,
            "parentId":"d-1","ctime":1700000000,"utime":1700000001
        });
        let e = entry_from_upload_info(&info, "/电影", "本地名.mkv", 42);
        assert_eq!(e.id, "f-9");
        assert_eq!(e.name, "服务端改名.mkv");
        assert_eq!(e.path, "/电影/服务端改名.mkv");
        assert!(!e.is_dir);
        assert_eq!(e.size, 42);
        assert_eq!(e.modified, Some(1_700_000_001));

        let fallback = entry_from_upload_info(&json!({}), "/", "a.bin", 7);
        assert_eq!(fallback.path, "/a.bin");
        assert_eq!(fallback.size, 7);
    }

    #[test]
    fn oss_error_classification() {
        let e = classify_oss_error(
            404,
            "<Error><Code>NoSuchBucket</Code><Message>no</Message></Error>",
            None,
        );
        assert_eq!(e.kind(), wp_core::error::ErrorKind::NotFound);
        let e = classify_oss_error(403, "<Error><Code>AccessDenied</Code></Error>", Some("rid"));
        assert!(matches!(e, Error::Server { code: 403, .. }));
        assert!(e.to_string().contains("AccessDenied"));
        assert!(transient_status(500));
        assert!(transient_status(429));
        assert!(!transient_status(403));
    }
}
