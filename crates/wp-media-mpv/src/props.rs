//! mpv 属性与信令的**纯逻辑**层：JSON 解析、参数拼装、时间换算。
//!
//! 这里不出现任何 FFI / 全局状态，因此可以完整单测（引擎线程只负责把
//! `mpv_get_property` 拿到的 JSON 交给这些函数）。

use serde_json::Value;
use wp_media::{BufferedRange, MediaSource, TrackInfo, TrackKind};

use crate::{MpvConfig, RenderMode};

/// 需要观察的 mpv 属性。
///
/// 观察使用 `MPV_FORMAT_NONE`（只收「变了」通知，不读事件里的临时数据），
/// 收到通知后再用 `get_property` 读取当前值——这样字符串/节点数据的生命周期
/// 完全由我们掌控，也不会因为某个属性在旧版本不存在而崩。
///
/// 不存在的属性（例如旧版 mpv 没有 `secondary-sub-text` / `disc-menu-active`）
/// 观察失败会被静默跳过，属于设计内行为。
pub const OBSERVED_PROPERTIES: &[&str] = &[
    // 核心播放状态
    "time-pos",
    "duration",
    "pause",
    "eof-reached",
    "idle-active",
    "seeking",
    "paused-for-cache",
    "cache-buffering-state",
    // 轨道/字幕（AI、字幕面板、弹幕对齐都要用）
    "track-list",
    "sub-text",
    "sub-start",
    "secondary-sub-text",
    "secondary-sub-start",
    // 画面/性能诊断
    "hwdec-current",
    "video-params",
    "vo-configured",
    "frame-drop-count",
    "demuxer-cache-state",
    // 蓝光（0.41 可能无此属性，观察失败即跳过）
    "disc-menu-active",
    // 用户可调项（其它入口改了也要让 UI 看到）
    "speed",
    "volume",
    "mute",
];

/// 观察 `time-pos` 时的事件风暴下限（秒）：位置变化小于该值不派发事件。
/// 4K 播放时 mpv 的通知频率受 `time-pos` 本身节流，这里只做兜底。
pub const POSITION_EVENT_MIN_DELTA_SECS: f64 = 0.01;

// ============================ 轨道解析 ============================

/// 解析 `track-list` 属性（mpv 的 track-list 是 mpv_node 数组）。
///
/// 字段对照 mpv 文档：`id/type/title/lang/codec/default/selected/external`。
/// `type` 为 `image` 之类时跳过（播放器 UI 只关心 视频/音频/字幕）。
pub fn parse_track_list(value: &Value) -> Vec<TrackInfo> {
    let Some(arr) = value.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let Some(kind) = obj
            .get("type")
            .and_then(Value::as_str)
            .and_then(track_kind_from_mpv)
        else {
            continue;
        };
        out.push(TrackInfo {
            id: obj.get("id").and_then(Value::as_i64).unwrap_or(0),
            kind: Some(kind),
            title: str_field(obj, "title"),
            lang: str_field(obj, "lang"),
            codec: str_field(obj, "codec"),
            default: bool_field(obj, "default"),
            selected: bool_field(obj, "selected"),
            external: bool_field(obj, "external"),
        });
    }
    out
}

/// mpv 的轨道类型字符串 → [`TrackKind`]。
pub fn track_kind_from_mpv(kind: &str) -> Option<TrackKind> {
    match kind {
        "video" => Some(TrackKind::Video),
        "audio" => Some(TrackKind::Audio),
        "sub" => Some(TrackKind::Subtitle),
        _ => None,
    }
}

/// [`TrackKind`] → mpv 的轨道属性名（`vid`/`aid`/`sid`）。
pub fn track_kind_property(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Video => "vid",
        TrackKind::Audio => "aid",
        TrackKind::Subtitle => "sid",
    }
}

