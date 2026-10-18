//! 夸克云端转码播放（大视频 HLS 链路）。
//!
//! # 背景
//!
//! `POST file/download` 对单文件 >约 100MB 返回业务码 **23018**
//! `download file size limit`（实测 50.6MB 成功；108MB/135MB/173MB 失败）。
//! 官方 Web 端遇到 23018 同样弹「下载文件过大」并引导使用客户端，
//! 因此这不是参数问题：**免费/会员都无法通过下载直链拿大文件**。
//!
//! 但官方 Web 播放器走的是另一条链路：`POST file/v2/play` 取云端转码 HLS，
//! 不受 100MB 下载限制（证据与探测矩阵见 `docs/PLAYER_QUARK_STREAM.md`）。
//! 本模块实现这条链路，供 [`crate::download::QuarkDrive::direct_link`] 在
//! 大视频/23018 时自动切换，返回 `is_hls = true` 的 [`DirectLink`]。
//!
//! # 关键约束（真机实测）
//!
//! * 首次播放若云端尚未转码，返回 `400/code=21002`「video is transcoding」，
//!   需客户端内轮询（本模块按 1/2/3/5… 秒退避，总等待上限约 26 秒）；
//! * m3u8 与分片都有防盗链：请求必须带**完整 Cookie**（只带 UA/Referer 返回
//!   HTTP 412），因此 [`DirectLink::headers`] 必须原样透传给播放器；
//! * 转码产物是 m3u8（h264+aac，分片 MPEG-TS），`auth_key` 有效期 3 小时，
//!   失效后重新调本接口即可换链；
//! * 档位由 `video_list[].resolution` 决定（如 `super`/`high`/`2k`/`4k`），
//!   账号会员等级决定能拿到哪些档位；具体档位以接口返回为准。

use crate::{as_bool, as_str, as_u64, data_of, QuarkDrive};
use serde_json::{json, Value};
use std::time::Duration;
use wp_core::model::{DirectLink, RemoteEntry};
use wp_core::{Error, Result};

/// 单文件直链下载上限（服务端 23018）。实测 50.6MB 可、108MB 起不可。
/// 仅用于「多大体积优先走转码」的启发式；真正的判定以服务端返回为准。
pub(crate) const DOWNLOAD_SIZE_LIMIT_BYTES: u64 = 100 * 1024 * 1024;

/// 播放接口：云端转码中（首次播放需等待，稍后重试即可）。
pub(crate) const CODE_TRANSCODING: i64 = 21002;
/// 播放接口：不是视频文件（非视频大文件没有转码链路）。
pub(crate) const CODE_NOT_VIDEO: i64 = 21005;
/// 播放接口：视频不存在 / 无法播放 / 风险文件 / 转码失败（官方 Web 端
/// 21112 的文案是「暂不支持在线播放，请下载到本地播放」）。
pub(crate) const CODE_PLAY_NOT_FOUND: i64 = 21001;
pub(crate) const CODE_PLAY_INVALID: i64 = 21003;
pub(crate) const CODE_PLAY_RISKY: i64 = 22001;
pub(crate) const CODE_PLAY_FAILED: i64 = 21112;
/// 播放接口：请求的画质档位不可用（服务端只接受实际可产出的档位）。
pub(crate) const CODE_RESOLUTION_INVALID: i64 = 21111;

/// 请求的画质档位（低→高；服务端只返回实际可用的档位）。
pub(crate) const PLAY_RESOLUTIONS: &str = "low,normal,high,super,2k,4k";
/// 官方 Web 播放器（4.6.7）使用的容器支持声明。
pub(crate) const PLAY_SUPPORTS: &str = "fmp4,m3u8";
/// `21002` 轮询计划（毫秒）：首次请求立即发，之后 1/2/3/5/5/5/5 秒，
/// 累计等待约 26 秒；仍转码中则返回可读错误，不做无限重试。
pub(crate) const PLAY_RETRY_DELAYS_MS: &[u64] = &[1_000, 2_000, 3_000, 5_000, 5_000, 5_000, 5_000];

