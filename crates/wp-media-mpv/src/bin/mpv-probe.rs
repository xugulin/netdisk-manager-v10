//! `mpv-probe`：headless（`vo=null`）播放探针 —— M0 实验与真机联调诊断工具。
//!
//! 用真实的 [`MpvEngine`] 播一段本地文件 / 网络流（含 HLS m3u8），打印：
//! - 关键事件（Opened / FirstFrame / EndOfFile / Error / SubtitleCue …）与时间点
//! - 最终快照（位置/时长/轨道/hwdec/丢帧/缓冲）
//! - 打开→首帧延迟、平均 CPU 占用（进程 utime+stime / wall）
//!
//! 用法：
//! ```text
//! cargo run -p wp-media-mpv --bin mpv-probe -- <文件或URL> [选项]
//!
//! 选项：
//!   --seconds <N>        最多播放 N 秒（默认 8；到 EOF 会提前结束）
//!   --hwdec <mode>       hwdec（默认 auto-safe；headless 建议 no / auto-safe 对比）
//!   --header "K: V"      网络流请求头（可多次；HLS 的 m3u8/分片都会带）
//!   --sub <path>         注入字幕文件（可多次，第一次选中）
//!   --prop k=v           设置 mpv 属性/选项（可多次，如 secondary-sid=2）
//!   --json               输出单行 JSON（脚本/CI 用）
//! ```
//!
//! **注意**：本工具是 headless 的（`vo=null`），不创建 GL；Mode A 的渲染实验见
//! `examples/render_probe.rs`。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wp_media::{
    MediaEngine, MediaSource, NetworkSource, PlaybackSnapshot, PlayerCommand, PlayerEvent,
    PlayerState,
};
use wp_media_mpv::{MpvConfig, MpvEngine};

#[derive(Debug, Clone)]
struct EventRecord {
    at: Instant,
    event: PlayerEvent,
}

#[derive(Default)]
struct Args {
    source: String,
    seconds: f64,
    hwdec: String,
    headers: Vec<(String, String)>,
    subs: Vec<String>,
    props: Vec<(String, String)>,
    seek: Option<f64>,
    json: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        seconds: 8.0,
        hwdec: "auto-safe".into(),
        ..Default::default()
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--seconds" => {
                args.seconds = it
                    .next()
                    .ok_or("--seconds 缺少值")?
                    .parse()
                    .map_err(|_| "--seconds 需要数字")?;
            }
            "--hwdec" => args.hwdec = it.next().ok_or("--hwdec 缺少值")?,
            "--header" => {
                let raw = it.next().ok_or("--header 缺少值")?;
                let (k, v) = raw
                    .split_once(':')
                    .ok_or_else(|| format!("--header 需要 \"K: V\" 形式：{raw}"))?;
                args.headers.push((k.trim().into(), v.trim().into()));
            }
            "--sub" => args.subs.push(it.next().ok_or("--sub 缺少值")?),
            "--prop" => {
                let raw = it.next().ok_or("--prop 缺少值")?;
                let (k, v) = raw.split_once('=').ok_or("--prop 需要 k=v 形式")?;
                args.props.push((k.into(), v.into()));
            }
            "--seek" => {
                args.seek = Some(
                    it.next()
                        .ok_or("--seek 缺少值")?
                        .parse()
                        .map_err(|_| "需要数字")?,
                )
            }
            "--json" => args.json = true,
            "-h" | "--help" => {
                println!("用法：mpv-probe <文件或URL> [--seconds N] [--hwdec mode] [--header 'K: V'] [--sub path] [--prop k=v] [--json]");
                std::process::exit(0);
            }
            other if other.starts_with("--") => return Err(format!("未知选项：{other}")),
            other => {
                if !args.source.is_empty() {
                    return Err("只能指定一个媒体源".into());
                }
                args.source = other.to_string();
            }
        }
    }
    if args.source.is_empty() {
        return Err("缺少媒体源（文件路径或 URL）".into());
    }
    Ok(args)
}

fn cpu_ticks() -> u64 {
    // /proc/self/stat: 第 14/15 字段是 utime/stime（clock ticks）
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let after_comm = stat.rfind(')').map(|i| &stat[i + 1..]).unwrap_or("");
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let utime: u64 = fields.get(11).and_then(|s| s.parse().ok()).unwrap_or(0);
    let stime: u64 = fields.get(12).and_then(|s| s.parse().ok()).unwrap_or(0);
    utime + stime
}

fn clock_ticks_per_sec() -> f64 {
    // Linux 常见 USER_HZ=100；用 getconf 校准，拿不到就按 100
    std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(100.0)
}

