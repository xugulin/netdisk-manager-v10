//! [`MpvEngine`]：把 libmpv 包装成 [`wp_media::MediaEngine`]。
//!
//! 线程模型：
//! - **调用线程（GUI）**：只做参数校验 + 往命令 channel 投递，永不在调用线程里阻塞 libmpv；
//! - **引擎线程**（`mpv-engine`）：独占 mpv 句柄，执行命令队列 + `mpv_wait_event` 事件循环 +
//!   读属性 → 更新共享快照 → 回调 [`EventHandler`]；
//! - **GL/渲染线程**（GUI 主线程）：只通过 `mpv_render_*` 访问 render context（见 `render` 模块），
//!   绝不调用普通 libmpv API（mpv 文档明确要求）。
//!
//! 这样 GUI 的 `command()` 永不阻塞、不会与 mpv 渲染互等（避免 render 线程死锁）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Instant;

use wp_core::{Error, Result};
use wp_media::{
    EventHandler, MediaEngine, MediaSource, PlaybackSnapshot, PlayerCommand, PlayerEvent,
    PlayerState, TrackKind,
};

use crate::ffi::{self, MpvApi, RawMpv};
use crate::props;
use crate::{MpvConfig, MpvLibrary};

/// 事件线程单次阻塞等待事件的上限（秒）。命令延迟上限 ≈ 该值。
const EVENT_WAIT_SECS: f64 = 0.02;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 引擎内部共享状态：事件线程写，UI 线程读。
struct Shared {
    snapshot: Mutex<PlaybackSnapshot>,
    handler: Mutex<Option<Arc<dyn Fn(PlayerEvent) + Send + Sync>>>,
    stopped: AtomicBool,
    last_error: Mutex<Option<String>>,
    /// 渲染桥是否已创建 mpv render context（Mode A）。
    ///
    /// `vo=libmpv` 时，**render context 必须在媒体加载前就绪**，否则 mpv 会以
    /// 无视频输出方式起播（只出声音、`video-params` 为空）。见 M0-E2 实测。
    render_ready: AtomicBool,
    /// 渲染桥就绪前收到的 open 请求（就绪后由引擎线程执行，对 GUI 完全透明）。
    pending_open: Mutex<Option<MediaSource>>,
}

impl Shared {
    fn new() -> Self {
        let mut snapshot = PlaybackSnapshot::default();
        snapshot.state = PlayerState::Idle;
        snapshot.speed = 1.0;
        snapshot.volume = 100.0;
        Self {
            snapshot: Mutex::new(snapshot),
            handler: Mutex::new(None),
            stopped: AtomicBool::new(false),
            last_error: Mutex::new(None),
            render_ready: AtomicBool::new(false),
            pending_open: Mutex::new(None),
        }
    }

    fn snapshot(&self) -> PlaybackSnapshot {
        lock(&self.snapshot).clone()
    }

    fn update<F: FnOnce(&mut PlaybackSnapshot)>(&self, f: F) {
        f(&mut lock(&self.snapshot));
    }

    /// 派发事件（先把 handler `Arc` 克隆出来再回调，避免回调里反调引擎造成死锁）。
    fn emit(&self, event: PlayerEvent) {
        let handler = lock(&self.handler).clone();
        if let Some(handler) = handler {
            handler(event);
        }
    }

    /// 只在状态真正变化时派发 [`PlayerEvent::StateChanged`]。
    fn set_state(&self, state: PlayerState) {
        let changed = {
            let mut snap = lock(&self.snapshot);
            if snap.state == state {
                false
            } else {
                snap.state = state;
                true
            }
        };
        if changed {
            self.emit(PlayerEvent::StateChanged(state));
        }
    }
}

/// 引擎与渲染桥共享的 mpv core（句柄 + 动态库 + 共享状态）。
///
/// 生命周期：`MpvEngine` 与 `RenderSource`（渲染桥持有）各持一个 `Arc`；
/// **最后一个 Arc 释放时才销毁 mpv 句柄**，保证「先 destroy 渲染桥、再 shutdown 引擎」
/// 的释放顺序天然安全。
pub(crate) struct MpvCore {
    // 字段顺序即析构顺序：**必须先销毁 mpv 句柄，再卸载动态库**。
    // （否则 RawMpv::drop 会调用已卸载库里的 mpv_terminate_destroy → SIGSEGV）
    raw: RawMpv,
    library: Arc<MpvLibrary>,
    shared: Arc<Shared>,
}

// SAFETY: `raw` 的句柄由引擎线程独占使用（client API 本身线程安全）；
// render 线程只在 `mpv_render_context_create` 时把它当 core 引用传一次，mpv 文档允许。
unsafe impl Sync for MpvCore {}

impl MpvCore {
    pub(crate) fn api(&self) -> &Arc<MpvApi> {
        self.library.api()
    }

    pub(crate) fn raw_handle(&self) -> *mut ffi::mpv_handle {
        self.raw.raw_handle()
    }

    pub(crate) fn library(&self) -> &Arc<MpvLibrary> {
        &self.library
    }
}

/// 渲染桥的 mpv 句柄来源。
///
/// 由 [`MpvEngine::render_source`] 取得，可 clone、`Send + Sync`，
/// 传给 `render::MpvRenderContext::new`。它持有 mpv core 的强引用，
/// 因此渲染桥存活期间 libmpv 不会被卸载。
#[derive(Clone)]
pub struct RenderSource {
    pub(crate) core: Arc<MpvCore>,
}