fn str_field(obj: &serde_json::Map<String, Value>, key: &str) -> String {
    obj.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn bool_field(obj: &serde_json::Map<String, Value>, key: &str) -> bool {
    obj.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// 从轨道列表里找出当前选中的轨道 id（按类型）。
pub fn selected_track_id(tracks: &[TrackInfo], kind: TrackKind) -> Option<i64> {
    tracks
        .iter()
        .find(|t| t.kind == Some(kind) && t.selected)
        .map(|t| t.id)
}

// ============================ 画面/缓存解析 ============================

/// 解析 `video-params`，取解码后显示尺寸 `(w, h)`。
pub fn parse_video_params(value: &Value) -> Option<(u32, u32)> {
    let obj = value.as_object()?;
    let w = obj.get("w").and_then(Value::as_u64)?;
    let h = obj.get("h").and_then(Value::as_u64)?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w.min(u32::MAX as u64) as u32, h.min(u32::MAX as u64) as u32))
}

/// 解析 `demuxer-cache-state` → 已缓存区间。
///
/// 优先用 `seekable-ranges`（真实可 seek 的区间）；没有时退回
/// 「`bof-cached` + `cache-end`」这一条近似区间。
pub fn parse_demuxer_cache_state(value: &Value) -> Vec<BufferedRange> {
    let Some(obj) = value.as_object() else {
        return Vec::new();
    };
    let mut ranges = Vec::new();
    if let Some(list) = obj.get("seekable-ranges").and_then(Value::as_array) {
        for r in list {
            let Some(r) = r.as_object() else { continue };
            let Some(start) = r.get("start").and_then(Value::as_f64) else {
                continue;
            };
            let Some(end) = r.get("end").and_then(Value::as_f64) else {
                continue;
            };
            if end > start {
                ranges.push(BufferedRange {
                    start_secs: start,
                    end_secs: end,
                });
            }
        }
    }
    if ranges.is_empty() && bool_field(obj, "bof-cached") {
        if let Some(end) = obj.get("cache-end").and_then(Value::as_f64) {
            if end > 0.0 {
                ranges.push(BufferedRange {
                    start_secs: 0.0,
                    end_secs: end,
                });
            }
        }
    }
    ranges
}

// ============================ 网络/蓝光源参数 ============================

/// 把 `NetworkSource.headers` 转成 mpv 的 **list 选项每一项**（推荐用法）。
///
/// `--http-header-fields` 是 list 选项：用字符串以逗号拼接会被 list 解析器拆碎
/// （header 值本身含逗号时出错）。正确做法是通过 client API 传
/// `MPV_FORMAT_NODE_ARRAY`（见 `ffi::RawMpv::set_property_string_list`），
/// 每个元素一个 `"Name: value"`。
pub fn http_header_fields_list(headers: &[(String, String)]) -> Vec<String> {
    headers
        .iter()
        .filter(|(name, _)| !name.trim().is_empty())
        .filter(|(_, value)| !value.contains('\0'))
        .map(|(name, value)| format!("{name}: {value}"))
        .collect()
}

/// 从 headers 里取 `User-Agent`（HLS 走 FFmpeg 时 UA 要用 `--user-agent` 选项，
/// 只放 header list 里 FFmpeg 也会用，但显式设置更稳）。
pub fn user_agent_header(headers: &[(String, String)]) -> Option<String> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
        .map(|(_, value)| value.clone())
}

/// 把 `NetworkSource.headers` 拼成 mpv 的 `--http-header-fields` 选项值。
///
/// mpv 用逗号分隔多个 header；header 值里出现逗号会被拆错，
/// 因此这里对含逗号/SOH 的值直接跳过并返回 `Ok(None)`——网盘直链的
/// `Cookie/Referer/User-Agent/Authorization` 通常都不含逗号。
pub fn http_header_fields(headers: &[(String, String)]) -> Option<String> {
    let fields: Vec<String> = headers
        .iter()
        .filter(|(name, _)| !name.trim().is_empty())
        .filter(|(_, value)| !value.contains(',') && !value.contains('\0'))
        .map(|(name, value)| format!("{name}: {value}"))
        .collect();
    if fields.is_empty() {
        None
    } else {
        Some(fields.join(","))
    }
}

