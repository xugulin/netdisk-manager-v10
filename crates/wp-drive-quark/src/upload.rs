//! 上传：MD5/SHA1 + 增量 SHA1 上下文 + OSS 4 MiB 分片直传。
//!
//! 实测协议要点（均来自 Python 版 2026-09-14 spike）：
//! * `file/upload/pre` → `task_id/auth_info/upload_id/obj_key/bucket/callback`；
//!   响应里已有 `fid` 且（`finish=true` 或无 `upload_id`）时按秒传命中处理；
//! * `file/update/hash` 提交 md5 + sha1；
//! * 每片先 `file/upload/auth` 换 `auth_key`，再裸 PUT 到 OSS；
//! * 第 2 片起必须带 `X-Oss-Hash-Ctx`（前缀所有字节的 SHA1 中间状态）；
//! * `auth_meta` 与实际请求头逐字节一致，x-oss-* 顺序固定；
//! * 合并 XML 的 ETag 带双引号，`Content-MD5` 基于最终 XML；HTTP 200/203 都算成功；
//! * `file/upload/finish` 返回 fid，之后列表有 1~3 秒可见性延迟。

use crate::{
    as_bool, as_str, compact_json, data_of, now_ms, QuarkDrive, OSS_DEFAULT_BUCKET, OSS_UA,
};
use serde_json::json;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::task::JoinSet;
use wp_core::model::{ProgressFn, RemoteEntry, UploadOptions};
use wp_core::util::join_path;
use wp_core::{Error, Result};
use wp_net::crypto::{base64_encode, rfc1123_now};

const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
const SINGLE_THRESHOLD: u64 = 5 * 1024 * 1024;
const DEFAULT_CONCURRENCY: usize = 3;
const MAX_CONCURRENCY: usize = 8;
const PART_RETRIES: u32 = 3;

// ───────────────────────── SHA1（支持中间状态） ─────────────────────────

/// 可导出中间状态的 SHA1。
///
/// 夸克的 `X-Oss-Hash-Ctx` 是 base64(JSON)，包含 h0..h4 与已处理位数。
/// 分片大小恒为 64 的倍数时，分片边界一定是完整 SHA1 块，不需要 padding。
#[derive(Debug, Clone)]
pub(crate) struct Sha1State {
    h: [u32; 5],
    len: u64,
    buf: [u8; 64],
    buf_len: usize,
}