impl RenderSource {
    /// mpv 版本信息（含 `mpv v0.41.0` 串）。
    pub fn version(&self) -> crate::MpvVersion {
        self.core.library().version()
    }

    /// 实际加载的 libmpv 路径。
    pub fn library_path(&self) -> PathBuf {
        self.core.library().path().to_path_buf()
    }

    /// 渲染桥初始化完成后由 `render` 模块调用：唤醒引擎线程执行「就绪前排队」的 open。
    pub(crate) fn mark_render_ready(&self) {
        self.core.shared.render_ready.store(true, Ordering::Release);
    }
}

/// 引擎线程消息。
enum EngineMsg {
    Open(MediaSource),
    Command(PlayerCommand),
    /// 直接设置 mpv 属性/选项（高级逃生门：双字幕 secondary-sid、HLS 调参等；
    /// 先按属性设置，失败再退回 option 接口）。
    SetProperty(String, String),
    Shutdown,
}

/// libmpv 播放引擎。
pub struct MpvEngine {
    core: Arc<MpvCore>,
    tx: Sender<EngineMsg>,
    worker: Option<JoinHandle<()>>,
    config: MpvConfig,
    /// 是否必须等 render context 就绪后再加载媒体（Mode A + `vo=libmpv`）。
    defer_open_until_render: bool,
}

impl MpvEngine {
    /// 创建引擎：加载 libmpv → 设置选项 → 初始化 → 启动事件线程。
    ///
    /// 失败时返回可读错误（缺库 / 版本过旧 / 选项不支持），不会 panic。
    pub fn new(config: MpvConfig) -> Result<Self> {
        let library = crate::load_library(config.lib_path.as_deref())?;
        let raw = RawMpv::create(library.api().clone())?;

        // 选项必须在 mpv_initialize 之前设置。
        let mut options = props::build_options(&config);
        for (name, value) in &options {
            raw.set_option(name, value)
                .map_err(|e| Error::other(format!("初始化 mpv 选项 {name}={value} 失败：{e}")))?;
        }
        // 只有「最终 vo 是 libmpv」时才需要等渲染桥（headless 的 vo=null 不需要排队）。
        let final_vo = options
            .iter()
            .rev()
            .find(|(name, _)| name == "vo")
            .map(|(_, value)| value.as_str())
            .unwrap_or("");
        let defer_open_until_render =
            config.render_mode == crate::RenderMode::EmbeddedGl && final_vo == "libmpv";
        options.clear();
        raw.initialize()?;
        if let Some(version) = raw.mpv_version() {
            library.set_mpv_version(version);
        }

        let shared = Arc::new(Shared::new());
        let core = Arc::new(MpvCore {
            raw,
            library,
            shared,
        });

        let (tx, rx) = mpsc::channel();
        let worker_core = core.clone();
        let worker = std::thread::Builder::new()
            .name("mpv-engine".into())
            .spawn(move || Worker::new(worker_core).run(rx))
            .map_err(|e| Error::other(format!("创建 mpv 事件线程失败：{e}")))?;

        Ok(Self {
            core,
            tx,
            worker: Some(worker),
            config,
            defer_open_until_render,
        })
    }

    /// 引擎配置。
    pub fn config(&self) -> &MpvConfig {
        &self.config
    }

    /// 已加载的 libmpv（版本/路径诊断）。
    pub fn library(&self) -> &Arc<MpvLibrary> {
        self.core.library()
    }

    /// mpv 版本串，例如 `mpv v0.41.0（client API 2.5）`。
    pub fn mpv_version(&self) -> String {
        self.core.library().version().to_string()
    }

    /// 最近一次错误（引擎线程记录，便于 UI 诊断）。
    pub fn last_error(&self) -> Option<String> {
        lock(&self.core.shared.last_error).clone()
    }

    /// 渲染桥的 mpv 句柄来源（`render-gl` feature）。
    pub fn render_source(&self) -> Result<RenderSource> {
        if self.core.shared.stopped.load(Ordering::Acquire) {
            return Err(Error::other("mpv 引擎已关闭，无法创建渲染桥"));
        }
        Ok(RenderSource {
            core: self.core.clone(),
        })
    }

    /// 直接设置一个 mpv 属性/选项（高级逃生门，异步执行）。
    ///
    /// 用于 `wp_media::PlayerCommand` 尚未覆盖的能力（例：双字幕 `secondary-sid`、
    /// `secondary-sub-visibility`、HLS 调参）。设置了不存在的属性会在事件流里
    /// 收到可恢复的 [`PlayerEvent::Error`]。
    pub fn set_property(&self, name: impl Into<String>, value: impl Into<String>) -> Result<()> {
        self.enqueue(EngineMsg::SetProperty(name.into(), value.into()))
    }

    fn enqueue(&self, msg: EngineMsg) -> Result<()> {
        if self.core.shared.stopped.load(Ordering::Acquire) {
            return Err(Error::other("mpv 引擎已关闭"));
        }
        self.tx
            .send(msg)
            .map_err(|_| Error::other("mpv 事件线程已退出，命令未送达"))
    }
}