fn event_label(event: &PlayerEvent) -> String {
    match event {
        PlayerEvent::Opened {
            duration_secs,
            tracks,
            video_size,
        } => format!(
            "Opened(duration={duration_secs:?}, tracks={}, video={video_size:?})",
            tracks.len()
        ),
        PlayerEvent::StateChanged(s) => format!("StateChanged({s:?})"),
        PlayerEvent::PositionChanged(p) => format!("PositionChanged({p:.3})"),
        PlayerEvent::DurationChanged(d) => format!("DurationChanged({d:?})"),
        PlayerEvent::TracksChanged(t) => format!("TracksChanged({} tracks)", t.len()),
        PlayerEvent::SubtitleCue {
            start_ms,
            text,
            secondary,
        } => format!(
            "SubtitleCue(start_ms={start_ms}, secondary={secondary}, text={:?})",
            text.chars().take(48).collect::<String>()
        ),
        PlayerEvent::Buffering(p) => format!("Buffering({p})"),
        PlayerEvent::FirstFrame => "FirstFrame".into(),
        PlayerEvent::EndOfFile => "EndOfFile".into(),
        PlayerEvent::Error {
            message,
            recoverable,
        } => format!("Error(recoverable={recoverable}, {message})"),
        PlayerEvent::Custom(c) => format!("Custom({c})"),
    }
}