/// 打开媒体源前需要设置的 mpv 选项（http 头、蓝光设备等）。
pub fn source_options(source: &MediaSource) -> Vec<(String, String)> {
    let mut opts = Vec::new();
    match source {
        MediaSource::Local(_) => {}
        MediaSource::Network(_ns) => {
            // 注意：请求头不在这里拼字符串——由 engine 用 `http-header-fields` 的
            // MPV_FORMAT_NODE_ARRAY 形式逐项设置（见 props::http_header_fields_list）。
            // 网盘流经本地代理，缓存窗口给大一点，抗上游抖动。
            opts.push(("cache".into(), "yes".into()));
            opts.push(("cache-secs".into(), "60".into()));
            opts.push(("demuxer-max-bytes".into(), "200MiB".into()));
            opts.push(("demuxer-max-back-bytes".into(), "100MiB".into()));
        }
        MediaSource::BluRay(br) => {
            opts.push(("bluray-device".into(), br.iso_or_dir.display().to_string()));
            // LGPL 构建无 GPL 的 dvdnav/蓝光菜单；主标题播放不需要菜单即可。
            opts.push(("bluray-angle".into(), "1".into()));
        }
    }
    opts
}

/// 拼出 mpv 的 `loadfile` 命令参数；`select_disc_title` 用于蓝光切 title。
pub fn loadfile_args(source: &MediaSource) -> Vec<String> {
    match source {
        MediaSource::Local(path) => vec![
            "loadfile".into(),
            path.display().to_string(),
            "replace".into(),
        ],
        MediaSource::Network(ns) => vec!["loadfile".into(), ns.url.clone(), "replace".into()],
        MediaSource::BluRay(br) => {
            // mpv 蓝光协议：bd:// 播主标题；bd://title/<n> 播指定 title。
            // 注意：LGPL 构建下仅支持未加密/已解密原盘的主标题播放。
            let target = match br.title_id {
                Some(0) | None => "bd://".to_string(),
                Some(t) => format!("bd://title/{t}"),
            };
            vec!["loadfile".into(), target, "replace".into()]
        }
    }
}

// ============================ mpv 选项拼装 ============================

/// 把 `MpvConfig` 拼成初始化前要设置的 mpv 选项表（保持顺序，后者覆盖前者）。
pub fn build_options(config: &MpvConfig) -> Vec<(String, String)> {
    let mut opts: Vec<(String, String)> = vec![
        // 嵌入式播放：不弹终端/OSD/默认快捷键，窗口由 eframe 提供
        ("terminal".into(), "no".into()),
        ("msg-level".into(), "all=warn".into()),
        ("input-default-bindings".into(), "no".into()),
        ("input-vo-keyboard".into(), "no".into()),
        ("osc".into(), "no".into()),
        ("osd-level".into(), "0".into()),
        ("idle".into(), "yes".into()),
        ("keep-open".into(), "yes".into()),
        ("force-window".into(), "no".into()),
        // 本地/代理流都走 mpv 内部缓存；代理模式下这里只是二次缓冲
        ("cache".into(), "yes".into()),
        ("cache-secs".into(), config.network_cache_secs.to_string()),
        ("demuxer-max-bytes".into(), "200MiB".into()),
        ("demuxer-max-back-bytes".into(), "100MiB".into()),
        // 字幕：外挂字幕由上层显式注入，先不做同名自动加载（E5 再开 fuzzy）
        ("sub-auto".into(), "no".into()),
        // 截图控制
        ("screenshot-format".into(), "png".into()),
        (
            "screenshot-directory".into(),
            std::env::temp_dir().display().to_string(),
        ),
        // 音频设备名（PipeWire/PulseAudio 里显示成「网盘管理」）
        ("audio-client-name".into(), "网盘管理".into()),
    ];
    match config.render_mode {
        RenderMode::EmbeddedGl => {
            // render API 的硬性要求：vo 必须是 libmpv，画面交给上层 GL。
            opts.push(("vo".into(), "libmpv".into()));
            // 注意：**不要**在 render context 建立前设置 video-sync=display-resample：
            // 它依赖 report_swap 的显示时间戳，首帧前会卡住视频初始化（M0-E2 实测）。
            // 帧同步调优留到 T6（拿到首帧后动态 set property）。
        }
        RenderMode::ExternalWindow => {
            opts.push(("vo".into(), "gpu".into()));
        }
    }
    opts.push(("hwdec".into(), config.hwdec.clone()));
    // 调试项放最后，允许覆盖上面的任何默认（例如测试时 vo=null / ao=null）。
    for raw in &config.extra_options {
        let (name, value) = parse_option_override(raw);
        opts.push((name, value));
    }
    opts
}