impl MediaEngine for MpvEngine {
    fn open(&mut self, source: MediaSource) -> Result<()> {
        // 先做廉价、可读的本地校验；真正的打开由引擎线程执行（不阻塞 UI）。
        match &source {
            MediaSource::Local(path) => {
                if !path.exists() {
                    return Err(Error::not_found(format!(
                        "本地文件不存在：{}",
                        path.display()
                    )));
                }
                if !path.is_file() {
                    return Err(Error::not_found(format!(
                        "不是普通文件：{}",
                        path.display()
                    )));
                }
            }
            MediaSource::Network(ns) => {
                if ns.url.trim().is_empty() {
                    return Err(Error::other(
                        "网络流 URL 为空（wp-stream 尚未给出代理地址？）",
                    ));
                }
            }
            MediaSource::BluRay(br) => {
                if !br.iso_or_dir.exists() {
                    return Err(Error::not_found(format!(
                        "蓝光源不存在：{}",
                        br.iso_or_dir.display()
                    )));
                }
            }
        }
        // Mode A：`vo=libmpv` 要求 render context 先于媒体加载就绪，否则 mpv 会
        // 以「无视频输出」起播（只出声音）。若播放页还没画过第一帧，就把 open
        // 排队，等渲染桥初始化后由引擎线程自动执行（对 GUI 透明）。
        if self.defer_open_until_render && !self.core.shared.render_ready.load(Ordering::Acquire) {
            *lock(&self.core.shared.pending_open) = Some(source);
            return Ok(());
        }
        self.enqueue(EngineMsg::Open(source))
    }

    fn command(&mut self, command: PlayerCommand) -> Result<()> {
        // 涉及文件路径的命令先校验，让 UI 立即得到可读错误。
        match &command {
            PlayerCommand::AddSubtitleFile { path, .. } => {
                if !path.is_file() {
                    return Err(Error::not_found(format!(
                        "字幕文件不存在：{}",
                        path.display()
                    )));
                }
            }
            PlayerCommand::Screenshot { path } => {
                if let Some(parent) = path.parent() {
                    if !parent.as_os_str().is_empty() && !parent.exists() {
                        return Err(Error::not_found(format!(
                            "截图目录不存在：{}",
                            parent.display()
                        )));
                    }
                }
            }
            _ => {}
        }
        self.enqueue(EngineMsg::Command(command))
    }

    fn snapshot(&self) -> PlaybackSnapshot {
        self.core.shared.snapshot()
    }

    fn set_event_handler(&mut self, handler: EventHandler) {
        let handler: Arc<dyn Fn(PlayerEvent) + Send + Sync> = Arc::from(handler);
        *lock(&self.core.shared.handler) = Some(handler);
    }

    fn shutdown(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.tx.send(EngineMsg::Shutdown);
            let _ = worker.join();
        }
        self.core.shared.stopped.store(true, Ordering::Release);
    }
}

impl Drop for MpvEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ============================ 引擎线程 ============================

/// 事件消费结果。
enum Flow {
    Continue,
    Exit,
}

/// 引擎线程：命令队列 + mpv 事件循环 + 属性刷新。
struct Worker {
    core: Arc<MpvCore>,
    // ---- 播放状态标志（用于推导 PlayerState）----
    loaded: bool,
    idle: bool,
    paused: bool,
    eof: bool,
    seeking: bool,
    paused_for_cache: bool,
    error: Option<String>,
    first_frame_emitted: bool,
    // ---- 其它 ----
    current_source: Option<MediaSource>,
    primary_cue: Option<(i64, String)>,
    secondary_cue: Option<(i64, String)>,
    last_position_event: f64,
    last_log_error: Option<String>,
    /// 网络流错误日志 → Error 事件的节流（HLS 分片 4xx / 链接过期）
    last_network_error_emit: Option<Instant>,
}

impl Worker {
    fn new(core: Arc<MpvCore>) -> Self {
        Self {
            core,
            loaded: false,
            idle: true,
            paused: false,
            eof: false,
            seeking: false,
            paused_for_cache: false,
            error: None,
            first_frame_emitted: false,
            current_source: None,
            primary_cue: None,
            secondary_cue: None,
            last_position_event: 0.0,
            last_log_error: None,
            last_network_error_emit: None,
        }
    }

    fn raw(&self) -> &RawMpv {
        &self.core.raw
    }

    fn emit(&self, event: PlayerEvent) {
        self.core.shared.emit(event);
    }

    fn emit_error(&self, message: String, recoverable: bool) {
        self.core.shared.emit(PlayerEvent::Error {
            message: message.clone(),
            recoverable,
        });
        self.core.shared.update(|s| s.error = Some(message.clone()));
        *lock(&self.core.shared.last_error) = Some(message);
    }

    fn run(mut self, rx: Receiver<EngineMsg>) {
        // 日志：默认只收 warn 及以上；错误日志用于 END_FILE 的错误兜底信息。
        let _ = self.raw().request_log_messages("warn");
        // 属性观察：不存在的属性（旧版 mpv）静默跳过。
        for name in props::OBSERVED_PROPERTIES {
            let _ = self.raw().observe_property_none(name);
        }

        loop {
            // 1) 优先处理命令（顺序与 UI 投递一致）
            let mut exit = false;
            loop {
                match rx.try_recv() {
                    Ok(EngineMsg::Open(source)) => self.handle_open(source),
                    Ok(EngineMsg::Command(command)) => self.handle_command(command),
                    Ok(EngineMsg::SetProperty(name, value)) => {
                        self.handle_set_property(&name, &value)
                    }
                    Ok(EngineMsg::Shutdown) => {
                        exit = true;
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        exit = true;
                        break;
                    }
                }
            }
            if exit {
                break;
            }
            // 2) 渲染桥就绪后执行排队的 open（Mode A 的加载顺序保证）
            if self.core.shared.render_ready.load(Ordering::Acquire) {
                let pending = lock(&self.core.shared.pending_open).take();
                if let Some(source) = pending {
                    self.handle_open(source);
                }
            }
            // 3) 处理 mpv 事件
            if let Some(event) = self.raw().wait_event(EVENT_WAIT_SECS) {
                if matches!(self.handle_event(event), Flow::Exit) {
                    break;
                }
            }
        }
        // 停止状态：后续 command()/open() 直接返回错误。
        self.core.shared.stopped.store(true, Ordering::Release);
    }

