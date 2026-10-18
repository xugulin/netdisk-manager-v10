//! 上传：哈希 → 秒传 → precreate → superfile2 分片 → create。
//!
//! 文档 §5 的关键点：
//! * 分片 4 MiB；首片 256 KiB（仅秒传 slice-md5 用）；
//! * MD5 有“纯净”和“展示态（加密）”两套：precreate/superfile2/create 用纯净，
//!   rapidupload 的 content-md5/slice-md5 用加密态；
//! * 单分片文件的 block_list[0] 是整文件 MD5；空文件也走 1 片；
//! * precreate 返回的 block_list 是占位序号，create 必须用 superfile2 返回的 md5；
//! * 上传域名由 locateupload 动态给出，缓存 60s；分片失败切域名重试 3 次；
//! * < 256 KiB 无法秒传；`data_offset` 是确定性公式，`data_content` 必须真实；
//! * 上传后轻量轮询文件可见性（秒传命中时没有 fs_id）。

use crate::files::{self, has_dot_segment};
use crate::http::{gen_logid, DomainCache, ReqOpts};
use crate::{lock, BaiduDrive, CHUNK_SIZE, PCS_FILE, SLICE_SIZE};
use md5::Digest;
use reqwest::header::{ACCEPT, COOKIE, ORIGIN, REFERER};
use serde_json::Value;
use std::io::SeekFrom;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use wp_core::model::{ProgressFn, RemoteEntry, UploadOptions};
use wp_core::util::{join_path, norm_path, now_ts};
use wp_core::{Error, Result};
use wp_net::client::{http_status_error, net_err};
use wp_net::crypto::{base64_encode, md5_hex};

/// locateupload 完全不可用时的兜底域名（正常绝不该走到）。
const FALLBACK_DOMAIN: &str = "bddwd-cm01.pcs.baidu.com";
/// 上传域名缓存时间。
const DOMAIN_TTL: Duration = Duration::from_secs(60);

fn p(key: &str, value: impl Into<String>) -> (String, String) {
    (key.to_string(), value.into())
}

/// 一次扫描算出的全部上传哈希。
#[derive(Debug, Clone, Default)]
pub(crate) struct Hashes {
    pub size: u64,
    pub content_md5: String,
    pub slice_md5: String,
    pub chunk_md5: Vec<String>,
    pub content_md5_enc: String,
    pub slice_md5_enc: String,
}

/// 官方 `_encryptMd5`：纯净 MD5 → 展示态 MD5。
///
/// 非“32 位纯小写 hex”的输入原样返回（与官方 JS 一致）。
pub(crate) fn encrypt_md5(md5: &str) -> String {
    if md5.len() != 32
        || !md5
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return md5.to_string();
    }
    // ① 4×8 重排：8-16, 0-8, 24-32, 16-24
    let rearranged = format!(
        "{}{}{}{}",
        &md5[8..16],
        &md5[0..8],
        &md5[24..32],
        &md5[16..24]
    );
    // ② 逐位 XOR (index % 16)
    let bytes = rearranged.as_bytes();
    let mut out = String::with_capacity(32);
    for (i, b) in bytes.iter().enumerate() {
        let value = (*b as char).to_digit(16).unwrap_or(0) as u8;
        let x = value ^ (15 & i as u8);
        out.push(char::from_digit(x as u32, 16).unwrap_or('0'));
    }
    // ③ 第 9 位 → chr('g' + 值)
    let digit9 = out.as_bytes()[9] as char;
    let transformed = char::from_u32('g' as u32 + digit9.to_digit(16).unwrap_or(0)).unwrap_or('g');
    format!("{}{}{}", &out[..9], transformed, &out[10..])
}

/// 分片切分（纯函数）：`(offset, len)`；空文件也返回 1 片 `(0,0)`。
pub(crate) fn chunk_ranges(size: u64, chunk_size: u64) -> Vec<(u64, u64)> {
    let chunk_size = chunk_size.max(1);
    if size == 0 {
        return vec![(0, 0)];
    }
    let mut out = Vec::new();
    let mut offset = 0u64;
    while offset < size {
        let len = chunk_size.min(size - offset);
        out.push((offset, len));
        offset += len;
    }
    out
}