/// 高清档位排序（数值越大越好），未知档位排最低。
fn resolution_rank(resolution: &str) -> u8 {
    match resolution.trim().to_ascii_lowercase().as_str() {
        "4k" => 6,
        "2k" => 5,
        "super" => 4,
        "high" => 3,
        "normal" => 2,
        "low" => 1,
        _ => 0,
    }
}

/// 播放接口解析出的播放信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlayInfo {
    /// m3u8 地址（带签名，3 小时有效）。
    pub url: String,
    /// 服务端实际返回的档位名（`super` / `high` / `2k` / `4k` …）。
    pub resolution: String,
    /// 从 `auth_key` 解析出的过期时间（Unix 秒）；解析不到为 None。
    pub expire_at: Option<i64>,
    /// 原文件大小（接口 `data.size`，不是转码产物大小）。
    pub size: u64,
    /// 文件名（接口 `data.file_name`）。
    pub file_name: String,
}

/// 播放接口的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlayParse {
    Ready(PlayInfo),
    /// 视频还在云端转码中，可稍后重试。
    Transcoding,
    /// 结构异常（缺 `video_list` 等），按可读错误处理。
    Invalid(String),
}

/// 一次播放请求的结果分类（供轮询逻辑消费）。
enum Attempt {
    Transcoding,
    Unavailable(Error),
    Fatal(Error),
}

/// 视频文件判定：优先用 `file/sort` 的 `obj_category`，缺失时按扩展名兜底。
pub(crate) fn is_video_entry(entry: &RemoteEntry) -> bool {
    if let Some(raw) = &entry.raw {
        for key in ["obj_category", "category_name", "file_category"] {
            if let Some(cat) = as_str(raw.get(key)) {
                let cat = cat.trim().to_ascii_lowercase();
                if !cat.is_empty() {
                    return cat == "video";
                }
            }
        }
    }
    is_video_name(&entry.name)
}

/// 按扩展名判断是否视频（小写、忽略查询串/结尾空格）。
pub(crate) fn is_video_name(name: &str) -> bool {
    let name = name.trim();
    let ext = name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "mp4"
            | "mkv"
            | "mov"
            | "avi"
            | "wmv"
            | "flv"
            | "webm"
            | "m4v"
            | "ts"
            | "m2ts"
            | "mts"
            | "rmvb"
            | "rm"
            | "mpg"
            | "mpeg"
            | "3gp"
            | "ogv"
            | "vob"
            | "f4v"
            | "divx"
    )
}

/// 从 m3u8 的 `auth_key=<过期Unix秒>-<随机>-<ttl>-<签名>` 解析过期时间。
pub(crate) fn parse_auth_key_expiry(url: &str) -> Option<i64> {
    let rest = url.split("auth_key=").nth(1)?;
    let first = rest.split(['&', '-']).next()?;
    let ts = first.parse::<i64>().ok()?;
    // 合理值域粗校验（2001-01-01 之后），避免把别的参数误当时间戳。
    if ts > 978_307_200 {
        Some(ts)
    } else {
        None
    }
}

/// 解析 `file/v2/play` 的 `data`（见文件头注释的响应样例）。
pub(crate) fn parse_play_data(data: &Value) -> PlayParse {
    let Some(list) = data.get("video_list").and_then(Value::as_array) else {
        return PlayParse::Invalid("播放接口响应缺少 video_list".into());
    };
    let mut best: Option<(u8, PlayInfo)> = None;
    let mut saw_transcoding = false;
    for item in list {
        let trans = as_str(item.get("trans_status"))
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if trans == "transcoding" || trans == "doing" {
            saw_transcoding = true;
            continue;
        }
        let info = item.get("video_info").unwrap_or(&Value::Null);
        let url = as_str(info.get("url")).unwrap_or_default();
        // 官方 Web 播放器的判定：有 url 且 trans_status 不是 transcoding 才可用。
        if url.trim().is_empty() {
            continue;
        }
        if !trans.is_empty() && trans != "success" {
            continue;
        }
        // 会员等级不足的档位服务端会标记 accessable=false，跳过。
        if as_bool(item.get("accessable")) == Some(false) {
            continue;
        }
        let resolution = as_str(item.get("resolution"))
            .filter(|s| !s.trim().is_empty())
            .or_else(|| as_str(info.get("resolution")))
            .unwrap_or_default();
        let parsed = PlayInfo {
            expire_at: parse_auth_key_expiry(&url),
            url,
            resolution: resolution.clone(),
            size: as_u64(data.get("size")).unwrap_or(0),
            file_name: as_str(data.get("file_name")).unwrap_or_default(),
        };
        let rank = resolution_rank(&resolution);
        if best.as_ref().map(|(r, _)| rank > *r).unwrap_or(true) {
            best = Some((rank, parsed));
        }
    }
    if let Some((_, info)) = best {
        return PlayParse::Ready(info);
    }
    if saw_transcoding {
        return PlayParse::Transcoding;
    }
    PlayParse::Invalid("播放接口未返回可用视频流（可能转码失败或该视频不支持在线播放）".into())
}