    // ---------------- 命令 ----------------

    fn handle_open(&mut self, source: MediaSource) {
        self.reset_for_open();
        // 网络源（含 HLS m3u8）：请求头用 **list 属性** 逐项设置。
        // mpv 会把 http-header-fields 透传给 FFmpeg 的 avformat `headers`，
        // 因此 m3u8 与每个 .ts 分片都会带上 Cookie/Referer/UA（夸克 HLS 必需）。
        if let MediaSource::Network(ns) = &source {
            let headers = props::http_header_fields_list(&ns.headers);
            if !headers.is_empty() {
                if let Err(e) = self
                    .raw()
                    .set_property_string_list("http-header-fields", &headers)
                {
                    // 回退：逗号拼接（仅兼容极老版本；值含逗号会拆碎）
                    let joined = props::http_header_fields(&ns.headers).unwrap_or_default();
                    if let Err(e2) = self
                        .raw()
                        .set_property_string("http-header-fields", &joined)
                    {
                        self.fail(format!(
                            "设置 HTTP 请求头失败（NODE_ARRAY：{e}；字符串回退：{e2}）"
                        ));
                        return;
                    }
                }
            }
            if let Some(ua) = props::user_agent_header(&ns.headers) {
                let _ = self.apply_source_option("user-agent", &ua);
            }
        }
        for (name, value) in props::source_options(&source) {
            if let Err(e) = self.apply_source_option(&name, &value) {
                self.fail(format!("设置媒体源选项 {name} 失败：{e}"));
                return;
            }
        }
        self.current_source = Some(source.clone());
        let args = props::loadfile_args(&source);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        if let Err(e) = self.raw().command(&refs) {
            self.fail(format!("加载媒体失败：{e}"));
        }
    }

    /// 运行期设置源相关选项：优先按属性设置（mpv 0.41 实测 `http-header-fields`/
    /// `bluray-device` 均可运行期设置），失败再退回 option 接口。
    fn apply_source_option(&self, name: &str, value: &str) -> Result<()> {
        match self.raw().set_property_string(name, value) {
            Ok(()) => Ok(()),
            Err(prop_err) => self
                .raw()
                .set_option(name, value)
                .map_err(|opt_err| Error::other(format!("{prop_err}；{opt_err}"))),
        }
    }

    fn reset_for_open(&mut self) {
        self.loaded = false;
        self.eof = false;
        self.seeking = false;
        self.paused_for_cache = false;
        self.paused = false;
        self.error = None;
        self.first_frame_emitted = false;
        self.primary_cue = None;
        self.secondary_cue = None;
        self.last_position_event = 0.0;
        self.core.shared.update(|s| {
            s.state = PlayerState::Opening;
            s.position_secs = 0.0;
            s.duration_secs = None;
            s.buffered.clear();
            s.tracks.clear();
            s.audio_track = None;
            s.subtitle_track = None;
            s.secondary_subtitle_visible = false;
            s.disc = None;
            s.hwdec = None;
            s.video_size = None;
            s.dropped_frames = 0;
            s.error = None;
        });
        self.emit(PlayerEvent::StateChanged(PlayerState::Opening));
    }

    fn handle_set_property(&mut self, name: &str, value: &str) {
        if let Err(e) = self.apply_source_option(name, value) {
            self.emit_error(format!("设置 mpv 属性 {name}={value} 失败：{e}"), true);
        }
    }

    fn handle_command(&mut self, command: PlayerCommand) {
        let outcome = self.execute(command);
        if let Err(e) = outcome {
            // 命令失败不改播放状态，只报可恢复错误（UI 可提示后继续）。
            self.emit_error(format!("命令失败：{e}"), true);
        }
    }