/// 秒传 `data_offset` 公式：
/// `int(md5(uk + content_md5_enc + data_time)[:8], 16) % (size - 256KiB + 1)`。
pub(crate) fn rapid_offset(uk: &str, content_md5_enc: &str, data_time: i64, size: u64) -> u64 {
    if size < SLICE_SIZE || uk.is_empty() {
        return 0;
    }
    let window = size - SLICE_SIZE + 1;
    if window <= 1 {
        return 0;
    }
    let digest = md5_hex(format!("{uk}{content_md5_enc}{data_time}").as_bytes());
    let head = u64::from_str_radix(&digest[..8], 16).unwrap_or(0);
    head % window
}

fn report(progress: &mut ProgressFn, total: u64, fraction: f64) -> Result<()> {
    let fraction = fraction.clamp(0.0, 1.0);
    let done = ((total as f64) * fraction) as u64;
    if !progress(done, total) {
        return Err(Error::Canceled("用户取消上传".into()));
    }
    Ok(())
}

/// 单次读盘计算整文件 MD5、首 256KiB MD5 与 4MiB 分片 MD5。
///
/// 进度映射到整文件上传的 0–15%。
pub(crate) async fn compute_hashes(
    local: &Path,
    total: u64,
    progress: &mut ProgressFn,
) -> Result<Hashes> {
    let mut file = tokio::fs::File::open(local).await.map_err(Error::Io)?;
    let mut whole = md5::Md5::new();
    let mut slice = md5::Md5::new();
    let mut slice_read = 0u64;
    let mut chunk_buf: Vec<u8> = Vec::with_capacity(CHUNK_SIZE as usize);
    let mut chunks: Vec<String> = Vec::new();
    let mut read = 0u64;
    let mut buf = vec![0u8; 2 * 1024 * 1024];

    loop {
        let n = file.read(&mut buf).await.map_err(Error::Io)?;
        if n == 0 {
            break;
        }
        let data = &buf[..n];
        whole.update(data);
        if slice_read < SLICE_SIZE {
            let take = ((SLICE_SIZE - slice_read) as usize).min(n);
            slice.update(&data[..take]);
            slice_read += take as u64;
        }
        let mut off = 0usize;
        while off < n {
            let room = CHUNK_SIZE as usize - chunk_buf.len();
            let take = room.min(n - off);
            chunk_buf.extend_from_slice(&data[off..off + take]);
            off += take;
            if chunk_buf.len() == CHUNK_SIZE as usize {
                chunks.push(md5_hex(&chunk_buf));
                chunk_buf.clear();
            }
        }
        read += n as u64;
        let fraction = if total > 0 {
            0.15 * (read as f64 / total as f64)
        } else {
            0.15
        };
        report(progress, total, fraction)?;
    }
    if !chunk_buf.is_empty() || chunks.is_empty() {
        chunks.push(md5_hex(&chunk_buf));
    }
    let content_md5 = format!("{:x}", whole.finalize());
    let slice_md5 = format!("{:x}", slice.finalize());
    Ok(Hashes {
        size: total,
        content_md5_enc: encrypt_md5(&content_md5),
        slice_md5_enc: encrypt_md5(&slice_md5),
        content_md5,
        slice_md5,
        chunk_md5: chunks,
    })
}