/// 把播放接口的 400 错误分类：转码中可重试，其余给出可读业务错误。
fn classify_play_error(err: Error) -> Attempt {
    if let Error::Server { code, ref msg } = err {
        match code {
            CODE_TRANSCODING => return Attempt::Transcoding,
            CODE_NOT_VIDEO => {
                return Attempt::Unavailable(Error::unsupported(format!(
                    "夸克：该文件不是视频，没有云端转码播放链路（服务端 {code}：{msg}）"
                )))
            }
            CODE_PLAY_NOT_FOUND | CODE_PLAY_INVALID | CODE_PLAY_RISKY | CODE_PLAY_FAILED => {
                return Attempt::Unavailable(play_unavailable_error(code, msg))
            }
            _ => {}
        }
    }
    Attempt::Fatal(err)
}

/// 视频无法在线播放时的可读错误（区别于网络错误，UI 可直接展示）。
fn play_unavailable_error(code: i64, msg: &str) -> Error {
    Error::unsupported(format!(
        "夸克：该视频暂不支持在线播放（服务端 {code}：{msg}）；\
         文件超过 100MB 时直链下载也不可用，请用夸克网盘客户端播放或下载"
    ))
}

/// 转码等待超时可读错误。
pub(crate) fn transcode_timeout_error() -> Error {
    Error::server(
        CODE_TRANSCODING,
        format!(
            "夸克视频正在云端转码（首次播放需转码，通常十几秒，长视频更久），\
             已等待约 {} 秒仍未就绪，请稍后重试",
            PLAY_RETRY_DELAYS_MS.iter().sum::<u64>() / 1000
        ),
    )
}

/// 下载直链过大（23018）的可读错误。
///
/// 视频有云端转码播放兜底；非视频只能提示用客户端。
pub(crate) fn download_limit_error(entry: &RemoteEntry, is_video: bool) -> Error {
    let size_mb = entry.size as f64 / (1024.0 * 1024.0);
    let tip = if is_video {
        "该视频超过 100MB：直链下载被服务端拒绝（23018），且在线转码播放不可用；\
         请用夸克网盘客户端播放/下载"
    } else {
        "文件超过 100MB：夸克直链下载被服务端拒绝（23018 download file size limit），\
         请用夸克网盘客户端下载"
    };
    Error::unsupported(format!("夸克限制：{tip}（{}，{size_mb:.1}MB）", entry.name))
}

/// 是否属于「大文件直链被拒」错误（服务端业务码 23018）。
pub(crate) fn is_download_limit_error(err: &Error) -> bool {
    matches!(err, Error::Server { code: 23018, .. })
}

/// 是否属于「该视频没有在线播放链路」的可读错误（大视频时可退化到直链尝试）。
pub(crate) fn is_play_unavailable(err: &Error) -> bool {
    match err {
        Error::Unsupported(_) => true,
        Error::Server { code, .. } => matches!(
            *code,
            CODE_NOT_VIDEO
                | CODE_PLAY_NOT_FOUND
                | CODE_PLAY_INVALID
                | CODE_PLAY_RISKY
                | CODE_PLAY_FAILED
                | CODE_RESOLUTION_INVALID
        ),
        _ => false,
    }
}