    fn execute(&mut self, command: PlayerCommand) -> Result<()> {
        match command {
            PlayerCommand::Play => self.set_paused(false),
            PlayerCommand::Pause => self.set_paused(true),
            PlayerCommand::TogglePause => self.set_paused(!self.paused),
            PlayerCommand::Stop => {
                self.raw().command(&["stop"])?;
                self.loaded = false;
                self.eof = false;
                self.seeking = false;
                self.current_source = None;
                self.core.shared.set_state(PlayerState::Idle);
                Ok(())
            }
            PlayerCommand::SeekAbsolute(target) => {
                let duration = self.core.shared.snapshot().duration_secs;
                let target = props::clamp_seek_target(target, duration);
                self.seek_to(&format!("{target:.3}"), "absolute+exact")
            }
            PlayerCommand::SeekRelative(delta) => {
                self.seek_to(&format!("{delta:.3}"), "relative+exact")
            }
            PlayerCommand::SetSpeed(speed) => self
                .raw()
                .set_property_f64("speed", speed.clamp(0.01, 100.0)),
            PlayerCommand::SetVolume(volume) => self
                .raw()
                .set_property_f64("volume", volume.clamp(0.0, 100.0)),
            PlayerCommand::SetMuted(muted) => self.raw().set_property_flag("mute", muted),
            PlayerCommand::SelectTrack { kind, id } => {
                let prop = props::track_kind_property(kind);
                if id < 0 {
                    self.raw().set_property_string(prop, "no")?;
                } else {
                    self.raw().set_property_i64(prop, id)?;
                }
                if let Ok(()) = Ok::<(), Error>(()) {
                    // 立即更新快照，UI 不用等 track-list 事件（事件随后会再校正）
                    self.core.shared.update(|s| match kind {
                        TrackKind::Audio => s.audio_track = (id >= 0).then_some(id),
                        TrackKind::Subtitle => s.subtitle_track = (id >= 0).then_some(id),
                        TrackKind::Video => {}
                    });
                }
                Ok(())
            }
            PlayerCommand::SetSecondarySubtitleVisible(visible) => {
                // FLAG 属性（实测 0.41 存在）；失败按可恢复错误上报。
                self.raw()
                    .set_property_flag("secondary-sub-visibility", visible)?;
                self.core
                    .shared
                    .update(|s| s.secondary_subtitle_visible = visible);
                Ok(())
            }
            PlayerCommand::SetSubtitleDelay(delay) => {
                self.raw().set_property_f64("sub-delay", delay)
            }
            PlayerCommand::AddSubtitleFile { path, select } => {
                let flags = if select { "select" } else { "auto" };
                let path = path.display().to_string();
                self.raw().command(&["sub-add", &path, flags])
            }
            PlayerCommand::ReloadSubtitles => self.raw().command(&["sub-reload"]),
            PlayerCommand::Screenshot { path } => {
                let path = path.display().to_string();
                self.raw().command(&["screenshot-to-file", &path, "video"])
            }
            PlayerCommand::SelectDiscTitle { id } => {
                let is_bluray = matches!(self.current_source, Some(MediaSource::BluRay(_)));
                if !is_bluray {
                    return Err(Error::unsupported("当前媒体不是蓝光，无法切换 title"));
                }
                let target = if id == 0 {
                    "bd://".to_string()
                } else {
                    format!("bd://{id}")
                };
                self.raw().command(&["loadfile", &target, "replace"])?;
                if let Some(MediaSource::BluRay(br)) = &mut self.current_source {
                    br.title_id = (id != 0).then_some(id);
                }
                Ok(())
            }
            PlayerCommand::RequestRepaint => {
                // 渲染桥经 update callback 自行触发 repaint；引擎侧无需动作。
                Ok(())
            }
        }
    }

    fn set_paused(&mut self, paused: bool) -> Result<()> {
        self.raw().set_property_flag("pause", paused)?;
        self.paused = paused;
        self.update_state();
        Ok(())
    }

    fn seek_to(&mut self, value: &str, flags: &str) -> Result<()> {
        self.raw().command(&["seek", value, flags])?;
        self.seeking = true;
        self.eof = false;
        self.update_state();
        Ok(())
    }

    // ---------------- 事件 ----------------

    fn handle_event(&mut self, event: ffi::MpvEventRef) -> Flow {
        match event.id() {
            ffi::MPV_EVENT_SHUTDOWN => return Flow::Exit,
            ffi::MPV_EVENT_PROPERTY_CHANGE => {
                if let Some(name) = event.property_name() {
                    self.on_property(&name);
                }
            }
            ffi::MPV_EVENT_START_FILE => self.on_start_file(),
            ffi::MPV_EVENT_FILE_LOADED => self.on_file_loaded(),
            ffi::MPV_EVENT_END_FILE => self.on_end_file(&event),
            ffi::MPV_EVENT_IDLE => {
                self.loaded = false;
                self.idle = true;
                self.eof = false;
                self.update_state();
            }
            ffi::MPV_EVENT_VIDEO_RECONFIG => {
                self.refresh_video_params();
                self.refresh_vo_configured();
                self.refresh_dropped_frames();
                self.refresh_track_list();
            }
            ffi::MPV_EVENT_SEEK => {
                self.seeking = true;
                self.update_state();
            }
            ffi::MPV_EVENT_PLAYBACK_RESTART => {
                self.seeking = false;
                self.refresh_eof();
                self.refresh_paused_for_cache();
                self.update_state();
            }
            ffi::MPV_EVENT_LOG_MESSAGE => {
                if let Some((level, _prefix, text)) = event.log_message() {
                    if level.starts_with("error") || level.starts_with("fatal") {
                        self.maybe_emit_network_error(&text);
                        self.last_log_error = Some(text);
                    }
                }
            }
            ffi::MPV_EVENT_QUEUE_OVERFLOW => {
                self.emit(PlayerEvent::Custom("mpv-event-queue-overflow".into()));
            }
            _ => {}
        }
        Flow::Continue
    }