/// 上传单个本地文件到远端目录。
pub(crate) async fn upload(
    d: &BaiduDrive,
    local: &Path,
    remote_dir: &str,
    opts: &UploadOptions,
    mut progress: ProgressFn,
) -> Result<RemoteEntry> {
    let meta = tokio::fs::metadata(local).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::not_found(format!("本地文件不存在：{}", local.display()))
        } else {
            Error::Io(e)
        }
    })?;
    if !meta.is_file() {
        return Err(Error::other(format!(
            "只能上传普通文件：{}",
            local.display()
        )));
    }
    let total = meta.len();

    let dir = norm_path(remote_dir);
    if has_dot_segment(remote_dir) {
        return Err(Error::other(format!(
            "远端目录含非法路径段（. / ..）：{remote_dir:?}"
        )));
    }
    let name = opts
        .rename_to
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| local.file_name().map(|s| s.to_string_lossy().to_string()))
        .ok_or_else(|| Error::other("无法确定远端文件名"))?;
    if name.contains('/') || name == "." || name == ".." {
        return Err(Error::other(format!("非法远端文件名：{name:?}")));
    }
    let remote_path = join_path(&dir, &name);

    // 分片固定 4MiB（百度上传协议要求）；并发分片未实现，保持串行以降低风控。
    // `opts.chunk_size` / `opts.concurrency` 在这里刻意忽略。
    // 先确保父目录存在（百度“新建已存在目录”会悄悄建副本，必须先探测）。
    files::ensure_dir(d, &dir).await?;
    report(&mut progress, total, 0.0)?;

    let hashes = compute_hashes(local, total, &mut progress).await?;
    wp_core::log_info!(
        "百度",
        "上传 {}：{}B，{} 片，md5={}，slice-md5={}",
        name,
        hashes.size,
        hashes.chunk_md5.len(),
        hashes.content_md5,
        hashes.slice_md5
    );

    // 秒传（< 256KiB 跳过）
    if hashes.size >= SLICE_SIZE {
        match try_rapid(d, local, &remote_path, &dir, &hashes).await {
            Ok(true) => {
                wp_core::log_info!("百度", "秒传命中：{remote_path}");
                report(&mut progress, total, 1.0)?;
                let fallback =
                    RemoteEntry::file(&name, &remote_path, hashes.size).with_modified(now_ts());
                return Ok(wait_visible(d, &remote_path, fallback).await);
            }
            Ok(false) => {}
            Err(e) => {
                // 与 Python 版一致：秒传异常回落分片上传（服务端未命中是正常分支）。
                wp_core::log_warn!("百度", "秒传失败/未命中，回落分片上传：{e}");
            }
        }
    }
    report(&mut progress, total, 0.20)?;

    let uploadid = precreate(d, &remote_path, &dir, &hashes).await?;
    report(&mut progress, total, 0.25)?;

    let server_md5 = upload_chunks(d, local, &remote_path, &uploadid, total, &mut progress).await?;
    if server_md5.is_empty() {
        return Err(Error::server(-1, "superfile2 未返回任何分片 md5"));
    }
    report(&mut progress, total, 0.90)?;

    let created = create_file(d, &remote_path, total, &uploadid, &dir, &server_md5).await?;
    report(&mut progress, total, 1.0)?;

    let fallback = {
        let mut e = RemoteEntry::file(&name, &remote_path, total);
        e.id = created
            .get("fs_id")
            .and_then(Value::as_i64)
            .map(|n| n.to_string())
            .unwrap_or_default();
        e.modified = created
            .get("server_mtime")
            .and_then(Value::as_i64)
            .or_else(|| created.get("mtime").and_then(Value::as_i64))
            .filter(|t| *t > 0);
        e
    };
    let entry = files::entry_from_value(&created, &dir).unwrap_or(fallback);
    wp_core::log_info!(
        "百度",
        "上传完成：{remote_path} fs_id={} md5={}",
        entry.id,
        hashes.content_md5
    );
    Ok(wait_visible(d, &remote_path, entry).await)
}

/// 上传后轻量轮询可见性（最多 6 次、每次 200ms）。
async fn wait_visible(d: &BaiduDrive, path: &str, fallback: RemoteEntry) -> RemoteEntry {
    for i in 0..6 {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        match files::stat(d, path).await {
            Ok(entry) => return entry,
            Err(Error::NotFound(_)) => continue,
            Err(e) => {
                wp_core::log_debug!("百度", "上传后查询可见性失败（继续等待）：{e}");
                continue;
            }
        }
    }
    fallback
}