impl Sha1State {
    pub fn new() -> Self {
        Self {
            h: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0],
            len: 0,
            buf: [0u8; 64],
            buf_len: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for t in 16..80 {
            let x = w[t - 3] ^ w[t - 8] ^ w[t - 14] ^ w[t - 16];
            w[t] = x.rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) =
            (self.h[0], self.h[1], self.h[2], self.h[3], self.h[4]);
        for (t, word) in w.iter().enumerate() {
            let (f, k) = match t {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        self.h[0] = self.h[0].wrapping_add(a);
        self.h[1] = self.h[1].wrapping_add(b);
        self.h[2] = self.h[2].wrapping_add(c);
        self.h[3] = self.h[3].wrapping_add(d);
        self.h[4] = self.h[4].wrapping_add(e);
    }

    /// 标准 SHA1 收尾（含 padding），返回 20 字节摘要。
    pub fn digest(&self) -> [u8; 20] {
        let mut s = self.clone();
        let bit_len = s.len.wrapping_mul(8);
        s.update(&[0x80]);
        while s.buf_len != 56 {
            s.update(&[0u8]);
        }
        s.update(&bit_len.to_be_bytes());
        debug_assert_eq!(s.buf_len, 0);
        let mut out = [0u8; 20];
        for (i, v) in s.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
        }
        out
    }

    pub fn hex(&self) -> String {
        self.digest().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// 导出夸克增量哈希上下文字符串（base64(JSON)）。
    pub fn context_b64(&self) -> String {
        debug_assert_eq!(self.buf_len, 0, "分片边界必须落在完整 SHA1 块上");
        let len_bits = self.len.wrapping_mul(8);
        let json = format!(
            "{{\"hash_type\":\"sha1\",\"h0\":\"{}\",\"h1\":\"{}\",\"h2\":\"{}\",\
             \"h3\":\"{}\",\"h4\":\"{}\",\"Nl\":\"{}\",\"Nh\":\"0\",\"data\":\"\",\"num\":\"0\"}}",
            self.h[0], self.h[1], self.h[2], self.h[3], self.h[4], len_bits
        );
        base64_encode(json.as_bytes())
    }
}

impl Default for Sha1State {
    fn default() -> Self {
        Self::new()
    }
}

// ───────────────────────── 哈希 / 分片规划 ─────────────────────────

pub(crate) struct HashResult {
    pub md5: String,
    pub sha1: String,
    /// 与分片一一对应；第 1 片为 None。
    pub contexts: Vec<Option<String>>,
}

fn join_err(e: tokio::task::JoinError) -> Error {
    Error::other(format!("上传后台任务失败：{e}"))
}

fn read_range(path: &Path, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// 一遍读取同时算 MD5、完整 SHA1 与每片的前缀 SHA1 上下文。
pub(crate) fn hash_file(path: &Path, parts: &[(u64, u64)]) -> std::io::Result<HashResult> {
    use md5::Digest as _;
    let mut md5 = md5::Md5::new();
    let mut sha = Sha1State::new();
    let mut contexts = Vec::with_capacity(parts.len());
    let mut file = File::open(path)?;
    for (i, (offset, len)) in parts.iter().enumerate() {
        if i > 0 {
            if offset % 64 != 0 || sha.buf_len != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "分片大小必须是 64 的倍数（增量 SHA1 上下文要求完整块）",
                ));
            }
            contexts.push(Some(sha.context_b64()));
        } else {
            contexts.push(None);
        }
        if *len == 0 {
            continue;
        }
        let mut buf = vec![0u8; *len as usize];
        file.read_exact(&mut buf)?;
        md5.update(&buf);
        sha.update(&buf);
    }
    Ok(HashResult {
        md5: format!("{:x}", md5.finalize()),
        sha1: sha.hex(),
        contexts,
    })
}

/// 选择分片大小。
///
/// 实测通过的形态是固定 4 MiB；`UploadOptions.chunk_size` 只接受
/// **≥5 MiB、≤32 MiB 且为 64 的倍数**的自定义值（OSS multipart 对非末片
/// 有最小 5 MB 限制；4 MiB 是夸克实测放行的特例）。其余输入一律回退 4 MiB。
pub(crate) fn choose_chunk_size(opt: u64, total: u64) -> u64 {
    let mut chunk = CHUNK_SIZE;
    if (SINGLE_THRESHOLD..=32 * 1024 * 1024).contains(&opt) && opt % 64 == 0 {
        chunk = opt;
    }
    // >=5MB 的文件必须走多片；自定义片过大时回退到实测通过的 4MiB。
    if total >= SINGLE_THRESHOLD && chunk >= total {
        chunk = CHUNK_SIZE;
    }
    chunk
}

fn plan_parts(total: u64, chunk: u64) -> Vec<(u64, u64)> {
    if total < SINGLE_THRESHOLD {
        return vec![(0, total)];
    }
    let chunk = chunk.max(64);
    let mut parts = Vec::new();
    let mut offset = 0u64;
    while offset < total {
        let size = chunk.min(total - offset);
        parts.push((offset, size));
        offset += size;
    }
    parts
}