    /// 网络流（含 HLS 分片）出错时把 mpv 的错误日志提升为可恢复 Error 事件。
    ///
    /// 背景：HLS 的分片由 FFmpeg demuxer 直接 HTTP 拉取（mpv 会把
    /// `http-header-fields` 通过 avformat `headers` 选项透传，见
    /// `stream/stream_lavf.c::mp_setup_av_network_options`）。Cookie 过期/链接过期时
    /// 现象是**分片 4xx**，mpv 一般重试数次后报错；这里把这类日志及时转成
    /// [`PlayerEvent::Error`]，UI 才能提示「链接过期，请刷新」。
    ///
    /// 同一条错误 3 秒内只派发一次，避免网络抖动刷屏。
    fn maybe_emit_network_error(&mut self, text: &str) {
        if !matches!(self.current_source, Some(MediaSource::Network(_))) {
            return;
        }
        let t = text.to_ascii_lowercase();
        let network_like = t.contains("http error")
            || t.contains("failed to open")
            || t.contains("connection")
            || t.contains("timed out")
            || t.contains("403")
            || t.contains("404")
            || t.contains("410");
        if !network_like {
            return;
        }
        let now = Instant::now();
        if let Some(last) = self.last_network_error_emit {
            if now.duration_since(last).as_secs() < 3 {
                return;
            }
        }
        self.last_network_error_emit = Some(now);
        self.emit(PlayerEvent::Error {
            message: format!("网络流错误（链接可能已过期，可尝试刷新）：{text}"),
            recoverable: true,
        });
    }

    fn on_start_file(&mut self) {
        self.loaded = false;
        self.eof = false;
        self.seeking = false;
        self.error = None;
        self.first_frame_emitted = false;
        self.update_state();
    }

    fn on_file_loaded(&mut self) {
        self.loaded = true;
        self.idle = false;
        self.error = None;
        let duration = self
            .raw()
            .get_property_f64("duration")
            .filter(|d| d.is_finite() && *d >= 0.0);
        let tracks = self
            .raw()
            .get_property_json("track-list")
            .map(|json| props::parse_track_list(&json))
            .unwrap_or_default();
        let video_size = self
            .raw()
            .get_property_json("video-params")
            .and_then(|json| props::parse_video_params(&json));
        let audio_track = props::selected_track_id(&tracks, TrackKind::Audio);
        let subtitle_track = props::selected_track_id(&tracks, TrackKind::Subtitle);
        let hwdec = self.raw().get_property_string("hwdec-current");
        self.core.shared.update(|s| {
            s.duration_secs = duration;
            s.tracks = tracks.clone();
            s.audio_track = audio_track;
            s.subtitle_track = subtitle_track;
            s.video_size = video_size;
            s.hwdec = hwdec;
        });
        self.emit(PlayerEvent::Opened {
            duration_secs: duration,
            tracks,
            video_size,
        });
        if let Some(duration) = duration {
            self.emit(PlayerEvent::DurationChanged(Some(duration)));
        }
        self.refresh_vo_configured();
        self.update_state();
    }

    fn on_end_file(&mut self, event: &ffi::MpvEventRef) {
        let Some((reason, error)) = event.end_file() else {
            return;
        };
        match reason {
            ffi::MPV_END_FILE_REASON_EOF => {
                self.loaded = true;
                self.eof = true;
                self.update_state();
                self.emit(PlayerEvent::EndOfFile);
            }
            ffi::MPV_END_FILE_REASON_STOP => {
                self.loaded = false;
                self.eof = false;
                self.seeking = false;
                self.update_state();
            }
            ffi::MPV_END_FILE_REASON_ERROR => {
                let message = match self.last_log_error.take() {
                    Some(log) => format!("播放失败：{log}"),
                    None => format!("播放失败：{}", self.core.api().error_string(error)),
                };
                self.fail(message);
            }
            _ => {}
        }
    }

    // ---------------- 属性刷新 ----------------

    fn on_property(&mut self, name: &str) {
        match name {
            "time-pos" => self.refresh_position(),
            "duration" => self.refresh_duration(),
            "pause" => {
                if let Some(paused) = self.raw().get_property_flag("pause") {
                    self.paused = paused;
                    self.update_state();
                }
            }
            "eof-reached" => self.refresh_eof(),
            "idle-active" => {
                if let Some(idle) = self.raw().get_property_flag("idle-active") {
                    self.idle = idle;
                    self.update_state();
                }
            }
            "seeking" => {
                if let Some(seeking) = self.raw().get_property_flag("seeking") {
                    self.seeking = seeking;
                    self.update_state();
                }
            }
            "paused-for-cache" => self.refresh_paused_for_cache(),
            "cache-buffering-state" => {
                if let Some(state) = self.raw().get_property_i64("cache-buffering-state") {
                    self.emit(PlayerEvent::Buffering(state.clamp(0, 100) as u8));
                }
            }
            "track-list" => self.refresh_track_list(),
            "sub-text" | "sub-start" => self.refresh_cue(false),
            "secondary-sub-text" | "secondary-sub-start" => self.refresh_cue(true),
            "hwdec-current" => {
                let hwdec = self.raw().get_property_string("hwdec-current");
                self.core.shared.update(|s| s.hwdec = hwdec);
            }
            "video-params" => self.refresh_video_params(),
            "vo-configured" => self.refresh_vo_configured(),
            "frame-drop-count" => self.refresh_dropped_frames(),
            "demuxer-cache-state" => {
                if let Some(json) = self.raw().get_property_json("demuxer-cache-state") {
                    let buffered = props::parse_demuxer_cache_state(&json);
                    self.core.shared.update(|s| s.buffered = buffered);
                }
            }
            "disc-menu-active" => {
                if let Some(active) = self.raw().get_property_flag("disc-menu-active") {
                    if active {
                        self.emit(PlayerEvent::Custom("disc-menu-active=true".into()));
                    }
                }
            }
            "speed" => {
                if let Some(speed) = self.raw().get_property_f64("speed") {
                    self.core.shared.update(|s| s.speed = speed);
                }
            }
            "volume" => {
                if let Some(volume) = self.raw().get_property_f64("volume") {
                    self.core.shared.update(|s| s.volume = volume);
                }
            }
            "mute" => {
                if let Some(muted) = self.raw().get_property_flag("mute") {
                    self.core.shared.update(|s| s.muted = muted);
                }
            }
            _ => {}
        }
    }