/// 解析 `"name=value"` / `"flag"` 形式的额外选项。
pub fn parse_option_override(raw: &str) -> (String, String) {
    match raw.split_once('=') {
        Some((name, value)) => (name.trim().to_string(), value.trim().to_string()),
        None => (raw.trim().to_string(), "yes".to_string()),
    }
}

// ============================ 时间/事件小工具 ============================

/// 秒 → 毫秒（负值归零，NaN 归零）。
pub fn secs_to_ms(secs: f64) -> i64 {
    if !secs.is_finite() || secs <= 0.0 {
        0
    } else {
        (secs * 1000.0).round() as i64
    }
}

/// 把 seek 目标夹到 `[0, duration]`。
pub fn clamp_seek_target(target: f64, duration_secs: Option<f64>) -> f64 {
    let t = if target.is_finite() {
        target.max(0.0)
    } else {
        0.0
    };
    match duration_secs {
        Some(d) if d.is_finite() && d > 0.0 => t.min(d),
        _ => t,
    }
}

/// 字幕 cue 是否值得派发（空文本/同一条 cue 重复通知不派发）。
///
/// `prev` 保存上一条派发的 `(start_ms, text)`；返回 `true` 表示应派发，
/// 调用方负责更新 `prev`。
pub fn should_emit_cue(prev: &mut Option<(i64, String)>, start_ms: i64, text: &str) -> bool {
    let text = text.trim_end();
    if text.is_empty() {
        // 字幕消失：清掉 prev 以便同一句下次重现时能再派发
        *prev = None;
        return false;
    }
    let cur = (start_ms, text.to_string());
    if prev.as_ref() == Some(&cur) {
        return false;
    }
    *prev = Some(cur);
    true
}

/// 位置事件是否需要派发（避免高频抖动）。
pub fn position_delta_worth_event(prev: f64, next: f64) -> bool {
    (next - prev).abs() >= POSITION_EVENT_MIN_DELTA_SECS
}

