//! `render_probe`：Mode A（进程内 GL render API）真渲染实验 —— M0-E2/E3。
//!
//! 用 eframe 打开一个窗口（headless 用 `xvfb-run`），把 mpv 画面渲染进 egui 中央区域，
//! 播完/超时后打印：首帧延迟、`mpv_render_context_update` 帧数、实际渲染帧数、
//! 估算 FPS、hwdec、丢帧、CPU 占用等；可同时注入主/副字幕（ASS 弹幕）用于 E3。
//!
//! 用法：
//! ```text
//! xvfb-run -a -s "-screen 0 1280x800x24" \
//!   cargo run -p wp-media-mpv --example render_probe -- <视频> [选项]
//!
//! 选项：
//!   --seconds <N>    持续 N 秒后退出（默认 8）
//!   --size <WxH>     窗口大小（默认 1280x720）
//!   --hwdec <mode>   默认 auto-safe
//!   --sub <path>     注入字幕（可多次；第一次选中）
//!   --prop k=v       设置 mpv 属性（可多次；E5 起副字幕请用 --secondary-sid 正式命令）
//!   --secondary-sid <N>  用正式 PlayerCommand::SelectSecondarySubtitleTrack 选副字幕轨（E5-1）
//!   --fonts-dir <dir>    MpvConfig.fonts_dir：自定义字幕字体目录（E5-2）
//!   --sub-auto <mode>    MpvConfig.sub_auto：no/exact/fuzzy/all（默认 no；E5-2 同名自动加载）
//!   --dump-props k1,k2   退出前读 mpv 属性写入 JSON（诊断，如 sub-fonts-dir,sub-auto,secondary-sid）
//!   --sub-reload <path>|<old>|<new>  播放中改字幕文本 → ReloadSubtitles → 验证 cue 更新（E5-4）
//!   --reload-at-ms <N>   sub-reload 触发时刻（默认 1500ms）
//!   --json           输出单行 JSON
//! ```
//!
//! 说明：eframe 初次使用需要编译 GUI 依赖（几十秒），仅开发/实验用。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use eframe::glow;
use wp_media::{MediaEngine, MediaSource, PlayerCommand, PlayerEvent, PlayerState};
use wp_media_mpv::render::{MpvRenderContext, RenderStats};
use wp_media_mpv::{MpvConfig, MpvEngine};