/// 尝试秒传；`Ok(true)` 命中，`Ok(false)` 未命中/太小。
async fn try_rapid(
    d: &BaiduDrive,
    local: &Path,
    remote_path: &str,
    target_dir: &str,
    hashes: &Hashes,
) -> Result<bool> {
    if hashes.size < SLICE_SIZE {
        return Ok(false);
    }
    let uk = match d.uk_value() {
        Some(uk) => uk.to_string(),
        None => {
            // uk 不在 Cookie 里；补取一次，失败则 offset 退化 0。
            if d.refresh_template().await.is_ok() {
                d.uk_value().map(|v| v.to_string()).unwrap_or_default()
            } else {
                String::new()
            }
        }
    };
    let data_time = now_ts();
    let offset = rapid_offset(&uk, &hashes.content_md5_enc, data_time, hashes.size);
    let slice = read_slice(local, offset, SLICE_SIZE).await?;
    let data_content = base64_encode(&slice);

    let form = vec![
        p("path", remote_path),
        p("content-length", hashes.size.to_string()),
        p("content-md5", hashes.content_md5_enc.clone()),
        p("slice-md5", hashes.slice_md5_enc.clone()),
        p("target_path", target_dir),
        p("local_mtime", data_time.to_string()),
        p("data_time", data_time.to_string()),
        p("data_offset", offset.to_string()),
        p("data_content", data_content),
    ];
    let v = d
        .api_post_form(
            "/api/rapidupload",
            &[p("rtype", "1")],
            &form,
            ReqOpts::authed().with_bdstoken().allow(0).allow(404),
        )
        .await?;
    let errno = v.get("errno").and_then(Value::as_i64).unwrap_or(0);
    if errno == 0 {
        return Ok(true);
    }
    if errno == 404 {
        wp_core::log_info!("百度", "秒传未命中（errno:404），回落分片上传");
        return Ok(false);
    }
    Err(crate::map_errno(errno, &v))
}

/// precreate：返回 uploadid。
async fn precreate(
    d: &BaiduDrive,
    remote_path: &str,
    target_dir: &str,
    hashes: &Hashes,
) -> Result<String> {
    let block_list = serde_json::to_string(&hashes.chunk_md5).unwrap_or_else(|_| "[]".into());
    let form = vec![
        p("path", remote_path),
        p("autoinit", "1"),
        p("block_list", block_list),
        p("target_path", target_dir),
        p("local_mtime", now_ts().to_string()),
    ];
    let v = d
        .api_post_form(
            "/api/precreate",
            &[p("rtype", "1")],
            &form,
            ReqOpts::authed().with_bdstoken(),
        )
        .await?;
    let uploadid = v
        .get("uploadid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if uploadid.is_empty() {
        return Err(Error::server(-1, "precreate 未返回 uploadid"));
    }
    wp_core::log_debug!(
        "百度",
        "precreate ✅ {} 片，uploadid={}…",
        hashes.chunk_md5.len(),
        &uploadid[..uploadid.len().min(12)]
    );
    Ok(uploadid)
}

/// create：用 superfile2 返回的 md5 列表创建文件。
async fn create_file(
    d: &BaiduDrive,
    remote_path: &str,
    size: u64,
    uploadid: &str,
    target_dir: &str,
    server_md5: &[String],
) -> Result<Value> {
    if server_md5.is_empty() {
        return Err(Error::server(
            -1,
            "block_list 为空：必须使用 superfile2 返回的 md5，不能用 precreate 占位序号",
        ));
    }
    let block_list = serde_json::to_string(server_md5).unwrap_or_else(|_| "[]".into());
    let form = vec![
        p("path", remote_path),
        p("size", size.to_string()),
        p("uploadid", uploadid),
        p("block_list", block_list),
        p("target_path", target_dir),
        p("local_mtime", now_ts().to_string()),
    ];
    let v = d
        .api_post_form(
            "/api/create",
            &[p("isdir", "0"), p("rtype", "1")],
            &form,
            ReqOpts::authed().with_bdstoken(),
        )
        .await?;
    wp_core::log_debug!(
        "百度",
        "create ✅ {remote_path} fs_id={}",
        v.get("fs_id").map(|x| x.to_string()).unwrap_or_default()
    );
    Ok(v)
}