pub(crate) fn guess_mime(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "mp4" => "video/mp4",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "json" => "application/json",
        "zip" => "application/zip",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        "apk" => "application/vnd.android.package-archive",
        "exe" => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

// ───────────────────────── 签名串 / XML ─────────────────────────

pub(crate) fn auth_meta_put(
    mime: &str,
    oss_date: &str,
    ctx: &str,
    bucket: &str,
    obj_key: &str,
    part: u32,
    upload_id: &str,
) -> String {
    let mut lines = vec![format!("x-oss-date:{oss_date}")];
    if !ctx.is_empty() {
        lines.push(format!("x-oss-hash-ctx:{ctx}"));
    }
    lines.push(format!("x-oss-user-agent:{OSS_UA}"));
    format!(
        "PUT\n\n{mime}\n{oss_date}\n{}\n/{bucket}/{obj_key}?partNumber={part}&uploadId={upload_id}",
        lines.join("\n")
    )
}

pub(crate) fn auth_meta_complete(
    xml_md5: &str,
    oss_date: &str,
    callback_b64: &str,
    bucket: &str,
    obj_key: &str,
    upload_id: &str,
) -> String {
    let mut lines = Vec::new();
    if !callback_b64.is_empty() {
        lines.push(format!("x-oss-callback:{callback_b64}"));
    }
    lines.push(format!("x-oss-date:{oss_date}"));
    lines.push(format!("x-oss-user-agent:{OSS_UA}"));
    format!(
        "POST\n{xml_md5}\napplication/xml\n{oss_date}\n{}\n/{bucket}/{obj_key}?uploadId={upload_id}",
        lines.join("\n")
    )
}

pub(crate) fn build_complete_xml(parts: &[(u32, String)]) -> String {
    let inner = parts
        .iter()
        .map(|(n, etag)| {
            format!("<Part>\n<PartNumber>{n}</PartNumber>\n<ETag>\"{etag}\"</ETag>\n</Part>")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <CompleteMultipartUpload>\n{inner}\n</CompleteMultipartUpload>"
    )
}

// ───────────────────────── 上传执行 ─────────────────────────

#[derive(Clone)]
struct UploadToken {
    task_id: String,
    auth_info: String,
    upload_id: String,
    obj_key: String,
    bucket: String,
    mime: String,
}

fn report(progress: &mut ProgressFn, done: u64, total: u64) -> Result<()> {
    if progress(done, total) {
        Ok(())
    } else {
        Err(Error::Canceled("上传已取消".into()))
    }
}

async fn upload_part(
    api: crate::session::Api,
    oss: reqwest::Client,
    token: UploadToken,
    part: u32,
    offset: u64,
    len: u64,
    ctx: String,
    path: PathBuf,
) -> Result<(u32, u64, String)> {
    let data = tokio::task::spawn_blocking(move || read_range(&path, offset, len))
        .await
        .map_err(join_err)?
        .map_err(Error::Io)?;
    let oss_url = format!(
        "https://{}.pds.quark.cn/{}?partNumber={part}&uploadId={}",
        token.bucket, token.obj_key, token.upload_id
    );
    let mut last_err: Option<Error> = None;
    for attempt in 0..=PART_RETRIES {
        let oss_date = rfc1123_now();
        let auth_meta = auth_meta_put(
            &token.mime,
            &oss_date,
            &ctx,
            &token.bucket,
            &token.obj_key,
            part,
            &token.upload_id,
        );
        let auth_result = api
            .post(
                "file/upload/auth",
                json!({
                    "task_id": token.task_id,
                    "auth_info": token.auth_info,
                    "auth_meta": auth_meta,
                }),
            )
            .await
            .and_then(|resp| {
                let data = data_of(&resp);
                let key = as_str(data.get("auth_key")).unwrap_or_default();
                if key.is_empty() {
                    Err(Error::other(format!(
                        "分片 {part} 未取到 auth_key：{}",
                        compact_json(&resp)
                    )))
                } else {
                    Ok(key)
                }
            });

        let auth_key = match auth_result {
            Ok(k) => k,
            Err(e) => {
                last_err = Some(e);
                if attempt < PART_RETRIES {
                    let wait = (1u64 << (attempt + 1)).min(10);
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
                continue;
            }
        };

        let mut rb = oss
            .put(&oss_url)
            .header("Content-Type", token.mime.as_str())
            .header("x-oss-date", oss_date.as_str())
            .header("x-oss-user-agent", OSS_UA)
            .header("authorization", auth_key.as_str())
            .body(data.clone());
        if !ctx.is_empty() {
            rb = rb.header("X-Oss-Hash-Ctx", ctx.as_str());
        }
        let resp = match rb.send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(wp_net::client::net_err(e));
                if attempt < PART_RETRIES {
                    let wait = (1u64 << (attempt + 1)).min(10);
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
                continue;
            }
        };
        let status = resp.status().as_u16();
        if status != 200 {
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(400).collect();
            last_err = Some(Error::server(
                status as i64,
                format!("OSS 分片 {part} 上传失败：HTTP {status} {snippet}"),
            ));
            if attempt < PART_RETRIES {
                let wait = (1u64 << (attempt + 1)).min(10);
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
            continue;
        }
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().trim_matches('"').to_string())
            .unwrap_or_default();
        if etag.is_empty() {
            last_err = Some(Error::other(format!(
                "OSS 分片 {part} 返回 200 但没有 ETag"
            )));
            if attempt < PART_RETRIES {
                let wait = (1u64 << (attempt + 1)).min(10);
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
            continue;
        }
        return Ok((part, len, etag));
    }
    let e = last_err.unwrap_or_else(|| Error::other(format!("分片 {part} 上传失败")));
    wp_core::log_error!(
        "夸克",
        "分片 {} 重试 {} 次后仍失败：{}",
        part,
        PART_RETRIES,
        wp_core::error::err_line(&e)
    );
    Err(e)
}

impl QuarkDrive {
    pub(crate) async fn upload_file(
        &self,
        local: &Path,
        remote_dir: &str,
        opts: &UploadOptions,
        mut progress: ProgressFn,
    ) -> Result<RemoteEntry> {
        self.api.ensure_valid()?;
        let meta = tokio::fs::metadata(local).await.map_err(Error::Io)?;
        if !meta.is_file() {
            return Err(Error::other(format!(
                "上传源不是普通文件：{}",
                local.display()
            )));
        }
        let total = meta.len();
        let file_name = opts
            .rename_to
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| local.file_name().map(|s| s.to_string_lossy().to_string()))
            .ok_or_else(|| Error::other("无法确定上传文件名"))?;
        if file_name.contains('/') || file_name.contains('\\') {
            return Err(Error::other("上传文件名包含路径分隔符"));
        }
        let mime = guess_mime(&file_name);
        let dir = self.ensure_dir_entry(remote_dir).await?;
        let target_path = join_path(&dir.path, &file_name);

        report(&mut progress, 0, total)?;

        let chunk_size = choose_chunk_size(opts.chunk_size, total);
        let parts = plan_parts(total, chunk_size);
        let part_count = parts.len();

        // 哈希在 blocking 线程里一次读完：MD5 + 完整 SHA1 + 每片前缀上下文。
        let hash_path = local.to_path_buf();
        let hash_parts = parts.clone();
        let hash = tokio::task::spawn_blocking(move || hash_file(&hash_path, &hash_parts))
            .await
            .map_err(join_err)?
            .map_err(Error::Io)?;
        report(&mut progress, total / 10, total)?;

        // ① 预上传（顺带秒传判定）。
        report(&mut progress, total / 10, total)?;
        let now = now_ms();
        let pre_body = json!({
            "ccp_hash_update": true,
            "parallel_upload": true,
            "pdir_fid": dir.id,
            "dir_name": "",
            "size": total,
            "file_name": &file_name,
            "format_type": mime,
            "l_updated_at": now,
            "l_created_at": now,
        });
        let pre = self.api.post("file/upload/pre", pre_body).await?;
        let pre_data = data_of(&pre);
        if !pre_data.is_object() {
            return Err(Error::other(format!(
                "预上传返回异常：{}",
                compact_json(&pre)
            )));
        }
        let quick_fid = as_str(pre_data.get("fid")).unwrap_or_default();
        let upload_id = as_str(pre_data.get("upload_id")).unwrap_or_default();
        let quick_hit = !quick_fid.is_empty()
            && (as_bool(pre_data.get("finish")) == Some(true) || upload_id.is_empty());
        if quick_hit {
            report(&mut progress, total, total)?;
            self.invalidate_cache();
            wp_core::log_info!("夸克", "秒传命中，跳过上传：{file_name} fid={quick_fid}");
            return Ok(self
                .finish_uploaded_file(&file_name, &target_path, &quick_fid, total)
                .await);
        }

        let task_id = as_str(pre_data.get("task_id"))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::other(format!(
                    "预上传失败，未取到 task_id：{}",
                    compact_json(&pre)
                ))
            })?;
        let auth_info = as_str(pre_data.get("auth_info")).unwrap_or_default();
        let obj_key = as_str(pre_data.get("obj_key")).unwrap_or_default();
        let bucket = as_str(pre_data.get("bucket"))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| OSS_DEFAULT_BUCKET.to_string());
        if upload_id.is_empty() || obj_key.is_empty() || auth_info.is_empty() {
            return Err(Error::other(format!(
                "预上传返回字段不完整（upload_id/obj_key/auth_info）：{}",
                compact_json(&pre_data)
            )));
        }
        let callback = pre_data.get("callback").cloned().filter(|v| !v.is_null());

        // ② 提交哈希。
        report(&mut progress, total * 12 / 100, total)?;
        let _ = self
            .api
            .post(
                "file/update/hash",
                json!({"task_id": &task_id, "md5": &hash.md5, "sha1": &hash.sha1}),
            )
            .await?;

        // ③ 分片授权 + OSS PUT（可配置并发；默认 3，避免风控）。
        report(&mut progress, total * 15 / 100, total)?;
        let concurrency = if opts.concurrency == 0 {
            DEFAULT_CONCURRENCY
        } else {
            opts.concurrency
        }
        .clamp(1, MAX_CONCURRENCY)
        .min(part_count.max(1));
        let token = UploadToken {
            task_id: task_id.clone(),
            auth_info: auth_info.clone(),
            upload_id: upload_id.clone(),
            obj_key: obj_key.clone(),
            bucket: bucket.clone(),
            mime: mime.to_string(),
        };
        let mut set: JoinSet<Result<(u32, u64, String)>> = JoinSet::new();
        let mut results: Vec<(u32, String)> = Vec::new();
        let mut completed_bytes = 0u64;

        let abort_with = |set: &mut JoinSet<Result<(u32, u64, String)>>, e: Error| {
            set.abort_all();
            e
        };

        for (i, (offset, len)) in parts.iter().enumerate() {
            while set.len() >= concurrency {
                match set.join_next().await {
                    Some(Ok(Ok((n, size, etag)))) => {
                        completed_bytes += size;
                        results.push((n, etag));
                        let ratio = if total == 0 {
                            1.0
                        } else {
                            completed_bytes as f64 / total as f64
                        };
                        let done = (total as f64 * (0.15 + 0.75 * ratio)) as u64;
                        if let Err(e) = report(&mut progress, done, total) {
                            set.abort_all();
                            return Err(abort_with(&mut set, e));
                        }
                    }
                    Some(Ok(Err(e))) => {
                        set.abort_all();
                        wp_core::log_warn!(
                            "夸克",
                            "上传失败，服务端可能留下幽灵文件（task_id={task_id}）：{}",
                            wp_core::error::err_line(&e)
                        );
                        return Err(abort_with(&mut set, e));
                    }
                    Some(Err(e)) => {
                        set.abort_all();
                        let e = join_err(e);
                        wp_core::log_warn!(
                            "夸克",
                            "上传分片任务崩溃（task_id={task_id}）：{}",
                            wp_core::error::err_line(&e)
                        );
                        return Err(abort_with(&mut set, e));
                    }
                    None => break,
                }
            }
            let part_no = (i + 1) as u32;
            let ctx = hash.contexts.get(i).cloned().flatten().unwrap_or_default();
            set.spawn(upload_part(
                self.api.clone(),
                self.oss_http.clone(),
                token.clone(),
                part_no,
                *offset,
                *len,
                ctx,
                local.to_path_buf(),
            ));
        }

        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(Ok((n, size, etag))) => {
                    completed_bytes += size;
                    results.push((n, etag));
                    let ratio = if total == 0 {
                        1.0
                    } else {
                        completed_bytes as f64 / total as f64
                    };
                    let done = (total as f64 * (0.15 + 0.75 * ratio)) as u64;
                    if let Err(e) = report(&mut progress, done, total) {
                        set.abort_all();
                        return Err(abort_with(&mut set, e));
                    }
                }
                Ok(Err(e)) => {
                    set.abort_all();
                    wp_core::log_warn!(
                        "夸克",
                        "上传失败，服务端可能留下幽灵文件（task_id={task_id}）：{}",
                        wp_core::error::err_line(&e)
                    );
                    return Err(abort_with(&mut set, e));
                }
                Err(e) => {
                    set.abort_all();
                    let e = join_err(e);
                    return Err(abort_with(&mut set, e));
                }
            }
        }

        results.sort_by_key(|(n, _)| *n);
        if results.len() != part_count {
            return Err(Error::other(format!(
                "分片上传数量不符：计划 {part_count}，完成 {}",
                results.len()
            )));
        }

        // ④ 合并（200/203 都算成功）。
        report(&mut progress, total * 90 / 100, total)?;
        let xml = build_complete_xml(&results);
        use md5::Digest as _;
        let xml_md5 = {
            let mut h = md5::Md5::new();
            h.update(xml.as_bytes());
            base64_encode(&h.finalize())
        };
        let callback_b64 = callback
            .as_ref()
            .map(|v| base64_encode(compact_json(v).as_bytes()))
            .unwrap_or_default();
        let oss_date = rfc1123_now();
        let complete_meta = auth_meta_complete(
            &xml_md5,
            &oss_date,
            &callback_b64,
            &bucket,
            &obj_key,
            &upload_id,
        );
        let auth = self
            .api
            .post(
                "file/upload/auth",
                json!({
                    "task_id": &task_id,
                    "auth_info": &auth_info,
                    "auth_meta": complete_meta,
                }),
            )
            .await?;
        let auth_key = as_str(data_of(&auth).get("auth_key")).unwrap_or_default();
        if auth_key.is_empty() {
            return Err(Error::other(format!(
                "合并授权未取到 auth_key：{}",
                compact_json(&auth)
            )));
        }
        let complete_url = format!("https://{bucket}.pds.quark.cn/{obj_key}?uploadId={upload_id}");
        let mut rb = self
            .oss_http
            .post(&complete_url)
            .header("Content-Type", "application/xml")
            .header("x-oss-date", oss_date.as_str())
            .header("x-oss-user-agent", OSS_UA)
            .header("Content-MD5", xml_md5.as_str())
            .header("authorization", auth_key.as_str())
            .body(xml);
        if !callback_b64.is_empty() {
            rb = rb.header("x-oss-callback", callback_b64.as_str());
        }
        let resp = rb.send().await.map_err(wp_net::client::net_err)?;
        let status = resp.status().as_u16();
        if status != 200 && status != 203 {
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(400).collect();
            return Err(Error::server(
                status as i64,
                format!("OSS 合并失败：HTTP {status} {snippet}"),
            ));
        }
        if status == 203 {
            wp_core::log_warn!("夸克", "OSS 合并返回 203（合并成功但 callback 失败）");
        }

        // ⑤ finish。
        report(&mut progress, total * 97 / 100, total)?;
        let finish = self
            .api
            .post(
                "file/upload/finish",
                json!({"task_id": &task_id, "obj_key": &obj_key}),
            )
            .await?;
        let fid = as_str(data_of(&finish).get("fid")).unwrap_or_default();
        report(&mut progress, total, total)?;
        self.invalidate_cache();
        if fid.is_empty() {
            return Err(Error::other(format!(
                "完成上传未返回 fid：{}",
                compact_json(&finish)
            )));
        }
        wp_core::log_info!(
            "夸克",
            "上传完成 {file_name}（{total} 字节，{part_count} 片，fid={fid}）"
        );
        Ok(self
            .finish_uploaded_file(&file_name, &target_path, &fid, total)
            .await)
    }

    /// finish 后列表可能 1~3 秒不可见：重试解析，仍失败则返回合成条目（上传已成功）。
    async fn finish_uploaded_file(
        &self,
        file_name: &str,
        target_path: &str,
        fid: &str,
        size: u64,
    ) -> RemoteEntry {
        match self.resolve_retry(target_path, 4, 700).await {
            Ok(mut entry) => {
                if entry.id.is_empty() {
                    entry.id = fid.to_string();
                }
                entry
            }
            Err(e) => {
                wp_core::log_warn!(
                    "夸克",
                    "上传成功但路径暂不可见（{target_path}）：{}；返回合成条目",
                    wp_core::error::err_line(&e)
                );
                let mut entry = RemoteEntry::file(file_name, target_path, size);
                entry.id = fid.to_string();
                entry.modified = Some(wp_core::util::now_ts());
                entry
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_known_vectors_and_context() {
        let mut s = Sha1State::new();
        s.update(b"abc");
        assert_eq!(s.hex(), "a9993e364706816aba3e25717850c26c9cd0d89d");

        let mut s = Sha1State::new();
        s.update(b"a");
        s.update(b"b");
        s.update(b"c");
        assert_eq!(s.hex(), "a9993e364706816aba3e25717850c26c9cd0d89d");

        let s = Sha1State::new();
        assert_eq!(s.hex(), "da39a3ee5e6b4b0d3255bfef95601890afd80709");

        // 4 MiB 的 'a'：中间状态必须与 Python 版 压缩SHA1块 完全一致。
        let data = vec![b'a'; 4 * 1024 * 1024];
        let mut s = Sha1State::new();
        s.update(&data);
        assert_eq!(
            s.context_b64(),
            "eyJoYXNoX3R5cGUiOiJzaGExIiwiaDAiOiIxMTk2MDQzNzAiLCJoMSI6IjU3OTkwNjAwMyIsImgyIjoiMTkwMjE0NTMzNyIsImgzIjoiMzIzMDExNjUwNSIsImg0IjoiNjEyNzExNDIiLCJObCI6IjMzNTU0NDMyIiwiTmgiOiIwIiwiZGF0YSI6IiIsIm51bSI6IjAifQ=="
        );
        // 加标准 padding 收尾后必须等于 hashlib.sha1。
        assert_eq!(s.hex(), "ec10fafa5de7a233c22a26d491f308876b41ae3f");
    }

    #[test]
    fn sha1_matches_crate_on_random_blocks() {
        use sha1::Digest as _;
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut a = Sha1State::new();
        for chunk in data.chunks(7777) {
            a.update(chunk);
        }
        let expected = {
            let mut h = sha1::Sha1::new();
            h.update(&data);
            format!("{:x}", h.finalize())
        };
        assert_eq!(a.hex(), expected);
    }

    #[test]
    fn ots_auth_meta_exact() {
        let meta = auth_meta_put(
            "application/octet-stream",
            "Sun, 13 Sep 2026 17:54:18 GMT",
            "CTX",
            "ul-zb",
            "obj/key",
            2,
            "up-1",
        );
        assert_eq!(
            meta,
            format!(
                "PUT\n\napplication/octet-stream\nSun, 13 Sep 2026 17:54:18 GMT\n\
                 x-oss-date:Sun, 13 Sep 2026 17:54:18 GMT\n\
                 x-oss-hash-ctx:CTX\n\
                 x-oss-user-agent:{OSS_UA}\n\
                 /ul-zb/obj/key?partNumber=2&uploadId=up-1"
            )
        );
        let first = auth_meta_put("video/mp4", "date", "", "b", "k", 1, "u");
        assert!(!first.contains("x-oss-hash-ctx"));
        assert!(first.ends_with("/b/k?partNumber=1&uploadId=u"));
    }

    #[test]
    fn complete_meta_and_xml_exact() {
        let meta = auth_meta_complete("MD5", "date", "CB", "b", "k", "u");
        assert_eq!(
            meta,
            format!(
                "POST\nMD5\napplication/xml\ndate\n\
                 x-oss-callback:CB\n\
                 x-oss-date:date\n\
                 x-oss-user-agent:{OSS_UA}\n\
                 /b/k?uploadId=u"
            )
        );
        let meta2 = auth_meta_complete("MD5", "date", "", "b", "k", "u");
        assert!(!meta2.contains("x-oss-callback"));

        let xml = build_complete_xml(&[(1, "etag1".into()), (2, "etag2".into())]);
        let expected = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                        <CompleteMultipartUpload>\n\
                        <Part>\n<PartNumber>1</PartNumber>\n<ETag>\"etag1\"</ETag>\n</Part>\n\
                        <Part>\n<PartNumber>2</PartNumber>\n<ETag>\"etag2\"</ETag>\n</Part>\n\
                        </CompleteMultipartUpload>";
        assert_eq!(xml, expected);
    }

    #[test]
    fn chunk_planning_and_mime() {
        assert_eq!(choose_chunk_size(0, 10 * 1024 * 1024), CHUNK_SIZE);
        // 5MiB 恰好走 4+1 两片。
        let chunk = choose_chunk_size(0, 5 * 1024 * 1024);
        let parts = plan_parts(5 * 1024 * 1024, chunk);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], (0, 4 * 1024 * 1024));
        assert_eq!(parts[1], (4 * 1024 * 1024, 1024 * 1024));

        // 小文件单次 PUT。
        assert_eq!(plan_parts(1024, CHUNK_SIZE), vec![(0, 1024)]);
        assert_eq!(plan_parts(0, CHUNK_SIZE), vec![(0, 0)]);

        // 小于 5MiB 的自定义片不安全（OSS multipart 最小 5MB），回退 4MiB。
        assert_eq!(choose_chunk_size(1_000_000, 20 * 1024 * 1024), CHUNK_SIZE);
        // 合法的 8MiB 自定义片被接受，且一定是 64 的倍数。
        let c = choose_chunk_size(8 * 1024 * 1024, 20 * 1024 * 1024);
        assert_eq!(c, 8 * 1024 * 1024);
        assert_eq!(c % 64, 0);

        assert_eq!(guess_mime("a.MP4"), "video/mp4");
        assert_eq!(guess_mime("a.tar.gz"), "application/gzip");
        assert_eq!(guess_mime("noext"), "application/octet-stream");
    }

    #[test]
    fn hash_file_md5_sha1_and_contexts() {
        let dir = std::env::temp_dir().join(format!(
            "wp-quark-hash-{}-{}",
            std::process::id(),
            crate::now_ts()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.bin");
        let data: Vec<u8> = (0..5 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let chunk = CHUNK_SIZE;
        let parts = plan_parts(data.len() as u64, chunk);
        assert_eq!(parts.len(), 2);
        let h = hash_file(&path, &parts).unwrap();
        assert_eq!(h.md5, "4c28640dc8df1933aaea192100d50ae0");
        assert_eq!(h.sha1, "4419108489f30c94a14b667ce4e51fcc62ad8b19");
        assert!(h.contexts[0].is_none());
        assert!(h.contexts[1].as_ref().unwrap().len() > 20);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