#[derive(Debug, Clone)]
struct Args {
    video: String,
    seconds: f64,
    size: (u32, u32),
    hwdec: String,
    subs: Vec<String>,
    props: Vec<(String, String)>,
    json: bool,
    screenshot: Option<String>,
    screenshot_window: Option<String>,
    /// 视频矩形（x,y,w,h，egui points；模拟 GUI 里 inner 16:9 区域）
    rect: Option<[f32; 4]>,
    seek: Option<f64>,
    /// E5-1：副字幕轨 id（用正式 PlayerCommand 选择，不再用 set_property 逃生门）。
    secondary_sid: Option<i64>,
    /// E5-2：MpvConfig.fonts_dir / sub_auto。
    fonts_dir: Option<String>,
    sub_auto: Option<String>,
    /// 退出前读取的 mpv 属性名（诊断）。
    dump_props: Vec<String>,
    /// E5-4：sub-reload 演示 (path, old, new)。
    sub_reload: Option<(String, String, String)>,
    reload_at_ms: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        video: String::new(),
        seconds: 8.0,
        size: (1280, 720),
        hwdec: "auto-safe".into(),
        subs: Vec::new(),
        props: Vec::new(),
        json: false,
        screenshot: None,
        screenshot_window: None,
        rect: None,
        seek: None,
        secondary_sid: None,
        fonts_dir: None,
        sub_auto: None,
        dump_props: Vec::new(),
        sub_reload: None,
        reload_at_ms: 1500,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--seconds" => {
                args.seconds = it.next().ok_or("缺少值")?.parse().map_err(|_| "需要数字")?
            }
            "--size" => {
                let raw = it.next().ok_or("缺少值")?;
                let (w, h) = raw.split_once('x').ok_or("--size 需要 WxH")?;
                args.size = (
                    w.parse().map_err(|_| "宽需要数字")?,
                    h.parse().map_err(|_| "高需要数字")?,
                );
            }
            "--hwdec" => args.hwdec = it.next().ok_or("缺少值")?,
            "--sub" => args.subs.push(it.next().ok_or("缺少值")?),
            "--prop" => {
                let raw = it.next().ok_or("缺少值")?;
                let (k, v) = raw.split_once('=').ok_or("--prop 需要 k=v")?;
                args.props.push((k.into(), v.into()));
            }
            "--secondary-sid" => {
                args.secondary_sid =
                    Some(it.next().ok_or("缺少值")?.parse().map_err(|_| "需要整数")?)
            }
            "--fonts-dir" => args.fonts_dir = Some(it.next().ok_or("缺少值")?),
            "--sub-auto" => args.sub_auto = Some(it.next().ok_or("缺少值")?),
            "--dump-props" => {
                let raw = it.next().ok_or("缺少值")?;
                args.dump_props = raw
                    .split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect();
            }
            "--sub-reload" => {
                let raw = it.next().ok_or("缺少值")?;
                let parts: Vec<&str> = raw.split('|').collect();
                if parts.len() != 3 {
                    return Err("--sub-reload 需要 <path>|<old>|<new>".into());
                }
                args.sub_reload = Some((parts[0].into(), parts[1].into(), parts[2].into()));
            }
            "--reload-at-ms" => {
                args.reload_at_ms = it.next().ok_or("缺少值")?.parse().map_err(|_| "需要整数")?
            }
            "--json" => args.json = true,
            "--screenshot" => args.screenshot = Some(it.next().ok_or("缺少值")?),
            "--screenshot-window" => args.screenshot_window = Some(it.next().ok_or("缺少值")?),
            "--rect" => {
                let raw = it.next().ok_or("缺少值")?;
                let parts: Vec<f32> = raw
                    .split(',')
                    .map(|p| p.trim().parse::<f32>())
                    .collect::<Result<_, _>>()
                    .map_err(|_| "--rect 需要 x,y,w,h")?;
                if parts.len() != 4 {
                    return Err("--rect 需要 x,y,w,h".into());
                }
                args.rect = Some([parts[0], parts[1], parts[2], parts[3]]);
            }
            "--seek" => {
                args.seek = Some(it.next().ok_or("缺少值")?.parse().map_err(|_| "需要数字")?)
            }
            other if other.starts_with("--") => return Err(format!("未知选项：{other}")),
            other => {
                if !args.video.is_empty() {
                    return Err("只能指定一个视频".into());
                }
                args.video = other.into();
            }
        }
    }
    if args.video.is_empty() {
        return Err("缺少视频路径".into());
    }
    Ok(args)
}

fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let after_comm = stat.rfind(')').map(|i| &stat[i + 1..]).unwrap_or("");
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let utime: u64 = fields.get(11).and_then(|s| s.parse().ok()).unwrap_or(0);
    let stime: u64 = fields.get(12).and_then(|s| s.parse().ok()).unwrap_or(0);
    utime + stime
}

#[derive(Default)]
struct Measurements {
    first_frame_at: Option<Instant>,
    opened_at: Option<Instant>,
    injected: bool,
    rendered_frames: u64,
    paint_calls: u64,
    open_called_at: Option<Instant>,
    seek_done: bool,
    /// E5-1：副字幕正式命令已发。
    secondary_sent: bool,
    /// E5-4：sub-reload 已执行 / 执行时刻 / 执行前 cue 数。
    reload_done: bool,
    reload_at: Option<Instant>,
    reload_cues_before: usize,
}