/// 用播放信息构建 HLS 版 [`DirectLink`]。
///
/// `headers` 必须是完整下载头（Cookie/UA/Referer）——m3u8 与分片都校验 Cookie，
/// 缺了会得到 HTTP 412。
pub(crate) fn hls_direct_link(
    entry: &RemoteEntry,
    info: &PlayInfo,
    headers: Vec<(String, String)>,
) -> DirectLink {
    DirectLink {
        url: info.url.clone(),
        headers,
        expire_at: info.expire_at,
        size: if info.size > 0 { info.size } else { entry.size },
        file_name: if info.file_name.is_empty() {
            entry.name.clone()
        } else {
            info.file_name.clone()
        },
        is_hls: true,
    }
}

/// 单次 `file/v2/play` 请求体（官方 Web 4.6.7 同款参数）。
fn play_body(fid: &str) -> Value {
    json!({
        "fid": fid,
        "resolutions": PLAY_RESOLUTIONS,
        "supports": PLAY_SUPPORTS,
    })
}

/// 带转码轮询的播放请求：`21002`/转码中按 `delays` 退避重试，
/// 其余错误立即返回；超时给出可读错误。
///
/// 抽成泛型函数便于单测注入「先 21002 后成功」的假服务端。
pub(crate) async fn play_with_retry<F, Fut>(mut fetch: F, delays: &[u64]) -> Result<PlayInfo>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    for round in 0..=delays.len() {
        if round > 0 {
            tokio::time::sleep(Duration::from_millis(delays[round - 1])).await;
        }
        let attempt = match fetch().await {
            Ok(resp) => match parse_play_data(&data_of(&resp)) {
                PlayParse::Ready(info) => return Ok(info),
                PlayParse::Transcoding => Attempt::Transcoding,
                PlayParse::Invalid(msg) => Attempt::Unavailable(Error::unsupported(format!(
                    "夸克：{msg}；文件超过 100MB 时直链下载也不可用，请用客户端播放/下载"
                ))),
            },
            Err(e) => classify_play_error(e),
        };
        match attempt {
            Attempt::Transcoding => continue,
            Attempt::Unavailable(e) | Attempt::Fatal(e) => return Err(e),
        }
    }
    Err(transcode_timeout_error())
}

impl QuarkDrive {
    /// 取大视频的云端转码 HLS 直链（首次播放可能等待转码，最长约 26 秒）。
    pub(crate) async fn fetch_play_link(&self, entry: &RemoteEntry) -> Result<DirectLink> {
        self.api.ensure_valid()?;
        match play_with_retry(
            || self.api.post("file/v2/play", play_body(&entry.id)),
            PLAY_RETRY_DELAYS_MS,
        )
        .await
        {
            Ok(info) => Ok(self.finish_play_link(entry, info)),
            // 21111：请求的画质档位不被接受（服务端只认实际可产出档位），
            // 兜底改成不指定档位，让服务端给默认档位。
            Err(Error::Server {
                code: CODE_RESOLUTION_INVALID,
                ..
            }) => {
                wp_core::log_debug!("夸克", "画质档位被拒，改用默认档位重试：{}", entry.name);
                let info = play_with_retry(
                    || {
                        self.api.post(
                            "file/v2/play",
                            json!({"fid": entry.id, "supports": PLAY_SUPPORTS}),
                        )
                    },
                    PLAY_RETRY_DELAYS_MS,
                )
                .await?;
                Ok(self.finish_play_link(entry, info))
            }
            Err(e) => Err(e),
        }
    }