    fn refresh_position(&mut self) {
        let Some(position) = self.raw().get_property_f64("time-pos") else {
            return;
        };
        self.core.shared.update(|s| s.position_secs = position);
        if props::position_delta_worth_event(self.last_position_event, position) {
            self.last_position_event = position;
            self.emit(PlayerEvent::PositionChanged(position));
        }
    }

    fn refresh_duration(&mut self) {
        let Some(duration) = self.raw().get_property_f64("duration") else {
            return;
        };
        if !duration.is_finite() || duration <= 0.0 {
            return;
        }
        let changed = {
            let mut snapshot = lock(&self.core.shared.snapshot);
            if snapshot.duration_secs == Some(duration) {
                false
            } else {
                snapshot.duration_secs = Some(duration);
                true
            }
        };
        if changed {
            self.emit(PlayerEvent::DurationChanged(Some(duration)));
        }
    }

    fn refresh_eof(&mut self) {
        if let Some(eof) = self.raw().get_property_flag("eof-reached") {
            let was = self.eof;
            self.eof = eof;
            self.update_state();
            if eof && !was {
                self.emit(PlayerEvent::EndOfFile);
            }
        }
    }

    fn refresh_paused_for_cache(&mut self) {
        if let Some(paused) = self.raw().get_property_flag("paused-for-cache") {
            self.paused_for_cache = paused;
            self.update_state();
        }
    }

    fn refresh_track_list(&mut self) {
        let Some(json) = self.raw().get_property_json("track-list") else {
            return;
        };
        let tracks = props::parse_track_list(&json);
        let audio_track = props::selected_track_id(&tracks, TrackKind::Audio);
        let subtitle_track = props::selected_track_id(&tracks, TrackKind::Subtitle);
        let changed = {
            let mut snapshot = lock(&self.core.shared.snapshot);
            let changed = snapshot.tracks != tracks;
            snapshot.tracks = tracks.clone();
            snapshot.audio_track = audio_track;
            snapshot.subtitle_track = subtitle_track;
            changed
        };
        if changed {
            self.emit(PlayerEvent::TracksChanged(tracks));
        }
    }

    fn refresh_cue(&mut self, secondary: bool) {
        let (text_property, start_property) = if secondary {
            ("secondary-sub-text", "secondary-sub-start")
        } else {
            ("sub-text", "sub-start")
        };
        let text = self
            .raw()
            .get_property_string(text_property)
            .unwrap_or_default();
        let start_ms = props::secs_to_ms(
            self.raw()
                .get_property_f64(start_property)
                .unwrap_or_default(),
        );
        let prev = if secondary {
            &mut self.secondary_cue
        } else {
            &mut self.primary_cue
        };
        if props::should_emit_cue(prev, start_ms, &text) {
            self.emit(PlayerEvent::SubtitleCue {
                start_ms,
                text,
                secondary,
            });
        }
    }

    fn refresh_video_params(&mut self) {
        let Some(json) = self.raw().get_property_json("video-params") else {
            self.core.shared.update(|s| s.video_size = None);
            return;
        };
        let size = props::parse_video_params(&json);
        let changed = {
            let mut snapshot = lock(&self.core.shared.snapshot);
            let changed = snapshot.video_size != size;
            snapshot.video_size = size;
            changed
        };
        if changed && size.is_some() {
            self.refresh_vo_configured();
        }
    }

    fn refresh_vo_configured(&mut self) {
        if self.first_frame_emitted {
            return;
        }
        let configured = self
            .raw()
            .get_property_flag("vo-configured")
            .unwrap_or(false);
        let has_video = lock(&self.core.shared.snapshot).video_size.is_some();
        if configured && has_video {
            self.first_frame_emitted = true;
            self.emit(PlayerEvent::FirstFrame);
        }
    }

    fn refresh_dropped_frames(&mut self) {
        if let Some(count) = self.raw().get_property_i64("frame-drop-count") {
            self.core
                .shared
                .update(|s| s.dropped_frames = count.max(0) as u64);
        }
    }

    // ---------------- 状态推导 ----------------

    fn compute_state(&self) -> PlayerState {
        if self.error.is_some() {
            return PlayerState::Error;
        }
        if self.seeking {
            return PlayerState::Seeking;
        }
        if self.eof {
            return PlayerState::Ended;
        }
        if !self.loaded {
            return if self.idle {
                PlayerState::Idle
            } else {
                PlayerState::Opening
            };
        }
        if self.paused_for_cache {
            return PlayerState::Buffering;
        }
        if self.paused {
            PlayerState::Paused
        } else {
            PlayerState::Playing
        }
    }

    fn update_state(&self) {
        self.core.shared.set_state(self.compute_state());
    }

    fn fail(&mut self, message: String) {
        self.error = Some(message.clone());
        self.core.shared.update(|s| {
            s.state = PlayerState::Error;
            s.error = Some(message.clone());
        });
        *lock(&self.core.shared.last_error) = Some(message.clone());
        self.emit(PlayerEvent::Error {
            message,
            recoverable: false,
        });
        self.emit(PlayerEvent::StateChanged(PlayerState::Error));
    }
}