struct ProbeApp {
    engine: MpvEngine,
    render: Option<MpvRenderContext>,
    args: Args,
    started: Instant,
    t0: Instant,
    cpu0: u64,
    events: Arc<Mutex<Vec<(Instant, PlayerEvent)>>>,
    measurements: Measurements,
    final_stats: Option<(RenderStats, wp_media::PlaybackSnapshot)>,
    done: bool,
    /// 开始退出时刻：之后 mpv 因释放 render context 产生的 END_FILE error 是关停噪音，统计时忽略
    closed_at: Option<Instant>,
}

impl ProbeApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        engine: MpvEngine,
        args: Args,
    ) -> Result<Self, String> {
        let get_proc_address = cc
            .get_proc_address
            .clone()
            .ok_or("eframe 没有提供 GL get_proc_address（需要 glow 后端）")?;
        let render = MpvRenderContext::new(
            engine
                .render_source()
                .map_err(|e| format!("render_source 失败：{e}"))?,
            get_proc_address,
            cc.egui_ctx.clone(),
        )
        .map_err(|e| format!("创建渲染桥失败：{e}"))?;
        Ok(Self {
            engine,
            render: Some(render),
            args,
            started: Instant::now(),
            t0: Instant::now(),
            cpu0: cpu_ticks(),
            events: Arc::new(Mutex::new(Vec::new())),
            measurements: Measurements::default(),
            final_stats: None,
            done: false,
            closed_at: None,
        })
    }

    fn subtitle_cue_count(&self) -> usize {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, e)| matches!(e, PlayerEvent::SubtitleCue { .. }))
            .count()
    }

    fn finish(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        if let Some(render) = &self.render {
            self.final_stats = Some((render.stats(), self.engine.snapshot()));
        }
    }
}