/// 逐片上传（串行，与 Python 版一致；避免触发风控）。
async fn upload_chunks(
    d: &BaiduDrive,
    local: &Path,
    remote_path: &str,
    uploadid: &str,
    total: u64,
    progress: &mut ProgressFn,
) -> Result<Vec<String>> {
    let ranges = chunk_ranges(total, CHUNK_SIZE);
    let mut file = tokio::fs::File::open(local).await.map_err(Error::Io)?;
    let mut md5s = Vec::with_capacity(ranges.len());
    let mut uploaded = 0u64;
    for (seq, (offset, len)) in ranges.iter().enumerate() {
        file.seek(SeekFrom::Start(*offset))
            .await
            .map_err(Error::Io)?;
        let mut data = vec![0u8; *len as usize];
        if *len > 0 {
            file.read_exact(&mut data).await.map_err(Error::Io)?;
        }
        let md5 = upload_one_chunk(d, remote_path, uploadid, seq, data).await?;
        md5s.push(md5);
        uploaded += *len;
        let fraction = if total > 0 {
            0.25 + 0.65 * (uploaded as f64 / total as f64)
        } else {
            0.90
        };
        report(progress, total, fraction)?;
    }
    wp_core::log_info!("百度", "分片上传完成：{remote_path} {} 片", md5s.len());
    Ok(md5s)
}

fn chunk_error_retryable(e: &Error) -> bool {
    !matches!(
        e,
        Error::Auth(_) | Error::Canceled(_) | Error::Server { code: 132, .. }
    )
}

/// 单片上传：失败最多 3 次，每次切域名并退避 0.5/1s。
async fn upload_one_chunk(
    d: &BaiduDrive,
    remote_path: &str,
    uploadid: &str,
    seq: usize,
    data: Vec<u8>,
) -> Result<String> {
    let mut last_error: Option<Error> = None;
    for attempt in 0..3u32 {
        let domain = current_domain(d).await;
        match post_chunk(d, &domain, remote_path, uploadid, seq, data.clone()).await {
            Ok(md5) => return Ok(md5),
            Err(e) => {
                let retry = attempt < 2 && chunk_error_retryable(&e);
                wp_core::log_warn!(
                    "百度",
                    "superfile2 partseq={seq} 第 {}/3 次失败（{domain}）：{e}",
                    attempt + 1
                );
                last_error = Some(e);
                if !retry {
                    break;
                }
                let next = advance_domain(d).await;
                wp_core::log_warn!("百度", "切换上传域名 → {next}");
                tokio::time::sleep(Duration::from_millis(500 * (attempt as u64 + 1))).await;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| Error::server(-1, format!("分片 {seq} 上传失败"))))
}

async fn post_chunk(
    d: &BaiduDrive,
    domain: &str,
    remote_path: &str,
    uploadid: &str,
    seq: usize,
    data: Vec<u8>,
) -> Result<String> {
    let cookie = d.cookie_header();
    if cookie.is_empty() {
        return Err(Error::auth("上传通道需要 BDUSS/STOKEN Cookie，请重新登录"));
    }
    let params = vec![
        p("method", "upload"),
        p("logid", gen_logid()),
        p("app_id", crate::APP_ID),
        p("channel", "chunlei"),
        p("web", "1"),
        p("clienttype", "0"),
        p("path", remote_path),
        p("uploadid", uploadid),
        p("uploadsign", "0"),
        p("partseq", seq.to_string()),
        p("dp-logid", d.http.dp.fresh()),
    ];
    let part = reqwest::multipart::Part::bytes(data)
        .file_name(format!("part{seq}"))
        .mime_str("application/octet-stream")
        .map_err(|e| Error::other(format!("构造分片 multipart 失败：{e}")))?;
    let form = reqwest::multipart::Form::new().part("file", part);
    let url = format!("https://{domain}/rest/2.0/pcs/superfile2");
    let resp = d
        .http
        .api
        .post(&url)
        .query(&params)
        .multipart(form)
        .header(REFERER, "https://pan.baidu.com/disk/main")
        .header(ORIGIN, "https://pan.baidu.com")
        .header(ACCEPT, "*/*")
        .header(COOKIE, cookie)
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .map_err(net_err)?;
    let status = resp.status();
    let text = resp.text().await.map_err(net_err)?;
    if !status.is_success() {
        return Err(http_status_error(status.as_u16(), &text));
    }
    let v: Value = serde_json::from_str(text.trim()).map_err(|e| {
        let snippet: String = text.chars().take(240).collect();
        Error::server(
            status.as_u16() as i64,
            format!("superfile2 响应不是合法 JSON（{e}）：{snippet}"),
        )
    })?;
    let md5 = v.get("md5").and_then(Value::as_str).unwrap_or("").trim();
    if md5.is_empty() {
        return Err(Error::server(
            status.as_u16() as i64,
            "superfile2 未返回 md5",
        ));
    }
    wp_core::log_debug!("百度", "partseq={seq} {}B → md5={md5}", text.len());
    Ok(md5.to_string())
}

// ───────────────────────────── 上传域名 ─────────────────────────────

async fn fetch_locateupload(d: &BaiduDrive) -> Result<Vec<String>> {
    let resp = d
        .http
        .api
        .get(PCS_FILE)
        .query(&[("method", "locateupload")])
        .header(ACCEPT, "application/json, text/plain, */*")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(net_err)?;
    let status = resp.status();
    let text = resp.text().await.map_err(net_err)?;
    if !status.is_success() {
        return Err(http_status_error(status.as_u16(), &text));
    }
    let v: Value = serde_json::from_str(text.trim()).map_err(|e| {
        Error::server(
            status.as_u16() as i64,
            format!("locateupload 响应不是合法 JSON（{e}）"),
        )
    })?;
    let code = v.get("error_code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        return Err(Error::server(
            code,
            format!(
                "locateupload 失败：{}",
                v.get("error_msg").and_then(Value::as_str).unwrap_or("")
            ),
        ));
    }
    let mut servers: Vec<String> = v
        .get("server")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if let Some(host) = v.get("host").and_then(Value::as_str) {
        let host = host.trim();
        if !host.is_empty() && !servers.iter().any(|s| s == host) {
            servers.push(host.to_string());
        }
    }
    if servers.is_empty() {
        return Err(Error::server(-1, "locateupload 未返回可用上传域名"));
    }
    wp_core::log_debug!(
        "百度",
        "locateupload → server[0]={}（候选 {} 个）",
        servers[0],
        servers.len()
    );
    Ok(servers)
}