fn fmt_snapshot(s: &PlaybackSnapshot) -> String {
    format!(
        "state={:?} pos={:.3} dur={:?} hwdec={:?} video={:?} dropped={} tracks={} \
         audio_track={:?} sub_track={:?} secondary_visible={} buffered_ranges={}",
        s.state,
        s.position_secs,
        s.duration_secs,
        s.hwdec,
        s.video_size,
        s.dropped_frames,
        s.tracks.len(),
        s.audio_track,
        s.subtitle_track,
        s.secondary_subtitle_visible,
        s.buffered.len()
    )
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("[mpv-probe] 参数错误：{e}");
            std::process::exit(2);
        }
    };

    let mut config = MpvConfig {
        hwdec: args.hwdec.clone(),
        ..Default::default()
    };
    // headless：不创建 GL / 不出声；仍走缓存/解码/解复用全链路
    config.extra_options = vec![
        "vo=null".into(),
        "ao=null".into(),
        "cache=yes".into(),
        "cache-secs=30".into(),
    ];

    let t0 = Instant::now();
    let cpu0 = cpu_ticks();
    let mut engine = match MpvEngine::new(config) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("[mpv-probe] 创建引擎失败：{e}");
            std::process::exit(1);
        }
    };
    let version = engine.mpv_version();
    let events: Arc<Mutex<Vec<EventRecord>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    engine.set_event_handler(Box::new(move |event| {
        sink.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(EventRecord {
                at: Instant::now(),
                event,
            });
    }));

    let is_network = args.source.starts_with("http://") || args.source.starts_with("https://");
    let source = if is_network {
        MediaSource::Network(NetworkSource {
            display: args.source.clone(),
            url: args.source.clone(),
            headers: args.headers.clone(),
            cache_key: String::new(),
            size: 0,
        })
    } else {
        MediaSource::Local(args.source.clone().into())
    };

    let open_called_at = Instant::now();
    let open_error = engine.open(source).err();
    let open_result = open_error.map(|e| format!("open 失败：{e}"));

    // 等待 Opened / 首帧 / EOF / 超时，同时注入字幕/属性
    let deadline = t0 + Duration::from_secs_f64(args.seconds.max(1.0));
    let mut injected = false;
    let mut opened_at: Option<Instant> = None;
    let mut first_frame_at: Option<Instant> = None;
    while Instant::now() < deadline {
        let snapshot = engine.snapshot();
        let seen = {
            let list = events.lock().unwrap_or_else(|e| e.into_inner());
            let opened = list
                .iter()
                .any(|r| matches!(r.event, PlayerEvent::Opened { .. }));
            let first_frame = list
                .iter()
                .any(|r| matches!(r.event, PlayerEvent::FirstFrame));
            (opened, first_frame)
        };
        if opened_at.is_none() && seen.0 {
            opened_at = events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .find(|r| matches!(r.event, PlayerEvent::Opened { .. }))
                .map(|r| r.at);
            // 打开成功后注入字幕与属性（模拟 GUI/AI 的实际操作顺序）
            if !injected {
                injected = true;
                for (i, sub) in args.subs.iter().enumerate() {
                    let _ = engine.command(PlayerCommand::AddSubtitleFile {
                        path: sub.into(),
                        select: i == 0,
                    });
                }
                for (k, v) in &args.props {
                    let _ = engine.set_property(k.clone(), v.clone());
                }
                if let Some(target) = args.seek {
                    let _ = engine.command(PlayerCommand::SeekAbsolute(target));
                }
            }
        }
        if first_frame_at.is_none() && seen.1 {
            first_frame_at = events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .find(|r| matches!(r.event, PlayerEvent::FirstFrame))
                .map(|r| r.at);
        }
        // 结束条件：已打开后进入 Ended/Error；或 open() 本身同步失败。
        // 注意：打开前 state 也是 Idle，不能把 Idle 当结束。
        let finished = opened_at.is_some()
            && matches!(snapshot.state, PlayerState::Ended | PlayerState::Error);
        if open_result.is_some() || finished {
            break;
        }
        // 主动推进一点播放（异步命令，不阻塞）
        std::thread::sleep(Duration::from_millis(50));
    }

    let snapshot = engine.snapshot();
    let wall = t0.elapsed().as_secs_f64();
    let cpu = cpu_ticks().saturating_sub(cpu0) as f64 / clock_ticks_per_sec();
    let event_list = events.lock().unwrap_or_else(|e| e.into_inner()).clone();

    let open_to_opened_ms =
        opened_at.map(|at| at.duration_since(open_called_at).as_secs_f64() * 1000.0);
    let open_to_first_frame_ms =
        first_frame_at.map(|at| at.duration_since(open_called_at).as_secs_f64() * 1000.0);
    let cues: Vec<&EventRecord> = event_list
        .iter()
        .filter(|r| matches!(r.event, PlayerEvent::SubtitleCue { .. }))
        .collect();
    let primary_cue = cues.iter().find(|r| {
        matches!(
            &r.event,
            PlayerEvent::SubtitleCue {
                secondary: false,
                ..
            }
        )
    });
    let secondary_cue = cues.iter().find(|r| {
        matches!(
            &r.event,
            PlayerEvent::SubtitleCue {
                secondary: true,
                ..
            }
        )
    });

    if args.json {
        let json = serde_json::json!({
            "source": args.source,
            "network": is_network,
            "mpv_version": version,
            "hwdec_requested": args.hwdec,
            "seconds": args.seconds,
            "seek_requested": args.seek,
            "wall_secs": wall,
            "cpu_secs": cpu,
            "cpu_cores_avg": if wall > 0.0 { cpu / wall } else { 0.0 },
            "open_error": open_result,
            "open_to_opened_ms": open_to_opened_ms,
            "open_to_first_frame_ms": open_to_first_frame_ms,
            "final_snapshot": format!("{snapshot:?}"),
            "state": format!("{:?}", snapshot.state),
            "position_secs": snapshot.position_secs,
            "duration_secs": snapshot.duration_secs,
            "hwdec_current": snapshot.hwdec,
            "video_size": snapshot.video_size.map(|(w, h)| vec![w, h]),
            "dropped_frames": snapshot.dropped_frames,
            "tracks": snapshot.tracks.len(),
            "first_primary_cue": primary_cue.map(|r| format!("{:?}", r.event)),
            "first_secondary_cue": secondary_cue.map(|r| format!("{:?}", r.event)),
            "events": event_list.iter().map(|r| event_label(&r.event)).collect::<Vec<_>>(),
        });
        println!("{json}");
    } else {
        println!("== mpv-probe ==");
        println!("源：{}（network={is_network}）", args.source);
        println!("mpv：{version}");
        println!("hwdec 请求：{}", args.hwdec);
        if let Some(err) = &open_result {
            println!("⚠ {err}");
        }
        println!("---- 事件（{} 条）----", event_list.len());
        for record in &event_list {
            println!(
                "[{:>8.3}s] {}",
                record.at.duration_since(t0).as_secs_f64(),
                event_label(&record.event)
            );
        }
        println!("---- 结果 ----");
        println!(
            "播放时长（墙钟）：{wall:.2}s  进程 CPU：{cpu:.2}s（{} 核均值）",
            if wall > 0.0 { cpu / wall } else { 0.0 }
        );
        if let Some(ms) = open_to_opened_ms {
            println!("打开→Opened 延迟：{ms:.1} ms");
        }
        if let Some(ms) = open_to_first_frame_ms {
            println!("打开→首帧延迟：{ms:.1} ms");
        } else {
            println!("打开→首帧延迟：未观测到 FirstFrame（headless vo=null 下也可能不触发）");
        }
        println!("快照：{}", fmt_snapshot(&snapshot));
        if let Some(record) = primary_cue {
            println!("主字幕 cue 样例：{}", event_label(&record.event));
        } else {
            println!("主字幕 cue：无（未注入字幕或该时段无字幕）");
        }
        if let Some(record) = secondary_cue {
            println!("副字幕 cue 样例：{}", event_label(&record.event));
        } else {
            println!("副字幕 cue：无");
        }
        println!(
            "结论：{}",
            if snapshot.state == PlayerState::Error {
                "失败"
            } else {
                "可播放"
            }
        );
    }

    engine.shutdown();
}