// ============================ 测试 ============================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use wp_media::{BluRaySource, NetworkSource};

    fn sample_track_list() -> Value {
        json!([
            {"id": 1, "type": "video", "codec": "hevc", "default": true, "selected": true,
             "title": "", "lang": ""},
            {"id": 1, "type": "audio", "codec": "aac", "default": true, "selected": true,
             "title": "国语", "lang": "chi"},
            {"id": 2, "type": "audio", "codec": "ac3", "default": false, "selected": false,
             "title": "粤语", "lang": "yue"},
            {"id": 1, "type": "sub", "codec": "ass", "default": true, "selected": true,
             "title": "简体", "lang": "chi"},
            {"id": 2, "type": "sub", "codec": "pgs", "default": false, "selected": false,
             "title": "繁中", "lang": "chi"},
            {"id": 3, "type": "sub", "codec": "ass", "default": false, "selected": false,
             "title": "AI 双语", "lang": "", "external": true},
            {"id": 1, "type": "image", "codec": "mjpeg", "default": false, "selected": false}
        ])
    }

    #[test]
    fn track_list_parses_all_kinds_and_skips_image() {
        let tracks = parse_track_list(&sample_track_list());
        assert_eq!(tracks.len(), 6, "image 轨道应被跳过");
        assert_eq!(tracks[0].kind, Some(TrackKind::Video));
        assert_eq!(tracks[1].kind, Some(TrackKind::Audio));
        assert_eq!(tracks[1].title, "国语");
        assert_eq!(tracks[1].lang, "chi");
        assert!(tracks[1].selected);
        assert_eq!(tracks[5].id, 3);
        assert!(tracks[5].external);
        assert_eq!(selected_track_id(&tracks, TrackKind::Audio), Some(1));
        assert_eq!(selected_track_id(&tracks, TrackKind::Subtitle), Some(1));
    }

    #[test]
    fn track_list_malformed_is_empty() {
        assert!(parse_track_list(&json!({"nope": 1})).is_empty());
        assert!(parse_track_list(&json!(null)).is_empty());
        let weird = json!([{"id": "x", "type": "video"}, 42, {"type": "unknown"}]);
        let tracks = parse_track_list(&weird);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, 0);
    }

    #[test]
    fn track_kind_property_names() {
        assert_eq!(track_kind_property(TrackKind::Video), "vid");
        assert_eq!(track_kind_property(TrackKind::Audio), "aid");
        assert_eq!(track_kind_property(TrackKind::Subtitle), "sid");
        assert_eq!(track_kind_from_mpv("sub"), Some(TrackKind::Subtitle));
        assert_eq!(track_kind_from_mpv("image"), None);
    }

    #[test]
    fn video_params_parses_size() {
        assert_eq!(
            parse_video_params(&json!({"w": 3840, "h": 2160, "pixelformat": "yuv420p10"})),
            Some((3840, 2160))
        );
        assert_eq!(parse_video_params(&json!({"w": 0, "h": 0})), None);
        assert_eq!(parse_video_params(&json!({})), None);
        assert_eq!(parse_video_params(&json!([])), None);
    }

    #[test]
    fn demuxer_cache_reads_seekable_ranges() {
        let v = json!({
            "seekable-ranges": [{"start": 0.0, "end": 12.5}, {"start": 30.0, "end": 44.0}],
            "cache-end": 44.0,
            "eof-cached": false,
            "bof-cached": true
        });
        let ranges = parse_demuxer_cache_state(&v);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[1].start_secs, 30.0);
        assert_eq!(ranges[1].end_secs, 44.0);
    }

    #[test]
    fn demuxer_cache_falls_back_to_bof_cached() {
        let v = json!({"seekable-ranges": [], "bof-cached": true, "cache-end": 7.5});
        let ranges = parse_demuxer_cache_state(&v);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start_secs, 0.0);
        assert_eq!(ranges[0].end_secs, 7.5);
        assert!(parse_demuxer_cache_state(&json!({})).is_empty());
    }

    #[test]
    fn http_header_fields_joins_and_skips_bad_values() {
        let headers = vec![
            ("User-Agent".to_string(), "Mozilla/5.0".to_string()),
            ("Referer".to_string(), "https://pan.baidu.com/".to_string()),
            ("Cookie".to_string(), "a=1,b=2".to_string()), // 含逗号 → 跳过
            (String::new(), "ignored".to_string()),
        ];
        let got = http_header_fields(&headers).unwrap();
        assert_eq!(
            got,
            "User-Agent: Mozilla/5.0,Referer: https://pan.baidu.com/"
        );
        assert_eq!(http_header_fields(&[]), None);
    }

    #[test]
    fn source_options_network_and_bluray() {
        let network = NetworkSource {
            display: "x.mkv".into(),
            url: "http://127.0.0.1:1/s/t".into(),
            headers: vec![("Referer".into(), "https://pan.baidu.com/".into())],
            cache_key: "k".into(),
            size: 100,
        };
        let opts = source_options(&MediaSource::Network(network.clone()));
        // 请求头不再拼进 option 字符串（改由 engine 用 NODE_ARRAY 设置）
        assert!(!opts.iter().any(|(k, _)| k == "http-header-fields"));
        assert!(opts.iter().any(|(k, _)| k == "demuxer-max-bytes"));
        let list = http_header_fields_list(&network.headers);
        assert_eq!(list, vec!["Referer: https://pan.baidu.com/".to_string()]);
        assert_eq!(
            user_agent_header(&[("user-agent".into(), "UA/1".into())]),
            Some("UA/1".to_string())
        );

        let br = MediaSource::BluRay(BluRaySource {
            iso_or_dir: PathBuf::from("/media/movie.iso"),
            title_id: None,
        });
        let opts = source_options(&br);
        assert!(opts
            .iter()
            .any(|(k, v)| k == "bluray-device" && v == "/media/movie.iso"));

        assert!(source_options(&MediaSource::Local(PathBuf::from("/tmp/a.mkv"))).is_empty());
    }

    #[test]
    fn loadfile_args_for_all_sources() {
        let local = loadfile_args(&MediaSource::Local(PathBuf::from("/tmp/a b.mkv")));
        assert_eq!(local, vec!["loadfile", "/tmp/a b.mkv", "replace"]);

        let net = loadfile_args(&MediaSource::Network(NetworkSource {
            url: "http://127.0.0.1:9/s/t".into(),
            ..Default::default()
        }));
        assert_eq!(net, vec!["loadfile", "http://127.0.0.1:9/s/t", "replace"]);

        let main = loadfile_args(&MediaSource::BluRay(BluRaySource {
            iso_or_dir: PathBuf::from("/media/m.iso"),
            title_id: None,
        }));
        assert_eq!(main, vec!["loadfile", "bd://", "replace"]);

        let title = loadfile_args(&MediaSource::BluRay(BluRaySource {
            iso_or_dir: PathBuf::from("/media/m.iso"),
            title_id: Some(2),
        }));
        assert_eq!(title, vec!["loadfile", "bd://title/2", "replace"]);
    }

    #[test]
    fn build_options_embedded_gl_sets_vo_libmpv_and_extras_last() {
        let mut cfg = MpvConfig::default();
        cfg.hwdec = "vaapi".into();
        cfg.extra_options = vec!["vo=null".into(), "ao=null".into(), "mute".into()];
        let opts = build_options(&cfg);
        let get = |name: &str| {
            opts.iter()
                .rev()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("vo"), Some("null"), "extra_options 应覆盖默认 vo");
        assert_eq!(get("ao"), Some("null"));
        assert_eq!(get("hwdec"), Some("vaapi"));
        assert_eq!(get("mute"), Some("yes"));
        // display-resample 不能在首帧前设置（会卡视频初始化），默认选项里不应出现
        assert_eq!(get("video-sync"), None);
        assert_eq!(get("keep-open"), Some("yes"));
        assert_eq!(get("terminal"), Some("no"));

        let mut cfg2 = MpvConfig::default();
        cfg2.hwdec = "auto-safe".into();
        let opts2 = build_options(&cfg2);
        assert_eq!(
            opts2
                .iter()
                .find(|(k, _)| k == "vo")
                .map(|(_, v)| v.as_str()),
            Some("libmpv")
        );
    }

    #[test]
    fn build_options_external_window_uses_gpu() {
        let mut cfg = MpvConfig::default();
        cfg.render_mode = RenderMode::ExternalWindow;
        let opts = build_options(&cfg);
        assert_eq!(
            opts.iter()
                .find(|(k, _)| k == "vo")
                .map(|(_, v)| v.as_str()),
            Some("gpu")
        );
    }

    #[test]
    fn option_override_forms() {
        assert_eq!(
            parse_option_override("vo=null"),
            ("vo".to_string(), "null".to_string())
        );
        assert_eq!(
            parse_option_override("mute"),
            ("mute".to_string(), "yes".to_string())
        );
        assert_eq!(
            parse_option_override(" cache-secs = 30 "),
            ("cache-secs".to_string(), "30".to_string())
        );
    }

    #[test]
    fn time_helpers() {
        assert_eq!(secs_to_ms(1.2345), 1235);
        assert_eq!(secs_to_ms(-3.0), 0);
        assert_eq!(secs_to_ms(f64::NAN), 0);
        assert_eq!(clamp_seek_target(-5.0, Some(100.0)), 0.0);
        assert_eq!(clamp_seek_target(500.0, Some(100.0)), 100.0);
        assert_eq!(clamp_seek_target(5.0, None), 5.0);
        assert!(position_delta_worth_event(0.0, 0.02));
        assert!(!position_delta_worth_event(1.0, 1.001));
        assert!(position_delta_worth_event(1.0, 0.9));
    }

    #[test]
    fn cue_emit_policy() {
        let mut prev = None;
        assert!(should_emit_cue(&mut prev, 1000, "你好"));
        assert!(
            !should_emit_cue(&mut prev, 1000, "你好"),
            "同一条不重复派发"
        );
        assert!(should_emit_cue(&mut prev, 2000, "世界"), "新 cue 应派发");
        assert!(!should_emit_cue(&mut prev, 2000, "  "), "空文本不派发");
        assert!(
            should_emit_cue(&mut prev, 2000, "世界2"),
            "清空后重现可派发"
        );
    }
}