async fn ensure_servers(d: &BaiduDrive) -> Vec<String> {
    {
        let guard = lock(&d.domain);
        if let Some(cache) = guard.as_ref() {
            if cache.fetched_at.elapsed() < DOMAIN_TTL && !cache.servers.is_empty() {
                return cache.servers.clone();
            }
        }
    }
    match fetch_locateupload(d).await {
        Ok(servers) => {
            *lock(&d.domain) = Some(DomainCache {
                servers: servers.clone(),
                index: 0,
                fetched_at: Instant::now(),
            });
            servers
        }
        Err(e) => {
            wp_core::log_warn!("百度", "locateupload 失败，使用缓存/兜底域名：{e}");
            let cached = lock(&d.domain)
                .as_ref()
                .map(|c| c.servers.clone())
                .filter(|s| !s.is_empty());
            cached.unwrap_or_else(|| vec![FALLBACK_DOMAIN.to_string()])
        }
    }
}

async fn current_domain(d: &BaiduDrive) -> String {
    let servers = ensure_servers(d).await;
    let index = lock(&d.domain)
        .as_ref()
        .map(|c| c.index.min(servers.len().saturating_sub(1)))
        .unwrap_or(0);
    servers
        .get(index)
        .cloned()
        .unwrap_or_else(|| FALLBACK_DOMAIN.to_string())
}

async fn advance_domain(d: &BaiduDrive) -> String {
    {
        let mut guard = lock(&d.domain);
        if let Some(cache) = guard.as_mut() {
            if cache.index + 1 < cache.servers.len() {
                cache.index += 1;
                return cache.servers[cache.index].clone();
            }
            // 候选耗尽 → 强制刷新
            cache.fetched_at = Instant::now() - DOMAIN_TTL - Duration::from_secs(1);
        }
    }
    let servers = ensure_servers(d).await;
    *lock(&d.domain) = Some(DomainCache {
        servers: servers.clone(),
        index: 0,
        fetched_at: Instant::now(),
    });
    servers
        .first()
        .cloned()
        .unwrap_or_else(|| FALLBACK_DOMAIN.to_string())
}