// ============================ 测试 ============================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// 等待某个条件成立（最多 `timeout`），条件由快照/事件计数驱动。
    fn wait_until<F: FnMut() -> bool>(timeout: Duration, mut cond: F) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    /// 生成 2 秒测试视频（需要 ffmpeg；无 ffmpeg 时跳过，返回 None）。
    fn make_test_video(dir: &std::path::Path) -> Option<PathBuf> {
        let path = dir.join("wp-mpv-test.mp4");
        if path.exists() {
            return Some(path);
        }
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=320x240:rate=24",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(&path)
            .status()
            .ok()?;
        status.success().then_some(path)
    }

    #[test]
    fn engine_constructs_or_errors_without_panic() {
        let extra = if cfg!(target_os = "linux") {
            vec!["vo=null".to_string(), "ao=null".to_string()]
        } else {
            vec![]
        };
        let config = MpvConfig {
            extra_options: extra,
            ..Default::default()
        };
        match MpvEngine::new(config) {
            Ok(mut engine) => {
                assert!(engine.mpv_version().contains("client API"));
                assert_eq!(engine.snapshot().state, PlayerState::Idle);
                engine.shutdown();
            }
            Err(e) => {
                // 无 libmpv 的环境：必须是可读错误而不是 panic
                let msg = e.to_string();
                assert!(msg.contains("libmpv"), "错误应提到 libmpv：{msg}");
            }
        }
    }

    #[test]
    fn open_missing_local_file_fails_fast() {
        let extra = vec!["vo=null".to_string(), "ao=null".to_string()];
        let config = MpvConfig {
            extra_options: extra,
            ..Default::default()
        };
        let Ok(mut engine) = MpvEngine::new(config) else {
            return; // 无 libmpv 环境跳过
        };
        let err = engine
            .open(MediaSource::Local(PathBuf::from(
                "/definitely/not/here.mp4",
            )))
            .expect_err("不存在的本地文件应返回错误");
        assert!(err.to_string().contains("不存在"));
        engine.shutdown();
    }

    /// 真实播放冒烟测试（headless：vo=null / ao=null）。手动运行：
    /// `cargo test -p wp-media-mpv -- --ignored --nocapture`
    #[test]
    #[ignore = "需要本机 libmpv + ffmpeg；headless 播放 2s 测试片验证事件/快照"]
    fn plays_generated_video_headless() {
        let dir = std::env::temp_dir().join("wp-media-mpv-test");
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        let Some(video) = make_test_video(&dir) else {
            eprintln!("跳过：未找到 ffmpeg");
            return;
        };

        let config = MpvConfig {
            hwdec: "no".into(),
            extra_options: vec!["vo=null".into(), "ao=null".into(), "cache=no".into()],
            ..Default::default()
        };
        let mut engine = MpvEngine::new(config).expect("引擎创建应成功");
        let events: Arc<Mutex<Vec<PlayerEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        engine.set_event_handler(Box::new(move |event| {
            lock(&sink).push(event);
        }));

        engine
            .open(MediaSource::Local(video))
            .expect("打开本地文件");
        let opened = wait_until(Duration::from_secs(10), || {
            lock(&events)
                .iter()
                .any(|e| matches!(e, PlayerEvent::Opened { .. }))
        });
        assert!(
            opened,
            "10s 内应收到 Opened 事件；当前事件：{:#?}",
            lock(&events)
        );

        let snap = engine.snapshot();
        assert!(
            snap.duration_secs.unwrap_or(0.0) > 1.0,
            "时长应 > 1s：{snap:?}"
        );
        assert!(!snap.tracks.is_empty(), "应有轨道：{snap:?}");

        let got_first_frame = wait_until(Duration::from_secs(10), || {
            lock(&events)
                .iter()
                .any(|e| matches!(e, PlayerEvent::FirstFrame))
        });
        assert!(got_first_frame, "应收到 FirstFrame 事件");

        // 暂停 / 播放 / seek / 音量 / 倍速 全链路
        engine.command(PlayerCommand::Pause).expect("暂停命令");
        assert!(wait_until(Duration::from_secs(5), || {
            engine.snapshot().state == PlayerState::Paused
        }));
        engine
            .command(PlayerCommand::SeekAbsolute(1.0))
            .expect("seek 命令");
        engine
            .command(PlayerCommand::SetVolume(50.0))
            .expect("音量命令");
        engine
            .command(PlayerCommand::SetSpeed(1.5))
            .expect("倍速命令");
        engine.command(PlayerCommand::Play).expect("播放命令");
        assert!(wait_until(Duration::from_secs(10), || {
            let snap = engine.snapshot();
            (snap.position_secs - 1.0).abs() < 1.0 && snap.speed > 1.2
        }));
        assert_eq!(engine.snapshot().volume, 50.0);

        // 播到结尾应收到 EndOfFile
        let ended = wait_until(Duration::from_secs(15), || {
            engine.snapshot().state == PlayerState::Ended
                || lock(&events)
                    .iter()
                    .any(|e| matches!(e, PlayerEvent::EndOfFile))
        });
        assert!(
            ended,
            "应收到 EndOfFile/Ended；快照：{:?}",
            engine.snapshot()
        );

        engine.shutdown();
        assert!(
            engine.command(PlayerCommand::Play).is_err(),
            "关闭后命令应报错"
        );
    }
}