impl eframe::App for ProbeApp {
    // eframe 0.37：App 的每帧入口是 `ui`（不再是旧的 `update`）。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // 首次 ui：设置事件处理器 + 打开媒体（只做一次）
        if !self.measurements.injected {
            self.measurements.injected = true;
            let events = self.events.clone();
            self.engine.set_event_handler(Box::new(move |event| {
                events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((Instant::now(), event));
            }));
            self.measurements.open_called_at = Some(Instant::now());
            if let Err(e) = self
                .engine
                .open(MediaSource::Local(self.args.video.clone().into()))
            {
                eprintln!("[render_probe] open 失败：{e}");
                self.finish();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }

        // 分配整个根 Ui；视频可画在指定子矩形（--rect 模拟 GUI 的 16:9 inner rect）。
        let size = ui.available_size();
        let (full_rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
        if response.clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let video_rect = match self.args.rect {
            Some([x, y, w, h]) => egui::Rect::from_min_size(
                full_rect.min + egui::vec2(x, y),
                egui::vec2(w.max(1.0), h.max(1.0)),
            ),
            None => full_rect,
        };
        // 背景（视频矩形之外）先画深灰，便于验证合成位置；
        // 顺序很重要：先背景，再视频回调，最后 UI 覆盖层。
        ui.painter()
            .rect_filled(full_rect, 0.0, egui::Color32::from_rgb(30, 30, 36));
        if let Some(render) = &self.render {
            render.paint_rect(ui, video_rect);
        }

        // E2 验收点：egui UI 与视频**同帧合成**（在 video 之后绘制 → 覆盖在视频上）。
        let badge = egui::Rect::from_min_size(
            video_rect.min + egui::vec2(16.0, 16.0),
            egui::vec2(220.0, 36.0),
        );
        ui.painter().rect_filled(
            badge,
            6.0,
            egui::Color32::from_rgba_unmultiplied(220, 30, 30, 200),
        );
        ui.painter().text(
            badge.center(),
            egui::Align2::CENTER_CENTER,
            "UI OVERLAY 同帧合成",
            egui::FontId::proportional(16.0),
            egui::Color32::WHITE,
        );

        // 事件驱动：Opened 后注入字幕/属性
        let (opened, first_frame) = {
            let list = self.events.lock().unwrap_or_else(|e| e.into_inner());
            (
                list.iter()
                    .find(|(_, e)| matches!(e, PlayerEvent::Opened { .. }))
                    .map(|(at, _)| *at),
                list.iter()
                    .find(|(_, e)| matches!(e, PlayerEvent::FirstFrame))
                    .map(|(at, _)| *at),
            )
        };
        if opened.is_some() && self.measurements.opened_at.is_none() {
            self.measurements.opened_at = opened;
            for (i, sub) in self.args.subs.iter().enumerate() {
                let path = std::path::PathBuf::from(sub);
                let _ = self.engine.command(PlayerCommand::AddSubtitleFile {
                    path,
                    select: i == 0,
                });
            }
            for (k, v) in &self.args.props {
                let _ = self.engine.set_property(k.clone(), v.clone());
            }
            // E5-1：副字幕用正式 PlayerCommand（等价 mpv secondary-sid），不再 set_property。
            if let Some(id) = self.args.secondary_sid {
                let _ = self
                    .engine
                    .command(PlayerCommand::SelectSecondarySubtitleTrack { id });
                let _ = self
                    .engine
                    .command(PlayerCommand::SetSecondarySubtitleVisible(true));
                self.measurements.secondary_sent = true;
            }
        }
        if first_frame.is_some() && self.measurements.first_frame_at.is_none() {
            self.measurements.first_frame_at = first_frame;
        }
        if !self.measurements.seek_done {
            if let Some(target) = self.args.seek {
                if self.measurements.opened_at.is_some() {
                    self.measurements.seek_done = true;
                    let _ = self.engine.command(PlayerCommand::SeekAbsolute(target));
                }
            }
        }
        // E5-4：播放中改写字幕文件 → ReloadSubtitles → 后续 cue 应为新文本。
        if !self.measurements.reload_done && self.measurements.opened_at.is_some() {
            if self.started.elapsed().as_millis() as u64 >= self.args.reload_at_ms {
                self.measurements.reload_done = true;
                if let Some((path, old, new)) = &self.args.sub_reload {
                    let before = self.subtitle_cue_count();
                    match std::fs::read_to_string(path) {
                        Ok(text) if text.contains(old) => {
                            match std::fs::write(path, text.replace(old, new)) {
                                Ok(()) => {
                                    let _ = self.engine.command(PlayerCommand::ReloadSubtitles);
                                    self.measurements.reload_at = Some(Instant::now());
                                    self.measurements.reload_cues_before = before;
                                    if !self.args.json {
                                        eprintln!(
                                            "[render_probe] sub-reload：{path} 文本 {old:?} → {new:?}"
                                        );
                                    }
                                }
                                Err(e) => eprintln!("[render_probe] sub-reload 写文件失败：{e}"),
                            }
                        }
                        Ok(_) => eprintln!(
                            "[render_probe] sub-reload 未找到旧文本 {old:?}（文件：{path}）"
                        ),
                        Err(e) => eprintln!("[render_probe] sub-reload 读文件失败：{e}"),
                    }
                }
            }
        }
        if let Some(render) = &self.render {
            let stats = render.stats();
            self.measurements.rendered_frames = stats.frames_rendered;
            self.measurements.paint_calls = stats.paint_calls;
            if self.measurements.paint_calls > 0 && self.measurements.rendered_frames == 0 {
                // 首帧迟迟不来时打印一次渲染桥错误（诊断用）
                if let (Some(e), true) = (render.last_error(), !self.args.json) {
                    eprintln!("[render_probe] 渲染桥错误：{e}");
                }
            }
        }

        // 结束条件：
        //  - 首帧后跑满 --seconds；
        //  - 已打开且播放出错/结束；
        //  - 10s 还没打开（失败兜底，防止窗口挂着不退出）。
        // ⚠ 打开前 state 也是 Idle，不能把 Idle 当结束（否则第一帧就退出、画面永远不出来）。
        let snapshot = self.engine.snapshot();
        let elapsed = self.started.elapsed().as_secs_f64();
        let opened_seen = self.measurements.opened_at.is_some();
        let run_time_done =
            self.measurements.first_frame_at.is_some() && elapsed >= self.args.seconds;
        let play_finished =
            opened_seen && matches!(snapshot.state, PlayerState::Ended | PlayerState::Error);
        let open_timeout = !opened_seen && elapsed > 10.0;
        if run_time_done || play_finished || open_timeout {
            self.finish();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // 持续重绘（视频新帧另由 mpv update callback 触发 request_repaint）
        ctx.request_repaint_after(Duration::from_millis(16));
    }

    fn on_exit(&mut self, gl: Option<&glow::Context>) {
        self.closed_at = Some(Instant::now());
        self.finish();
        // 诊断属性必须在 engine.shutdown()（下面）之前读：worker 停掉后属性读取全部 None。
        let props: serde_json::Map<String, serde_json::Value> = self
            .args
            .dump_props
            .iter()
            .filter_map(|name| {
                self.engine
                    .mpv_property_string(name)
                    .map(|value| (name.clone(), serde_json::Value::String(value)))
            })
            .collect();
        // 窗口截图：读当前（默认）framebuffer，验证「视频画在 egui 期望的矩形里」
        if let (Some(path), Some(gl)) = (&self.args.screenshot_window, gl) {
            let mut viewport = [0i32; 4];
            unsafe {
                use eframe::glow::HasContext as _;
                gl.get_parameter_i32_slice(eframe::glow::VIEWPORT, &mut viewport);
                let (w, h) = (viewport[2].max(1), viewport[3].max(1));
                let mut pixels = vec![0u8; (w * h * 4) as usize];
                gl.bind_framebuffer(eframe::glow::FRAMEBUFFER, None);
                gl.viewport(0, 0, w, h);
                gl.read_pixels(
                    0,
                    0,
                    w,
                    h,
                    eframe::glow::RGBA,
                    eframe::glow::UNSIGNED_BYTE,
                    eframe::glow::PixelPackData::Slice(Some(&mut pixels)),
                );
                if let Err(e) = write_ppm(path, w, h, &pixels) {
                    eprintln!("[render_probe] 窗口截图写入失败：{e}");
                }
            }
        }
        // 视频内容截图：直接从 GL 离屏目标读像素（不依赖 mpv screenshot 命令）
        if let (Some(path), Some(render), Some(gl)) = (&self.args.screenshot, &self.render, gl) {
            if let Ok((w, h, rgba)) = render.read_pixels(gl) {
                if let Err(e) = write_ppm(path, w, h, &rgba) {
                    eprintln!("[render_probe] 截图写入失败：{e}");
                }
            }
        }
        if let Some(render) = &self.render {
            render.destroy(gl);
        }
        self.engine.shutdown();

        // ---- 输出测量结果 ----
        let (stats, snapshot) = self.final_stats.take().unwrap_or_default();
        let wall = self.t0.elapsed().as_secs_f64();
        let cpu = cpu_ticks().saturating_sub(self.cpu0) as f64 / 100.0;
        // 打开→首帧（从调用 open() 算起；含排队等渲染桥就绪的时间）
        let first_frame_ms = match (
            self.measurements.open_called_at,
            self.measurements.first_frame_at,
        ) {
            (Some(open), Some(first)) => Some(first.duration_since(open).as_secs_f64() * 1000.0),
            _ => None,
        };
        let fps = if wall > 0.0 {
            stats.frames_rendered as f64 / wall
        } else {
            0.0
        };
        let events = self
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let cues: Vec<String> = events
            .iter()
            .filter(|(_, e)| matches!(e, PlayerEvent::SubtitleCue { .. }))
            .map(|(at, e)| format!("[{:.2}s] {:?}", at.duration_since(self.t0).as_secs_f64(), e))
            .collect();
        // 只统计「主动关停之前」的错误：destroy render context 会让 mpv 报
        // END_FILE(video output initialization failed)，属于关停噪音。
        let closed_at = self.closed_at;
        let errors: Vec<String> = events
            .iter()
            .filter(|(at, e)| {
                matches!(e, PlayerEvent::Error { .. })
                    && closed_at.map_or(true, |closed| *at < closed)
            })
            .map(|(_, e)| format!("{e:?}"))
            .collect();

        // E5：主/副 cue 分项 + sub-reload 前后对比（props 已在 on_exit 开头读取）。
        let primary_cues = events
            .iter()
            .filter(|(_, e)| {
                matches!(
                    e,
                    PlayerEvent::SubtitleCue {
                        secondary: false,
                        ..
                    }
                )
            })
            .count();
        let secondary_cues = events
            .iter()
            .filter(|(_, e)| {
                matches!(
                    e,
                    PlayerEvent::SubtitleCue {
                        secondary: true,
                        ..
                    }
                )
            })
            .count();
        let reload = self.measurements.reload_at.map(|at| {
            let after: Vec<String> = events
                .iter()
                .filter(|(t, e)| *t >= at && matches!(e, PlayerEvent::SubtitleCue { .. }))
                .map(|(_, e)| format!("{e:?}"))
                .collect();
            serde_json::json!({
                "at_secs": at.duration_since(self.t0).as_secs_f64(),
                "cues_before": self.measurements.reload_cues_before,
                "cues_after_reload": after,
            })
        });

        if self.args.json {
            let json = serde_json::json!({
                "video": self.args.video,
                "hwdec_requested": self.args.hwdec,
                "mpv_version": self.engine.mpv_version(),
                "wall_secs": wall,
                "cpu_secs": cpu,
                "cpu_cores_avg": if wall > 0.0 { cpu / wall } else { 0.0 },
                "open_to_first_frame_ms": first_frame_ms,
                "render": {
                    "paint_calls": stats.paint_calls,
                    "frames_rendered": stats.frames_rendered,
                    "frames_reused": stats.frames_reused,
                    "last_update_flags": stats.last_update_flags,
                    "first_frame": stats.first_frame,
                    "gl_error": stats.gl_error,
                },
                "avg_fps": fps,
                "final_snapshot": {
                    "state": format!("{:?}", snapshot.state),
                    "hwdec": snapshot.hwdec,
                    "video_size": snapshot.video_size.map(|(w,h)| vec![w,h]),
                    "dropped_frames": snapshot.dropped_frames,
                    "position_secs": snapshot.position_secs,
                    "duration_secs": snapshot.duration_secs,
                    "tracks": snapshot.tracks.len(),
                    "tracks_detail": snapshot.tracks.iter().map(|t| serde_json::json!({
                        "id": t.id,
                        "kind": format!("{:?}", t.kind),
                        "title": t.title,
                        "lang": t.lang,
                        "codec": t.codec,
                        "selected": t.selected,
                        "external": t.external,
                    })).collect::<Vec<_>>(),
                    "secondary_subtitle_track": snapshot.secondary_subtitle_track,
                },
                "primary_cues": primary_cues,
                "secondary_cues": secondary_cues,
                "secondary_command_sent": self.measurements.secondary_sent,
                "props": props,
                "sub_reload": reload,
                "subtitle_cues": cues,
                "errors": errors,
                "render_last_error": self.render.as_ref().and_then(|r| r.last_error()),
                "engine_last_error": self.engine.last_error(),
            });
            println!("{json}");
        } else {
            println!("== render_probe（Mode A / GL render API）==");
            println!("视频：{}", self.args.video);
            println!(
                "mpv：{}  hwdec 请求：{}",
                self.engine.mpv_version(),
                self.args.hwdec
            );
            println!(
                "墙钟：{wall:.2}s  CPU：{cpu:.2}s（{:.2} 核均值）",
                if wall > 0.0 { cpu / wall } else { 0.0 }
            );
            if let Some(ms) = first_frame_ms {
                println!("打开→首帧：{ms:.1} ms");
            }
            println!(
                "渲染：paint_calls={} frames_rendered={} frames_reused={} 平均 {fps:.1} fps",
                stats.paint_calls, stats.frames_rendered, stats.frames_reused
            );
            println!(
                "last_update_flags=0x{:x} gl_error={}",
                stats.last_update_flags, stats.gl_error
            );
            println!(
                "快照：state={:?} hwdec={:?} video={:?} dropped={} pos={:.2} dur={:?} tracks={}",
                snapshot.state,
                snapshot.hwdec,
                snapshot.video_size,
                snapshot.dropped_frames,
                snapshot.position_secs,
                snapshot.duration_secs,
                snapshot.tracks.len()
            );
            println!("字幕 cue 条数：{}", cues.len());
            println!(
                "字幕 cue 分项：主={primary_cues} 副={secondary_cues}；副字幕命令已发送={} 快照 secondary_track={:?}",
                self.measurements.secondary_sent, snapshot.secondary_subtitle_track
            );
            if !props.is_empty() {
                println!("mpv 属性诊断：{props:?}");
            }
            if let Some(reload) = &reload {
                println!("sub-reload：{reload}");
            }
            for cue in cues.iter().take(6) {
                println!("  {cue}");
            }
            if !errors.is_empty() {
                println!("错误事件：{}", errors.len());
                for e in errors.iter().take(5) {
                    println!("  {e}");
                }
            }
        }
    }
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("[render_probe] 参数错误：{e}");
            std::process::exit(2);
        }
    };
    let mut config = MpvConfig {
        hwdec: args.hwdec.clone(),
        fonts_dir: args.fonts_dir.as_ref().map(std::path::PathBuf::from),
        sub_auto: args.sub_auto.clone().unwrap_or_else(|| "no".into()),
        ..Default::default()
    };
    // 注意：render mode = EmbeddedGl 时 vo=libmpv（默认），不要覆盖成 vo=null。
    config.extra_options = vec!["ao=null".into()];
    let engine = match MpvEngine::new(config) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("[render_probe] 创建引擎失败：{e}");
            std::process::exit(1);
        }
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([args.size.0 as f32, args.size.1 as f32])
            .with_title("wp-media-mpv render_probe"),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    let result = eframe::run_native(
        "wp-media-mpv render_probe",
        native_options,
        Box::new(move |cc| {
            ProbeApp::new(cc, engine, args.clone())
                .map(|app| Box::new(app) as Box<dyn eframe::App>)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })
        }),
    );
    if let Err(e) = result {
        eprintln!("[render_probe] eframe 运行失败：{e}");
        std::process::exit(1);
    }
}

/// 写 PPM（P6，RGB）——无需 image 依赖，便于用 ImageMagick/ffmpeg 校验像素内容。
/// 输入是 GL 的 bottom-up RGBA，这里翻成 top-down RGB。
fn write_ppm(path: &str, width: i32, height: i32, rgba_bottom_up: &[u8]) -> std::io::Result<()> {
    let (w, h) = (width.max(1) as usize, height.max(1) as usize);
    let mut out = Vec::with_capacity(w * h * 3 + 64);
    out.extend_from_slice(format!("P6\n{w} {h}\n255\n").as_bytes());
    for row in (0..h).rev() {
        for x in 0..w {
            let i = (row * w + x) * 4;
            out.extend_from_slice(&rgba_bottom_up[i..i + 3]);
        }
    }
    std::fs::write(path, out)
}