/// 从本地文件 offset 处读取至多 len 字节（秒传 data_content）。
async fn read_slice(local: &Path, offset: u64, len: u64) -> Result<Vec<u8>> {
    let mut file = tokio::fs::File::open(local).await.map_err(Error::Io)?;
    file.seek(SeekFrom::Start(offset))
        .await
        .map_err(Error::Io)?;
    let mut buf = vec![0u8; len as usize];
    let mut read = 0usize;
    while read < buf.len() {
        let n = file.read(&mut buf[read..]).await.map_err(Error::Io)?;
        if n == 0 {
            break;
        }
        read += n;
    }
    buf.truncate(read);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_md5_known_vectors() {
        assert_eq!(
            encrypt_md5("5eb63bbbe01eeed093cb22bb8f5acdc3"),
            "e13dabb7dn1df6548e7988a41a60ef54"
        );
        assert_eq!(
            encrypt_md5("900150983cd24fb0d6963f7d28e17f72"),
            "3df10ad71paa9d7729c23a155f3df292"
        );
        assert_eq!(
            encrypt_md5("d41d8cd98f00b204e9800998ecf8427e"),
            "8e23f7635tb64136eddb0719602bc477"
        );
        // 非 32 位纯小写 hex 原样返回
        assert_eq!(encrypt_md5("ABC"), "ABC");
        assert_eq!(
            encrypt_md5("D41D8CD98F00B204E9800998ECF8427E"),
            "D41D8CD98F00B204E9800998ECF8427E"
        );
    }

    #[test]
    fn chunk_ranges_for_9mib() {
        assert_eq!(
            chunk_ranges(9 * 1024 * 1024, CHUNK_SIZE),
            vec![
                (0, 4_194_304),
                (4_194_304, 4_194_304),
                (8_388_608, 1_048_576)
            ]
        );
        assert_eq!(chunk_ranges(0, CHUNK_SIZE), vec![(0, 0)]);
        assert_eq!(chunk_ranges(1, CHUNK_SIZE), vec![(0, 1)]);
        assert_eq!(
            chunk_ranges(4 * 1024 * 1024, CHUNK_SIZE),
            vec![(0, 4_194_304)]
        );
    }

    #[test]
    fn rapid_offset_matches_python_formula() {
        // Python: md5("455281002" + enc + "1789325407")[:8] % (1000000 - 262144 + 1)
        assert_eq!(
            rapid_offset(
                "455281002",
                "e13dabb7dn1df6548e7988a41a60ef54",
                1_789_325_407,
                1_000_000
            ),
            643_575
        );
        assert_eq!(
            rapid_offset(
                "455281002",
                "e13dabb7dn1df6548e7988a41a60ef54",
                1_700_000_000,
                3_000_000
            ),
            996_719
        );
        // < 256KiB / 缺 uk → 0
        assert_eq!(rapid_offset("1", "x", 1, 100), 0);
        assert_eq!(rapid_offset("", "x", 1, 1_000_000), 0);
    }

    #[tokio::test]
    async fn hashes_are_computed_in_one_pass() {
        let path = std::env::temp_dir().join(format!(
            "wp_baidu_hash_{}.bin",
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::write(&path, b"hello world").await.unwrap();
        let mut progress: ProgressFn = Box::new(|done, total| {
            assert!(done <= total);
            true
        });
        let hashes = compute_hashes(&path, 11, &mut progress).await.unwrap();
        let _ = tokio::fs::remove_file(&path).await;
        assert_eq!(hashes.size, 11);
        assert_eq!(hashes.content_md5, "5eb63bbbe01eeed093cb22bb8f5acdc3");
        assert_eq!(hashes.slice_md5, "5eb63bbbe01eeed093cb22bb8f5acdc3");
        assert_eq!(hashes.chunk_md5, vec!["5eb63bbbe01eeed093cb22bb8f5acdc3"]);
        assert_eq!(hashes.content_md5_enc, "e13dabb7dn1df6548e7988a41a60ef54");
    }

    #[tokio::test]
    async fn cancellation_is_reported() {
        let path = std::env::temp_dir().join(format!(
            "wp_baidu_cancel_{}.bin",
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::write(&path, b"abc").await.unwrap();
        let mut progress: ProgressFn = Box::new(|_, _| false);
        let err = compute_hashes(&path, 3, &mut progress).await.unwrap_err();
        let _ = tokio::fs::remove_file(&path).await;
        assert!(err.is_canceled());
    }
}