    fn finish_play_link(&self, entry: &RemoteEntry, info: PlayInfo) -> DirectLink {
        wp_core::log_debug!(
            "夸克",
            "云端转码播放就绪：{}（{}，{}s 有效）",
            entry.name,
            info.resolution,
            info.expire_at
                .map(|t| t - crate::now_ts())
                .unwrap_or_default()
        );
        hls_direct_link(entry, &info, self.download_headers())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn entry(name: &str, size: u64, raw: Option<Value>) -> RemoteEntry {
        RemoteEntry {
            id: "fid1".into(),
            name: name.into(),
            path: format!("/{name}"),
            is_dir: false,
            size,
            modified: None,
            created: None,
            thumbnail: None,
            raw,
        }
    }

    fn play_data() -> Value {
        json!({
            "default_resolution": "super",
            "size": 173_709_677u64,
            "file_name": "video_20260903_171534.mp4",
            "video_list": [
                {
                    "resolution": "high",
                    "trans_status": "success",
                    "accessable": true,
                    "video_info": {
                        "url": "https://cdn.example/qv/x/media.m3u8?auth_key=1792356237-16048-10800-abc&sp=150",
                        "format": "m3u8"
                    }
                },
                {
                    "resolution": "super",
                    "trans_status": "success",
                    "accessable": true,
                    "video_info": {
                        "url": "https://cdn.example/qv/y/media.m3u8?auth_key=1792356237-31759-10800-def&sp=312",
                        "format": "m3u8"
                    }
                }
            ]
        })
    }

    #[test]
    fn video_detection_prefers_obj_category_then_extension() {
        let raw_video = json!({"obj_category": "video", "format_type": ""});
        assert!(is_video_entry(&entry("note.txt", 10, Some(raw_video)),));
        let raw_doc = json!({"obj_category": "doc"});
        assert!(!is_video_entry(&entry("movie.mp4", 10, Some(raw_doc))));
        assert!(is_video_entry(&entry("movie.MP4", 10, None)));
        assert!(is_video_entry(&entry("电影.mkv", 10, None)));
        assert!(!is_video_entry(&entry("archive.7z", 10, None)));
        assert!(!is_video_entry(&entry("README", 10, None)));
        assert!(is_video_name("a.b.m2ts"));
        assert!(!is_video_name("x.mp4.part"));
    }

    #[test]
    fn auth_key_expiry_parsed_from_real_url_shape() {
        let url = "https://video-play-h-sz.drive.quark.cn/qv/A8129E26/media.m3u8\
                   ?auth_key=1792356237-31759-10800-1c802bd4&sp=312&token=4-2-2";
        assert_eq!(parse_auth_key_expiry(url), Some(1_792_356_237));
        assert_eq!(parse_auth_key_expiry("https://cdn/x.m3u8"), None);
        assert_eq!(
            parse_auth_key_expiry("https://cdn/x.m3u8?auth_key=abc-def"),
            None
        );
    }

    #[test]
    fn parse_play_picks_best_accessible_resolution() {
        let play = parse_play_data(&play_data());
        match play {
            PlayParse::Ready(info) => {
                assert_eq!(info.resolution, "super");
                assert!(info.url.contains("auth_key=1792356237"));
                assert_eq!(info.expire_at, Some(1_792_356_237));
                assert_eq!(info.size, 173_709_677);
                assert_eq!(info.file_name, "video_20260903_171534.mp4");
            }
            other => panic!("期望 Ready，实际 {other:?}"),
        }
    }

    #[test]
    fn parse_play_skips_inaccessible_and_transcoding_items() {
        let data = json!({
            "size": 10u64, "file_name": "a.mp4",
            "video_list": [
                {"resolution": "4k", "trans_status": "success", "accessable": false,
                 "video_info": {"url": "https://cdn/4k.m3u8"}},
                {"resolution": "super", "trans_status": "transcoding",
                 "video_info": {"url": ""}},
                {"resolution": "high", "trans_status": "success",
                 "video_info": {"url": "https://cdn/high.m3u8"}}
            ]
        });
        match parse_play_data(&data) {
            PlayParse::Ready(info) => assert_eq!(info.resolution, "high"),
            other => panic!("期望 Ready(high)，实际 {other:?}"),
        }
        // 全部转码中 → Transcoding（用于轮询）。
        let data = json!({"video_list": [
            {"resolution": "super", "trans_status": "transcoding", "video_info": {"url": ""}}
        ]});
        assert_eq!(parse_play_data(&data), PlayParse::Transcoding);
        // 结构异常 → Invalid。
        assert!(matches!(
            parse_play_data(&json!({"foo": 1})),
            PlayParse::Invalid(_)
        ));
    }

    #[test]
    fn hls_direct_link_marks_is_hls_and_keeps_original_meta() {
        let info = PlayInfo {
            url: "https://cdn/x.m3u8?auth_key=1792356237-1-10800-s".into(),
            resolution: "super".into(),
            expire_at: Some(1_792_356_237),
            size: 173_709_677,
            file_name: "video.mp4".into(),
        };
        let e = entry("video.mp4", 100, None);
        let headers = vec![("cookie".to_string(), "k=v".to_string())];
        let link = hls_direct_link(&e, &info, headers.clone());
        assert!(link.is_hls);
        assert_eq!(link.url, info.url);
        assert_eq!(link.headers, headers);
        assert_eq!(link.expire_at, Some(1_792_356_237));
        assert_eq!(link.size, 173_709_677);
        assert_eq!(link.file_name, "video.mp4");
        // 接口没给 size/文件名时回退到原条目。
        let info2 = PlayInfo {
            size: 0,
            file_name: String::new(),
            ..info
        };
        let link = hls_direct_link(&e, &info2, vec![]);
        assert_eq!(link.size, 100);
        assert_eq!(link.file_name, "video.mp4");
    }

    #[test]
    fn error_classification() {
        assert!(is_download_limit_error(&Error::server(
            23018,
            "download file size limit[fid]"
        )));
        assert!(!is_download_limit_error(&Error::server(
            21002,
            "transcoding"
        )));
        assert!(!is_download_limit_error(&Error::network("timeout")));

        let limit = download_limit_error(&entry("big.7z", 108_954_019, None), false);
        assert_eq!(limit.kind(), wp_core::ErrorKind::Unsupported);
        assert!(limit.to_string().contains("100MB"), "错误需可读：{limit}");
        assert!(limit.to_string().contains("客户端"));
        let video_limit = download_limit_error(&entry("big.mp4", 172_594_093, None), true);
        assert!(video_limit.to_string().contains("在线转码播放"));

        // 21005 非视频 → 可读「不是视频」错误；21002 归为转码中。
        match classify_play_error(Error::server(21005, "not video")) {
            Attempt::Unavailable(e) => {
                assert_eq!(e.kind(), wp_core::ErrorKind::Unsupported);
                assert!(e.to_string().contains("不是视频"));
            }
            _ => panic!("21005 应映射为可读 Unavailable"),
        }
        assert!(matches!(
            classify_play_error(Error::server(21002, "video is transcoding")),
            Attempt::Transcoding
        ));
        assert!(matches!(
            classify_play_error(Error::network("connect timeout")),
            Attempt::Fatal(_)
        ));
        assert!(is_play_unavailable(&play_unavailable_error(21112, "fail")));
    }

    #[tokio::test]
    async fn retry_21002_then_success() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        let info = play_with_retry(
            move || {
                let calls = calls2.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        // 服务端在转码中：HTTP 400 + code 21002。
                        Err(Error::server(21002, "video is transcoding [转码中]"))
                    } else {
                        Ok(json!({"status": 200, "code": 0, "data": play_data()}))
                    }
                }
            },
            &[0, 0, 0],
        )
        .await
        .expect("21002 后应重试成功");
        assert_eq!(info.resolution, "super");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_timeout_returns_readable_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        let err = play_with_retry(
            move || {
                let calls = calls2.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err::<Value, Error>(Error::server(21002, "video is transcoding"))
                }
            },
            &[0, 0],
        )
        .await
        .expect_err("一直转码应超时");
        assert_eq!(calls.load(Ordering::SeqCst), 3, "应尝试 1+2 次");
        assert!(matches!(err, Error::Server { code: 21002, .. }));
        assert!(err.to_string().contains("转码"), "错误需可读：{err}");
        assert!(!err.to_string().contains("网络错误"));
    }

    #[tokio::test]
    async fn retry_does_not_loop_on_fatal_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        let err = play_with_retry(
            move || {
                let calls = calls2.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err::<Value, Error>(Error::network("connect reset"))
                }
            },
            &[0, 0],
        )
        .await
        .expect_err("网络错误不重试");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(err.kind(), wp_core::ErrorKind::Network);
    }
}
