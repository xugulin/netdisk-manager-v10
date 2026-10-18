//! 播放器页（M1 / T5 · M2 / T8·T10 · M5 / T24·T26 · M4 / T21）：页面布局 + 引擎接入层。
//!
//! 结构：
//! - 本文件：`PlayerUi`（App 持有的播放器状态 + 引擎句柄）、页面布局、控制条、状态行、
//!   快捷键动作、`App` 侧接线（`open_player` / `open_player_remote_entry` / 事件处理）；
//! - [`overlay`]：视频区 16:9 黑边 + Mock 演示帧 + OSD + 字幕/弹幕覆盖层绘制；
//! - [`subtitle_panel`]：字幕轨/主副字幕/延迟/样式/外挂导入；
//! - [`danmaku_panel`]：开关/透明度/速度/密度/区域/屏蔽词/时间轴密度；
//! - [`ai_panel`]：Provider 列表/优先级/翻译占位/隐私提示。
//!
//! 引擎接入（当前 M1 阶段）：
//! - 默认 [`wp_media::MockEngine`]，UI 全流程可交互/可截图；
//! - 设置环境变量 `WP_PLAYER_ENGINE=mpv` 时尝试 `wp_media_mpv::MpvEngine::new(MpvConfig::default())`，
//!   失败自动回退 Mock 并在状态行提示；
//! - 真实帧渲染（media-core 已冻结接口）：`MpvRenderContext::new(source, proc_address, egui_ctx)`
//!   后每帧 `rc.paint_video(ui, size)` 贴到视频区；App 退出时 `rc.destroy(gl)` + `engine.shutdown()`。
//!   该分支等 media-core E2 通过后由 Lead 决定切换默认（本页已预留 `backend` 与绘制分支）。

mod ai_panel;
mod danmaku_panel;
mod overlay;
mod subtitle_panel;

pub use ai_panel::AiUi;
pub use danmaku_panel::DanmakuUi;
pub use subtitle_panel::SubtitleUi;

use crate::app::{App, Event, FilePickerPurpose, Page, ToastKind};
use crate::theme::{self, Theme};
use crate::widgets;
use egui::{Align, Color32, Layout, RichText, Sense, Ui, Vec2};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wp_core::model::{DriveKind, RemoteEntry};
use wp_media::{
    EventHandler, MediaEngine, MediaSource, NetworkSource, PlaybackSnapshot, PlayerCommand,
    PlayerEvent, PlayerState, TrackInfo, TrackKind,
};
use wp_media_mpv::render::{GetProcAddress, MpvRenderContext};
use wp_media_mpv::{MpvConfig, MpvEngine};

/// 播放引擎类型（UI 徽标/绘制分支用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EngineBackend {
    /// 演示引擎（无真实解码；测试图 + 模拟时钟）。
    #[default]
    Mock,
    /// libmpv 真实引擎。
    Mpv,
}

/// 侧栏面板标签。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayerPanel {
    #[default]
    Subtitles,
    Danmaku,
    Ai,
}

impl PlayerPanel {
    pub fn label(self) -> &'static str {
        match self {
            PlayerPanel::Subtitles => "字幕",
            PlayerPanel::Danmaku => "弹幕",
            PlayerPanel::Ai => "AI",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            PlayerPanel::Subtitles => "💬",
            PlayerPanel::Danmaku => "🗨",
            PlayerPanel::Ai => "🤖",
        }
    }
}

/// UI 动作：页面/快捷键统一收集后由 `App` 应用（避免 UI 闭包里与 `&mut App` 冲突）。
#[derive(Debug, Clone)]
pub enum PlayerAction {
    TogglePlay,
    Stop,
    SeekRelative(f64),
    SeekAbsolute(f64),
    Command(PlayerCommand),
    ToggleFullscreen,
    ExitFullscreen,
    ToggleSubtitle,
    ToggleDanmaku,
    Screenshot,
    OpenLocalFile,
    OpenSelectedRemote,
    OpenDemo,
    CloseMedia,
    SetVolume(f64),
    ToggleMute,
    SetSpeed(f64),
    SwitchPanel(PlayerPanel),
    /// 选择外挂字幕文件（复用内置 FilePicker）。
    PickSubtitleFile,
    Toast(ToastKind, String),
}

/// 音/视频扩展名（双击/右键/文件选择器判定）。
pub const MEDIA_EXTS: &[&str] = &[
    // 视频
    "mp4", "mkv", "mov", "avi", "webm", "flv", "ts", "m2ts", "mts", "wmv", "mpg", "mpeg", "m4v",
    "3gp", "ogv", "rmvb", "iso", // 音频
    "mp3", "flac", "aac", "m4a", "ogg", "wav", "opus", "wma", "ape", "alac",
];

/// 字幕扩展名（文件选择器「浏览字幕」用）。
pub const SUBTITLE_EXTS: &[&str] = &["srt", "ass", "ssa", "vtt", "sub", "lrc"];

/// 是否为可播放的媒体文件（按扩展名判断，大小写不敏感）。
pub fn is_media_file(name: &str) -> bool {
    file_ext(name).is_some_and(|ext| MEDIA_EXTS.contains(&ext.as_str()))
}

/// 是否为字幕文件。
pub fn is_subtitle_file(name: &str) -> bool {
    file_ext(name).is_some_and(|ext| SUBTITLE_EXTS.contains(&ext.as_str()))
}

fn file_ext(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// 速度预设。
pub const SPEED_PRESETS: [f64; 8] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];

/// 播放器页状态：引擎句柄 + 快照 + 各面板状态。
///
/// 引擎事件通过 `event_tx` 回传到 App 主事件循环（`Event::Player`），本结构**不**在 UI 线程
/// 之外被访问；`tick` 每帧拉取一次 `snapshot()`（trait 约定低开销）。
pub struct PlayerUi {
    /// Mock 引擎实例（演示媒体 / 无 libmpv 回退时使用）
    pub engine: Option<Box<dyn MediaEngine>>,
    /// 真实 libmpv 引擎（启动时创建一次并复用；None=不可用或强制 Mock）
    pub real_engine: Option<Box<dyn MediaEngine>>,
    /// Mode A 渲染桥（与 real_engine 绑定；视频区用 `paint_rect` 画真实帧）
    pub render: Option<MpvRenderContext>,
    /// 真实引擎不可用原因（状态行/空态提示）
    pub real_engine_error: Option<String>,
    /// 是否已打开媒体（真实引擎常驻，因此不能用 `engine.is_some()` 判断）
    pub media_open: bool,
    pub backend: EngineBackend,
    event_tx: Arc<Mutex<Sender<Event>>>,
    pub ctx: Option<egui::Context>,
    pub source: Option<MediaSource>,
    pub source_label: String,
    pub snapshot: PlaybackSnapshot,
    /// 播放位置（秒；Mock 由 UI 时钟推进，真实引擎由 PositionChanged 事件更新）。
    pub position: f64,
    /// 媒体时长（秒；未知为 0）。
    pub duration: f64,
    pub volume: f64,
    pub muted: bool,
    pub speed: f64,
    /// 已缓冲百分比（0~100）。
    pub buffering_pct: u8,
    pub playing: bool,
    /// 是否已收到首帧。
    pub first_frame: bool,
    /// 伪全屏（隐藏顶栏/侧栏/页内 chrome）+ 同步 ViewportCommand::Fullscreen。
    pub fullscreen: bool,
    /// 当前侧栏面板。
    pub panel: PlayerPanel,
    /// 引擎回退/诊断提示（状态行展示）。
    pub engine_note: Option<String>,
    /// 最近错误。
    pub error: Option<String>,
    last_tick: Instant,
    pub subtitle: SubtitleUi,
    pub danmaku: DanmakuUi,
    pub ai: AiUi,
    /// 演示弹幕源数据（真实弹幕加载后替换为 DanmakuDoc；当前供 wp-danmaku 布局引擎使用）
    pub demo_comments: Vec<wp_danmaku::Comment>,
    /// wp-danmaku 布局结果（T23 车道/密度/坐标引擎）：按配置变化重建，覆盖层二分查询活跃弹幕
    pub danmaku_timeline: wp_danmaku::PlacedTimeline,
    /// 上次布局指纹（分辨率 + 弹幕配置），变化时重建时间轴
    danmaku_layout_key: (u32, u32, usize, u32, u32, u32, u32),
}

impl PlayerUi {
    pub fn new(event_tx: Sender<Event>) -> Self {
        Self {
            engine: None,
            real_engine: None,
            render: None,
            real_engine_error: None,
            media_open: false,
            backend: EngineBackend::Mock,
            event_tx: Arc::new(Mutex::new(event_tx)),
            ctx: None,
            source: None,
            source_label: String::new(),
            snapshot: PlaybackSnapshot::default(),
            position: 0.0,
            duration: 0.0,
            volume: 100.0,
            muted: false,
            speed: 1.0,
            buffering_pct: 0,
            playing: false,
            first_frame: false,
            fullscreen: false,
            panel: PlayerPanel::Subtitles,
            engine_note: None,
            error: None,
            last_tick: Instant::now(),
            subtitle: SubtitleUi::default(),
            danmaku: DanmakuUi::default(),
            ai: AiUi::default(),
            demo_comments: overlay::demo_comments(),
            danmaku_timeline: wp_danmaku::PlacedTimeline::default(),
            danmaku_layout_key: (0, 0, 0, 0, 0, 0, 0),
        }
    }

    pub fn has_media(&self) -> bool {
        self.media_open
    }

    /// 当前媒体是否由真实 libmpv 引擎播放（决定视频区渲染路径）。
    pub fn is_real_playback(&self) -> bool {
        self.media_open && self.backend == EngineBackend::Mpv && self.render.is_some()
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// 源显示名（空态提示用）。
    pub fn media_name(&self) -> &str {
        &self.source_label
    }

    // ─────────────────────── 引擎生命周期 ───────────────────────

    /// 启动时尝试创建真实 libmpv 引擎 + Mode A 渲染桥（失败回退演示引擎并记录原因）。
    ///
    /// 由 `App::new` 在 eframe `CreationContext` 里调用一次；之后所有真实媒体复用该引擎，
    /// 渲染桥绑定同一 mpv core（不能重复创建/重启引擎，否则 render context 失效）。
    pub fn init_real_engine(
        &mut self,
        get_proc_address: Option<GetProcAddress>,
        egui_ctx: egui::Context,
    ) {
        self.ctx = Some(egui_ctx.clone());
        // 显式强制 Mock（无 libmpv 环境/CI 截图）
        if std::env::var("WP_PLAYER_ENGINE")
            .map(|v| v.eq_ignore_ascii_case("mock"))
            .unwrap_or(false)
        {
            self.real_engine_error = Some("WP_PLAYER_ENGINE=mock：强制使用演示引擎".into());
            return;
        }
        let Some(get_proc_address) = get_proc_address else {
            self.real_engine_error =
                Some("当前渲染后端未提供 GL get_proc_address（需要 eframe glow 后端）".into());
            return;
        };
        // 运行期调参（无头验收/排障用）：ao=null 避开无声卡环境；hwdec 可显式覆盖
        let mut mpv_cfg = MpvConfig::default();
        // UI 自带控制条/时间码：关闭 mpv 自己的 OSD（否则播放页会叠一层 mpv 时间/文件名）
        mpv_cfg.extra_options.push("osd-level=0".into());
        if std::env::var("WP_PLAYER_AO_NULL").is_ok() {
            mpv_cfg.extra_options.push("ao=null".into());
        }
        if let Ok(hwdec) = std::env::var("WP_PLAYER_HWDEC") {
            if !hwdec.trim().is_empty() {
                mpv_cfg.hwdec = hwdec;
            }
        }
        let mut engine = match MpvEngine::new(mpv_cfg) {
            Ok(engine) => engine,
            Err(err) => {
                self.real_engine_error = Some(format!("libmpv 不可用：{err}"));
                return;
            }
        };
        let source = match engine.render_source() {
            Ok(source) => source,
            Err(err) => {
                self.real_engine_error = Some(format!("libmpv 渲染源不可用：{err}"));
                engine.shutdown();
                return;
            }
        };
        let render = match MpvRenderContext::new(source, get_proc_address, egui_ctx) {
            Ok(render) => render,
            Err(err) => {
                self.real_engine_error = Some(format!("创建 Mode A 渲染桥失败：{err}"));
                engine.shutdown();
                return;
            }
        };
        engine.set_event_handler(make_event_handler(self.event_tx.clone(), self.ctx.clone()));
        self.real_engine = Some(Box::new(engine));
        self.render = Some(render);
    }

    /// 真实引擎是否可用（状态行/引擎徽标展示用）。
    pub fn real_engine_available(&self) -> bool {
        self.real_engine.is_some() && self.render.is_some()
    }

    /// 打开媒体源（优先真实引擎；无 libmpv 回退演示引擎）。
    pub fn open_source(&mut self, source: MediaSource, label: String) {
        self.open_source_with(source, label, false);
    }

    /// 打开演示媒体（强制 Mock：假路径/截图回归不能走真实解码）。
    pub fn open_demo_source(&mut self, source: MediaSource, label: String) {
        self.open_source_with(source, label, true);
    }

    fn open_source_with(&mut self, source: MediaSource, label: String, force_mock: bool) {
        // 停掉当前媒体（真实引擎保持存活，供下次复用）
        {
            if let Some(engine) = self.active_engine() {
                let _ = engine.command(PlayerCommand::Stop);
            }
        }
        self.media_open = false;
        self.snapshot = PlaybackSnapshot::default();
        self.position = 0.0;
        self.duration = 0.0;
        self.buffering_pct = 0;
        self.first_frame = false;
        self.playing = false;
        self.error = None;
        self.engine_note = None;
        self.source = Some(source.clone());
        self.source_label = label;
        self.subtitle.reset();

        let use_mock = force_mock || self.real_engine.is_none();
        if use_mock {
            self.backend = EngineBackend::Mock;
            self.engine = None;
            let mut mock = wp_media::MockEngine::new();
            mock.set_event_handler(make_event_handler(self.event_tx.clone(), self.ctx.clone()));
            match mock.open(source) {
                Ok(()) => {
                    self.engine = Some(Box::new(mock));
                    self.media_open = true;
                    self.last_tick = Instant::now();
                    // Mock：从 2s 起播，让演示字幕/弹幕在截图时可见。
                    self.position = 2.0;
                    self.playing = true;
                    if self.real_engine.is_none() {
                        self.engine_note = Some(match &self.real_engine_error {
                            Some(err) => {
                                format!("真实引擎不可用，已回退演示引擎（MockEngine）：{err}")
                            }
                            None => "未检测到可用 libmpv，已回退演示引擎（MockEngine）".into(),
                        });
                    }
                }
                Err(err) => self.error = Some(err.to_string()),
            }
        } else {
            self.backend = EngineBackend::Mpv;
            let opened = match self.real_engine.as_mut() {
                Some(engine) => engine.open(source),
                None => unreachable!(),
            };
            match opened {
                Ok(()) => {
                    self.media_open = true;
                    self.playing = true;
                    self.last_tick = Instant::now();
                }
                Err(err) => self.error = Some(err.to_string()),
            }
        }
    }

    /// 当前生效的引擎（真实=MpvEngine，演示=MockEngine）。
    fn active_engine(&mut self) -> Option<&mut Box<dyn MediaEngine>> {
        match self.backend {
            EngineBackend::Mpv => self.real_engine.as_mut(),
            EngineBackend::Mock => self.engine.as_mut(),
        }
    }

    fn active_engine_ref(&self) -> Option<&Box<dyn MediaEngine>> {
        match self.backend {
            EngineBackend::Mpv => self.real_engine.as_ref(),
            EngineBackend::Mock => self.engine.as_ref(),
        }
    }

    /// 关闭当前媒体（回到空态；保留面板偏好与真实引擎/渲染桥）。
    pub fn close(&mut self) {
        if let Some(engine) = self.active_engine() {
            let _ = engine.command(PlayerCommand::Stop);
        }
        if self.backend == EngineBackend::Mock {
            self.engine = None;
        }
        self.media_open = false;
        self.backend = EngineBackend::Mock;
        self.source = None;
        self.source_label.clear();
        self.snapshot = PlaybackSnapshot::default();
        self.position = 0.0;
        self.duration = 0.0;
        self.buffering_pct = 0;
        self.playing = false;
        self.first_frame = false;
        self.fullscreen = false;
        self.volume = 100.0;
        self.muted = false;
        self.speed = 1.0;
        self.error = None;
        self.engine_note = None;
        self.subtitle.reset();
    }

    /// 退出时释放：先销毁渲染桥（释放 GL 资源）→ 再停真实引擎（顺序反了会崩/泄漏）。
    pub fn shutdown_all(&mut self, gl: Option<&eframe::glow::Context>) {
        self.media_open = false;
        if let Some(render) = self.render.take() {
            render.destroy(gl);
        }
        if let Some(mut engine) = self.engine.take() {
            engine.shutdown();
        }
        if let Some(mut engine) = self.real_engine.take() {
            engine.shutdown();
        }
    }

    /// 发命令给引擎并同步一次快照中的轻量字段。
    pub fn send(&mut self, command: PlayerCommand) {
        let result = match self.active_engine() {
            Some(engine) => engine.command(command).map(|_| engine.snapshot()),
            None => return,
        };
        let snap = match result {
            Ok(snap) => snap,
            Err(err) => {
                self.error = Some(err.to_string());
                return;
            }
        };
        self.snapshot.state = snap.state;
        self.snapshot.subtitle_track = snap.subtitle_track;
        self.snapshot.audio_track = snap.audio_track;
        self.snapshot.secondary_subtitle_visible = snap.secondary_subtitle_visible;
        self.snapshot.volume = snap.volume;
        self.snapshot.speed = snap.speed;
        self.snapshot.muted = snap.muted;
        self.snapshot.tracks = snap.tracks;
        if let Some(d) = snap.duration_secs {
            self.duration = d;
            self.snapshot.duration_secs = Some(d);
        }
    }

    pub fn toggle_play(&mut self) {
        if matches!(self.snapshot.state, PlayerState::Ended | PlayerState::Idle)
            && self.duration > 0.0
        {
            self.seek_absolute(0.0);
            self.send(PlayerCommand::Play);
        } else {
            self.send(PlayerCommand::TogglePause);
        }
    }

    /// 相对 seek（秒），自动夹到 [0, duration]。
    pub fn seek_relative(&mut self, delta: f64) {
        self.seek_absolute(self.position + delta);
    }

    /// 绝对 seek（秒）。
    pub fn seek_absolute(&mut self, target: f64) {
        let max = if self.duration > 0.0 {
            self.duration
        } else {
            f64::MAX
        };
        let target = target.clamp(0.0, max);
        self.position = target;
        self.send(PlayerCommand::SeekAbsolute(target));
    }

    pub fn set_volume(&mut self, volume: f64) {
        self.volume = volume.clamp(0.0, 100.0);
        self.send(PlayerCommand::SetVolume(self.volume));
    }

    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        self.send(PlayerCommand::SetMuted(self.muted));
    }

    pub fn set_speed(&mut self, speed: f64) {
        if speed > 0.0 {
            self.speed = speed;
            self.send(PlayerCommand::SetSpeed(speed));
        }
    }

    /// 伪全屏 + 同步窗口全屏命令。
    pub fn set_fullscreen(&mut self, on: bool, ctx: Option<&egui::Context>) {
        self.fullscreen = on;
        if let Some(ctx) = ctx {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
        }
    }

    // ─────────────────────── 每帧 tick / 事件 ───────────────────────

    /// 每帧调用：缓存 ctx、拉快照、推进 Mock 时钟、按需请求重绘。
    pub fn tick(&mut self, ctx: &egui::Context) {
        self.ctx = Some(ctx.clone());
        let now = Instant::now();
        let dt = (now - self.last_tick).as_secs_f64().clamp(0.0, 0.25);
        self.last_tick = now;

        if !self.media_open {
            return;
        }
        let Some(mut snap) = self.active_engine_ref().map(|engine| engine.snapshot()) else {
            return;
        };
        let duration = snap.duration_secs.unwrap_or(self.duration);
        if self.backend == EngineBackend::Mock {
            // MockEngine 无时钟：由 UI 模拟播放进度（只影响演示显示）。
            if snap.state == PlayerState::Playing && duration > 0.0 {
                self.position = (self.position + dt * self.speed.max(0.05)).min(duration);
                if self.position >= duration {
                    snap.state = PlayerState::Ended;
                }
            }
        } else {
            self.position = snap.position_secs;
        }
        if duration > 0.0 {
            self.duration = duration;
        }
        snap.position_secs = self.position;
        snap.duration_secs = if self.duration > 0.0 {
            Some(self.duration)
        } else {
            None
        };
        snap.speed = self.speed;
        snap.volume = self.volume;
        snap.muted = self.muted;
        self.buffering_pct = buffered_percent(&snap);
        self.snapshot = snap;
        self.playing = matches!(
            self.snapshot.state,
            PlayerState::Playing | PlayerState::Buffering | PlayerState::Opening
        );
        self.refresh_danmaku_layout();
        if self.playing {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    /// 重建弹幕时间轴：wp-danmaku T23 布局引擎（车道碰撞/密度/4K 相对坐标）。
    /// 仅在分辨率或弹幕设置变化时重算（演示数据量小，但真实数据可含数万条）。
    pub fn refresh_danmaku_layout(&mut self) {
        let (vw, vh) = self.snapshot.video_size.unwrap_or((3840, 2160));
        let key = (
            vw,
            vh,
            self.danmaku.density,
            self.danmaku.area_top.to_bits(),
            self.danmaku.area_bottom.to_bits(),
            self.danmaku.font_scale.to_bits(),
            self.danmaku.speed.to_bits(),
        );
        if key == self.danmaku_layout_key {
            return;
        }
        self.danmaku_layout_key = key;
        let mut cfg = wp_danmaku::LayoutConfig::for_resolution(vw, vh);
        cfg.density_limit = self.danmaku.density.max(1);
        cfg.area_top = self.danmaku.area_top;
        cfg.area_bottom = self.danmaku.area_bottom;
        cfg.font_scale = self.danmaku.font_scale;
        cfg.speed = self.danmaku.speed;
        // 屏蔽词先在布局前过滤（与 Mode B ASS 生成共用同一过滤结果）
        let filter = wp_danmaku::DanmakuFilter {
            keywords: self.danmaku.blocked_words(),
            ..Default::default()
        };
        let comments = filter.apply(&self.demo_comments);
        let placed = wp_danmaku::layout::layout(&comments, &cfg);
        self.danmaku_timeline = wp_danmaku::PlacedTimeline::new(placed);
    }

    /// 当前时刻同屏弹幕数（状态行/弹幕面板诊断用；O(log n + k)）。
    pub fn active_danmaku_count(&self) -> usize {
        let at_ms = (overlay::demo_time(self.position, self.backend) * 1000.0) as i64;
        self.danmaku_timeline.active_count(at_ms)
    }

    /// 处理引擎事件（由 `App::on_player_event` 转发）。
    pub fn on_event(&mut self, ev: PlayerEvent) {
        match ev {
            PlayerEvent::Opened {
                duration_secs,
                tracks,
                video_size,
            } => {
                if let Some(d) = duration_secs {
                    self.duration = d;
                }
                self.snapshot.duration_secs = duration_secs;
                self.snapshot.video_size = video_size;
                self.snapshot.tracks = tracks.clone();
                self.subtitle.on_tracks(&tracks);
                self.first_frame = false;
            }
            PlayerEvent::StateChanged(state) => {
                self.snapshot.state = state;
                self.playing = matches!(
                    state,
                    PlayerState::Playing | PlayerState::Buffering | PlayerState::Opening
                );
            }
            PlayerEvent::PositionChanged(pos) => {
                if self.backend == EngineBackend::Mpv {
                    self.position = pos.max(0.0);
                }
            }
            PlayerEvent::DurationChanged(duration) => {
                if let Some(d) = duration {
                    self.duration = d;
                }
                self.snapshot.duration_secs = duration;
            }
            PlayerEvent::TracksChanged(tracks) => {
                self.snapshot.tracks = tracks.clone();
                self.subtitle.on_tracks(&tracks);
            }
            PlayerEvent::SubtitleCue {
                start_ms: _,
                text,
                secondary,
            } => {
                if secondary {
                    self.subtitle.secondary_cue = Some(text);
                } else {
                    self.subtitle.current_cue = Some(text);
                }
            }
            PlayerEvent::Buffering(pct) => self.buffering_pct = pct.min(100),
            PlayerEvent::FirstFrame => self.first_frame = true,
            PlayerEvent::EndOfFile => {
                self.playing = false;
                self.snapshot.state = PlayerState::Ended;
            }
            PlayerEvent::Error { message, .. } => {
                self.snapshot.state = PlayerState::Error;
                self.playing = false;
                self.error = Some(message);
            }
            PlayerEvent::Custom(_) => {}
        }
    }

    // ─────────────────────── 快捷键 ───────────────────────

    /// 播放器快捷键：空格 / ←→（±5s，Shift ±10s）/ F / S / D / Esc。
    /// 用 `consume_key` 消费按键，避免与聚焦控件重复触发。
    pub fn handle_keys(&self, ctx: &egui::Context) -> Vec<PlayerAction> {
        let mut actions = Vec::new();
        if !self.has_media() {
            return actions;
        }
        ctx.input_mut(|i| {
            let mut hit = |mods: egui::Modifiers, key: egui::Key| i.consume_key(mods, key);
            if hit(egui::Modifiers::NONE, egui::Key::Space) {
                actions.push(PlayerAction::TogglePlay);
            }
            if hit(egui::Modifiers::SHIFT, egui::Key::ArrowLeft) {
                actions.push(PlayerAction::SeekRelative(-10.0));
            } else if hit(egui::Modifiers::NONE, egui::Key::ArrowLeft) {
                actions.push(PlayerAction::SeekRelative(-5.0));
            }
            if hit(egui::Modifiers::SHIFT, egui::Key::ArrowRight) {
                actions.push(PlayerAction::SeekRelative(10.0));
            } else if hit(egui::Modifiers::NONE, egui::Key::ArrowRight) {
                actions.push(PlayerAction::SeekRelative(5.0));
            }
            if hit(egui::Modifiers::NONE, egui::Key::F) {
                actions.push(PlayerAction::ToggleFullscreen);
            }
            if hit(egui::Modifiers::NONE, egui::Key::S) {
                actions.push(PlayerAction::ToggleSubtitle);
            }
            if hit(egui::Modifiers::NONE, egui::Key::D) {
                actions.push(PlayerAction::ToggleDanmaku);
            }
            if self.fullscreen && hit(egui::Modifiers::NONE, egui::Key::Escape) {
                actions.push(PlayerAction::ExitFullscreen);
            }
        });
        actions
    }
}

/// 引擎事件 → App 事件通道的处理器（可在引擎线程回调）。
fn make_event_handler(tx: Arc<Mutex<Sender<Event>>>, ctx: Option<egui::Context>) -> EventHandler {
    Box::new(move |ev| {
        if let Ok(tx) = tx.lock() {
            let _ = tx.send(Event::Player(ev));
        }
        if let Some(ctx) = &ctx {
            ctx.request_repaint();
        }
    })
}

/// 计算已缓冲百分比（Mock 用「当前位置 +120s」模拟，真实引擎用 BufferedRange）。
fn buffered_percent(snap: &PlaybackSnapshot) -> u8 {
    let Some(duration) = snap.duration_secs.filter(|d| *d > 0.0) else {
        return 0;
    };
    let pos = snap.position_secs;
    let mut covered = 0.0;
    for range in &snap.buffered {
        if range.end_secs > pos {
            covered += (range.end_secs - range.start_secs.max(pos)).max(0.0);
        }
    }
    if covered <= 0.0 {
        // 演示引擎：假装已缓冲未来 120 秒
        covered = (pos + 120.0).min(duration) - pos;
    }
    ((covered / duration) * 100.0).clamp(0.0, 100.0) as u8
}

// ═══════════════════════════ App 接线（播放器相关） ═══════════════════════════

impl App {
    /// 打开媒体源并切到播放器页（浏览器入口/外部调用方统一走这里）。
    pub fn open_player(&mut self, source: MediaSource, label: impl Into<String>) {
        let label = label.into();
        self.player.open_source(source, label);
        self.page = Page::Player;
        self.player.set_fullscreen(false, self.ctx.as_ref());
        if let Some(note) = self.player.engine_note.clone() {
            self.player.engine_note = None;
            self.toast(ToastKind::Warn, note);
        }
        match self.player.error.clone() {
            Some(err) => self.toast(ToastKind::Error, format!("打开媒体失败：{err}")),
            None => self.toast(
                ToastKind::Success,
                format!("已打开：{}", self.player.source_label),
            ),
        }
    }

    /// 浏览器文件条目 → 播放器。
    ///
    /// 网盘文件当前先构造占位 `NetworkSource`（MockEngine 只展示名称）；
    /// T15 完成后由 `wp-stream` 生成真实代理 URL 填入 `NetworkSource.url`。
    pub fn open_player_remote_entry(&mut self, entry: &RemoteEntry) {
        let kind = self.active_slot().map(|slot| slot.kind());
        let source = if kind == Some(DriveKind::Local) {
            MediaSource::Local(PathBuf::from(&entry.path))
        } else {
            MediaSource::Network(NetworkSource {
                display: entry.path.clone(),
                // TODO(T15/wp-stream)：替换为 StreamProxy::start().url_for(...)
                url: String::new(),
                headers: Vec::new(),
                cache_key: format!("{}:{}:{}", self.active_drive_id(), entry.path, entry.size),
                size: entry.size,
            })
        };
        self.open_player(source, display_name_for_entry(entry));
    }

    /// 本地文件 → 播放器。
    pub fn open_player_local_file(&mut self, path: PathBuf) {
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        self.open_player(MediaSource::Local(path), label);
    }

    /// 演示媒体（`--page player` 截图回归 / 空态「播放演示视频」）。
    ///
    /// 强制走 MockEngine：演示路径是假的，真实引擎会 open 失败；也保证 smoke 截图稳定。
    pub fn open_player_demo(&mut self) {
        self.player.open_demo_source(
            MediaSource::Local(PathBuf::from("/演示/影片/示例影片-4K HDR.mp4")),
            "示例影片-4K HDR.mp4 · 演示媒体".into(),
        );
        self.page = Page::Player;
        self.player.set_fullscreen(false, self.ctx.as_ref());
    }

    /// 播放网盘页当前选中的媒体文件。
    pub fn open_selected_media(&mut self) {
        let entries = self.selected_entries();
        if let Some(entry) = entries.iter().find(|e| !e.is_dir && is_media_file(&e.name)) {
            self.open_player_remote_entry(entry);
            return;
        }
        if entries.is_empty() {
            self.toast(
                ToastKind::Warn,
                "请先在「网盘」页选中一个视频/音频文件（或直接双击文件播放）",
            );
        } else {
            self.toast(ToastKind::Warn, "选中的条目里没有可播放的视频/音频文件");
        }
    }

    /// 打开内置本地文件选择器（播放媒体）。
    pub fn open_local_media_picker(&mut self) {
        self.open_file_picker(false);
        if let Some(picker) = &mut self.file_picker {
            picker.purpose = FilePickerPurpose::PlayMedia;
        }
    }

    /// 打开内置本地文件选择器（选择外挂字幕）。
    pub fn open_subtitle_picker(&mut self) {
        self.open_file_picker(false);
        if let Some(picker) = &mut self.file_picker {
            picker.purpose = FilePickerPurpose::SubtitleFile;
        }
    }

    /// 引擎事件入口（`Event::Player` 转发到这里；错误类事件额外 toast）。
    pub fn on_player_event(&mut self, ev: PlayerEvent) {
        match &ev {
            PlayerEvent::Error {
                message,
                recoverable,
            } => {
                let tail = if *recoverable {
                    "（可恢复：将尝试重试/换链）"
                } else {
                    "（不可恢复）"
                };
                self.toast(ToastKind::Error, format!("播放器错误：{message}{tail}"));
            }
            PlayerEvent::EndOfFile => {
                self.toast(ToastKind::Info, "播放结束".to_string());
            }
            _ => {}
        }
        self.player.on_event(ev);
    }

    /// 应用一批播放器 UI 动作（页面/快捷键统一出口）。
    pub fn apply_player_actions(&mut self, actions: Vec<PlayerAction>) {
        for action in actions {
            self.apply_player_action(action);
        }
    }

    fn apply_player_action(&mut self, action: PlayerAction) {
        match action {
            PlayerAction::TogglePlay => self.player.toggle_play(),
            PlayerAction::Stop => self.player.send(PlayerCommand::Stop),
            PlayerAction::SeekRelative(delta) => self.player.seek_relative(delta),
            PlayerAction::SeekAbsolute(target) => self.player.seek_absolute(target),
            PlayerAction::Command(cmd) => self.player.send(cmd),
            PlayerAction::ToggleFullscreen => {
                let on = !self.player.fullscreen;
                self.player.set_fullscreen(on, self.ctx.as_ref());
            }
            PlayerAction::ExitFullscreen => self.player.set_fullscreen(false, self.ctx.as_ref()),
            PlayerAction::ToggleSubtitle => {
                let cmd = self.player.subtitle.toggle_command();
                self.player.send(cmd);
            }
            PlayerAction::ToggleDanmaku => {
                self.player.danmaku.enabled = !self.player.danmaku.enabled;
            }
            PlayerAction::Screenshot => self.player_screenshot(),
            PlayerAction::OpenLocalFile => self.open_local_media_picker(),
            PlayerAction::OpenSelectedRemote => self.open_selected_media(),
            PlayerAction::OpenDemo => self.open_player_demo(),
            PlayerAction::CloseMedia => {
                self.player.set_fullscreen(false, self.ctx.as_ref());
                self.player.close();
                self.toast(ToastKind::Info, "已关闭当前媒体");
            }
            PlayerAction::SetVolume(volume) => self.player.set_volume(volume),
            PlayerAction::ToggleMute => self.player.toggle_mute(),
            PlayerAction::SetSpeed(speed) => self.player.set_speed(speed),
            PlayerAction::SwitchPanel(panel) => self.player.panel = panel,
            PlayerAction::PickSubtitleFile => self.open_subtitle_picker(),
            PlayerAction::Toast(kind, text) => self.toast(kind, text),
        }
    }

    fn player_screenshot(&mut self) {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let dir = self.paths.root.join("播放器").join("截图");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("截图-{stamp}.png"));
        self.player
            .send(PlayerCommand::Screenshot { path: path.clone() });
        if self.player.backend == EngineBackend::Mpv {
            self.toast(
                ToastKind::Success,
                format!("已请求截图：{}", path.display()),
            );
        } else {
            self.toast(
                ToastKind::Info,
                format!(
                    "截图接口已预留：{}（MockEngine 不写盘，MpvEngine 接入后生效）",
                    path.display()
                ),
            );
        }
    }

    /// 播放器页快捷键（由 `App::handle_shortcuts` 调用）。
    pub fn player_shortcuts(&mut self, ctx: &egui::Context) {
        let actions = self.player.handle_keys(ctx);
        if !actions.is_empty() {
            self.apply_player_actions(actions);
        }
    }
}

// ═══════════════════════════ 页面布局 ═══════════════════════════

/// 页面入口（app.rs 中央面板调用）。
pub fn show(app: &mut App, ui: &mut Ui) {
    let t = app.theme;
    let mut actions: Vec<PlayerAction> = Vec::new();
    let fullscreen = app.player.fullscreen;

    if !fullscreen {
        show_header(&app.player, ui, &t, &mut actions);
        ui.add_space(6.0);
    }

    if !app.player.has_media() {
        show_empty_state(ui, &t, &mut actions);
    } else {
        let available = ui.available_size();
        if fullscreen {
            ui.allocate_ui_with_layout(available, Layout::top_down(Align::Min), |ui| {
                show_stage(&app.player, ui, &t, &mut actions, true);
            });
        } else {
            let gap = 8.0;
            let panel_w = (available.x * 0.30).clamp(280.0, 390.0);
            let stage_w = (available.x - panel_w - gap).max(220.0);
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(stage_w, available.y),
                    Layout::top_down(Align::Min),
                    |ui| show_stage(&app.player, ui, &t, &mut actions, false),
                );
                ui.add_space(gap);
                ui.allocate_ui_with_layout(
                    Vec2::new(panel_w, available.y),
                    Layout::top_down(Align::Min),
                    |ui| show_side_panel(&mut app.player, ui, &t, &mut actions),
                );
            });
        }
    }

    app.apply_player_actions(actions);
}

fn show_header(player: &PlayerUi, ui: &mut Ui, t: &Theme, actions: &mut Vec<PlayerAction>) {
    ui.horizontal(|ui| {
        widgets::section_title(ui, t, "播放器");
        ui.add_space(6.0);
        // 引擎徽标：真实渲染中 / libmpv 已就绪（当前为演示媒体）/ 无 libmpv 回退
        let (badge, badge_color) = if player.backend == EngineBackend::Mpv {
            ("libmpv 真实渲染", t.success)
        } else if player.real_engine_available() {
            ("libmpv 就绪 · 演示中", t.accent)
        } else {
            ("演示引擎 Mock", t.warning)
        };
        pill(ui, t, badge, badge_color);
        if player.has_media() {
            if player.fullscreen {
                pill(ui, t, "全屏", t.accent);
            }
            let name = widgets::ellipsis(player.media_name(), 46);
            ui.label(RichText::new(name).size(12.5).color(t.text_weak))
                .on_hover_text(player.media_name());
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if player.has_media() {
                if widgets::secondary_button(ui, t, "✕ 关闭媒体").clicked() {
                    actions.push(PlayerAction::CloseMedia);
                }
                if widgets::secondary_button(ui, t, "📷 截图").clicked() {
                    actions.push(PlayerAction::Screenshot);
                }
                if widgets::secondary_button(
                    ui,
                    t,
                    if player.fullscreen {
                        "🗗 退出全屏"
                    } else {
                        "⛶ 全屏"
                    },
                )
                .clicked()
                {
                    actions.push(PlayerAction::ToggleFullscreen);
                }
            } else if widgets::secondary_button(ui, t, "🎬 演示媒体").clicked() {
                actions.push(PlayerAction::OpenDemo);
            }
            if widgets::secondary_button(ui, t, "📂 打开本地文件").clicked() {
                actions.push(PlayerAction::OpenLocalFile);
            }
            if widgets::secondary_button(ui, t, "▶ 播放网盘选中文件").clicked() {
                actions.push(PlayerAction::OpenSelectedRemote);
            }
        });
    });
}

fn show_empty_state(ui: &mut Ui, t: &Theme, actions: &mut Vec<PlayerAction>) {
    let height = ui.available_height().max(320.0);
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), height),
        Layout::top_down(Align::Center),
        |ui| {
            ui.add_space((height * 0.08).min(60.0));
            ui.label(RichText::new("▶").size(64.0).color(t.accent));
            ui.add_space(4.0);
            ui.label(
                RichText::new("播放器就绪")
                    .size(20.0)
                    .strong()
                    .color(t.text),
            );
            ui.add_space(6.0);
            widgets::hint(ui, t, "双击网盘里的视频/音频文件，或在文件上右键「▶ 播放」");
            widgets::hint(
                ui,
                t,
                "支持格式：MP4 / MKV / MOV / TS / WebM / FLV / MP3 / FLAC / AAC / M4A …",
            );
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                if widgets::primary_button(ui, t, "📂 打开本地文件").clicked() {
                    actions.push(PlayerAction::OpenLocalFile);
                }
                if widgets::secondary_button(ui, t, "▶ 播放网盘选中文件").clicked() {
                    actions.push(PlayerAction::OpenSelectedRemote);
                }
                if widgets::secondary_button(ui, t, "🎬 播放演示视频").clicked() {
                    actions.push(PlayerAction::OpenDemo);
                }
            });
            ui.add_space(14.0);
            widgets::hint(
                ui,
                t,
                "当前引擎：演示引擎（MockEngine）· 媒体内核 libmpv 接入后自动切换真实解码/渲染",
            );
            widgets::hint(
                ui,
                t,
                "快捷键：空格 暂停/播放 · ←/→ ±5s（Shift ±10s）· F 全屏 · S 字幕 · D 弹幕",
            );
        },
    );
}

fn show_stage(
    player: &PlayerUi,
    ui: &mut Ui,
    t: &Theme,
    actions: &mut Vec<PlayerAction>,
    fullscreen: bool,
) {
    let total = ui.available_size();
    let controls_h = 78.0;
    let status_h = if fullscreen { 0.0 } else { 24.0 };
    let video_h = (total.y - controls_h - status_h - 6.0).max(160.0);
    let size = Vec2::new(total.x.max(160.0), video_h);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
    if std::env::var("WP_PLAYER_DIAG").is_ok() && player.is_real_playback() {
        let inner = overlay::fit_16_9(rect);
        println!(
            "PLAYER_GEOM frame rect=({:.1},{:.1},{:.1},{:.1}) inner=({:.1},{:.1},{:.1},{:.1}) clip=({:.1},{:.1},{:.1},{:.1}) ppp={:.2} total=({:.1},{:.1}) avail={:?}",
            rect.min.x, rect.min.y, rect.width(), rect.height(),
            inner.min.x, inner.min.y, inner.width(), inner.height(),
            ui.clip_rect().min.x, ui.clip_rect().min.y, ui.clip_rect().max.x, ui.clip_rect().max.y,
            ui.ctx().pixels_per_point(),
            total.x, total.y,
            ui.available_size(),
        );
    }
    if player.is_real_playback() {
        // 真实帧：mpv GL render API 画进 16:9 内容区（Mode A），再叠加 egui 覆盖层（OSD/字幕/弹幕）
        ui.painter()
            .rect_filled(rect, 0, Color32::from_rgb(6, 7, 10));
        let inner = overlay::fit_16_9(rect);
        if let Some(render) = &player.render {
            render.paint_rect(ui, inner);
        }
        overlay::paint_overlay(ui, inner, player, t);
    } else {
        overlay::paint_video(ui, rect, player, t);
    }
    if response.clicked() {
        actions.push(PlayerAction::TogglePlay);
    }
    if response.double_clicked() {
        if fullscreen {
            actions.push(PlayerAction::ExitFullscreen);
        } else {
            actions.push(PlayerAction::ToggleFullscreen);
        }
    }
    if response.hovered() {
        response.clone().on_hover_text("单击播放/暂停 · 双击全屏");
    }
    ui.add_space(4.0);
    show_controls(player, ui, t, actions);
    if !fullscreen {
        show_status_line(player, ui, t);
    }
}

fn show_controls(player: &PlayerUi, ui: &mut Ui, t: &Theme, actions: &mut Vec<PlayerAction>) {
    egui::Frame::NONE
        .fill(t.card)
        .stroke(egui::Stroke::new(1.0, t.border))
        .corner_radius(theme::radius(8))
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            // 进度条 + 时间
            ui.horizontal(|ui| {
                let duration = player.duration.max(0.0);
                let mut scrub = player.position.clamp(0.0, duration.max(0.0));
                if duration > 0.0 {
                    let resp = ui.add(
                        egui::Slider::new(&mut scrub, 0.0..=duration)
                            .show_value(false)
                            .trailing_fill(true),
                    );
                    if resp.changed() {
                        actions.push(PlayerAction::SeekAbsolute(scrub));
                    }
                } else {
                    ui.add(egui::ProgressBar::new(0.0));
                }
                ui.label(
                    RichText::new(format!(
                        "{} / {}",
                        overlay::format_time(scrub),
                        overlay::format_time(duration)
                    ))
                    .size(12.0)
                    .monospace()
                    .color(t.text_weak),
                );
            });
            ui.add_space(2.0);
            // 按钮行
            ui.horizontal(|ui| {
                let play_icon = if player.is_playing() { "⏸" } else { "▶" };
                if ui
                    .add(
                        egui::Button::new(RichText::new(play_icon).size(17.0))
                            .min_size(Vec2::new(42.0, 28.0)),
                    )
                    .on_hover_text("播放/暂停（空格）")
                    .clicked()
                {
                    actions.push(PlayerAction::TogglePlay);
                }
                if widgets::tool_button(ui, t, "⏹", player.has_media())
                    .on_hover_text("停止")
                    .clicked()
                {
                    actions.push(PlayerAction::Stop);
                }
                if widgets::tool_button(ui, t, "⏪ 5s", true)
                    .on_hover_text("后退 5 秒（←；Shift+← 为 10 秒）")
                    .clicked()
                {
                    actions.push(PlayerAction::SeekRelative(-5.0));
                }
                if widgets::tool_button(ui, t, "⏩ 5s", true)
                    .on_hover_text("前进 5 秒（→；Shift+→ 为 10 秒）")
                    .clicked()
                {
                    actions.push(PlayerAction::SeekRelative(5.0));
                }
                ui.separator();
                // 音量
                if widgets::tool_button(ui, t, if player.muted { "🔇" } else { "🔊" }, true)
                    .on_hover_text("静音")
                    .clicked()
                {
                    actions.push(PlayerAction::ToggleMute);
                }
                let mut volume = if player.muted { 0.0 } else { player.volume };
                if ui
                    .add_sized(
                        Vec2::new(96.0, 18.0),
                        egui::Slider::new(&mut volume, 0.0..=100.0).show_value(false),
                    )
                    .changed()
                {
                    actions.push(PlayerAction::SetVolume(volume));
                }
                ui.separator();
                // 倍速
                egui::ComboBox::from_id_salt("player_speed")
                    .width(76.0)
                    .selected_text(format!("{}x", trim_speed(player.speed)))
                    .show_ui(ui, |ui| {
                        for speed in SPEED_PRESETS {
                            if ui
                                .selectable_label(
                                    (player.speed - speed).abs() < 0.001,
                                    format!("{}x", trim_speed(speed)),
                                )
                                .clicked()
                            {
                                actions.push(PlayerAction::SetSpeed(speed));
                            }
                        }
                    });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::tool_button(ui, t, "⛶", true)
                        .on_hover_text("全屏（F）")
                        .clicked()
                    {
                        actions.push(PlayerAction::ToggleFullscreen);
                    }
                    if widgets::tool_button(ui, t, "📷", true)
                        .on_hover_text("截图")
                        .clicked()
                    {
                        actions.push(PlayerAction::Screenshot);
                    }
                    let panel_label = format!("{} {}", player.panel.icon(), player.panel.label());
                    if widgets::tool_button(ui, t, &panel_label, true)
                        .on_hover_text("切换右侧面板")
                        .clicked()
                    {
                        let next = match player.panel {
                            PlayerPanel::Subtitles => PlayerPanel::Danmaku,
                            PlayerPanel::Danmaku => PlayerPanel::Ai,
                            PlayerPanel::Ai => PlayerPanel::Subtitles,
                        };
                        actions.push(PlayerAction::SwitchPanel(next));
                    }
                    if player.backend == EngineBackend::Mock {
                        widgets::hint(ui, t, "演示模式");
                    }
                });
            });
        });
}

fn show_status_line(player: &PlayerUi, ui: &mut Ui, t: &Theme) {
    ui.horizontal(|ui| {
        let (label, color) = overlay::state_style(player.snapshot.state, t);
        ui.label(RichText::new(format!("● {label}")).size(11.0).color(color));
        ui.separator();
        let hwdec = player.snapshot.hwdec.clone().unwrap_or_else(|| {
            if player.backend == EngineBackend::Mock {
                "演示（未接解码器）".into()
            } else {
                "软解/未知".into()
            }
        });
        status_kv(ui, t, "硬解", &hwdec);
        status_kv(ui, t, "丢帧", &player.snapshot.dropped_frames.to_string());
        status_kv(ui, t, "缓冲", &format!("{}%", player.buffering_pct));
        let res = player
            .snapshot
            .video_size
            .map(|(w, h)| format!("{w}×{h}"))
            .unwrap_or_else(|| "—".into());
        status_kv(ui, t, "分辨率", &res);
        if !player.snapshot.tracks.is_empty() {
            status_kv(ui, t, "轨道", &tracks_summary(&player.snapshot.tracks));
        }
        status_kv(ui, t, "倍速", &format!("{}x", trim_speed(player.speed)));
        status_kv(
            ui,
            t,
            "音量",
            &format!("{:.0}", if player.muted { 0.0 } else { player.volume }),
        );
        if player.backend == EngineBackend::Mpv {
            if let Some(render) = &player.render {
                let stats = render.stats();
                status_kv(ui, t, "渲染", &format!("{} 帧", stats.frames_rendered));
                if stats.paint_calls > 0 && stats.frames_rendered == 0 {
                    ui.label(
                        RichText::new("（渲染回调已触发，等待首帧…）")
                            .size(10.0)
                            .color(t.warning),
                    );
                }
                if let Some(err) = render.last_error() {
                    ui.label(
                        RichText::new(format!("⚠ {}", widgets::ellipsis(&err, 40)))
                            .size(10.5)
                            .color(t.error),
                    )
                    .on_hover_text(err);
                }
            }
        } else if let Some(err) = &player.real_engine_error {
            ui.label(
                RichText::new(format!("真实引擎：{}", widgets::ellipsis(err, 34)))
                    .size(10.0)
                    .color(t.text_weak),
            )
            .on_hover_text(err);
        }
        if let Some(note) = &player.engine_note {
            ui.label(RichText::new(note).size(10.5).color(t.warning));
        }
        if let Some(err) = &player.error {
            ui.label(
                RichText::new(format!("⚠ {}", widgets::ellipsis(err, 60)))
                    .size(10.5)
                    .color(t.error),
            )
            .on_hover_text(err);
        }
    });
}

fn show_side_panel(player: &mut PlayerUi, ui: &mut Ui, t: &Theme, actions: &mut Vec<PlayerAction>) {
    ui.horizontal(|ui| {
        for tab in [
            PlayerPanel::Subtitles,
            PlayerPanel::Danmaku,
            PlayerPanel::Ai,
        ] {
            let selected = player.panel == tab;
            if ui
                .selectable_label(selected, format!("{} {}", tab.icon(), tab.label()))
                .clicked()
            {
                player.panel = tab;
            }
        }
    });
    ui.add_space(4.0);
    egui::ScrollArea::vertical()
        .id_salt("player_side_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| match player.panel {
            PlayerPanel::Subtitles => subtitle_panel::show(player, t, ui, actions),
            PlayerPanel::Danmaku => danmaku_panel::show(player, t, ui, actions),
            PlayerPanel::Ai => ai_panel::show(player, t, ui, actions),
        });
}

fn status_kv(ui: &mut Ui, t: &Theme, key: &str, value: &str) {
    ui.label(RichText::new(key).size(10.5).color(t.text_weak));
    ui.label(RichText::new(value).size(10.5).color(t.text));
}

fn pill(ui: &mut Ui, _t: &Theme, text: &str, color: Color32) {
    let fill = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 26);
    egui::Frame::NONE
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(theme::radius(10))
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.0).color(color));
        });
}

/// 倍速显示：去掉多余的 `.0`。
pub fn trim_speed(speed: f64) -> String {
    if (speed - speed.round()).abs() < 0.001 {
        format!("{}", speed.round() as i64)
    } else {
        format!("{speed}")
    }
}

/// 媒体条目在播放器里的展示名（远程路径取文件名）。
pub fn display_name_for_entry(entry: &RemoteEntry) -> String {
    if entry.name.is_empty() {
        entry.path.clone()
    } else {
        entry.name.clone()
    }
}

/// 轨道类型标签（状态/面板调试展示用）。
pub fn track_kind_label(kind: Option<TrackKind>) -> &'static str {
    match kind {
        Some(TrackKind::Video) => "视频",
        Some(TrackKind::Audio) => "音频",
        Some(TrackKind::Subtitle) => "字幕",
        None => "未知",
    }
}

/// 轨道概要（`T5` 状态行/`T8` 面板调试用）。
pub fn tracks_summary(tracks: &[TrackInfo]) -> String {
    let mut v = 0;
    let mut a = 0;
    let mut s = 0;
    for track in tracks {
        match track.kind {
            Some(TrackKind::Video) => v += 1,
            Some(TrackKind::Audio) => a += 1,
            Some(TrackKind::Subtitle) => s += 1,
            None => {}
        }
    }
    format!("视频 {v} · 音频 {a} · 字幕 {s}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_ui() -> PlayerUi {
        let (tx, _rx) = std::sync::mpsc::channel();
        PlayerUi::new(tx)
    }

    #[test]
    fn media_file_detection() {
        assert!(is_media_file("电影.mp4"));
        assert!(is_media_file("影片.MKV"));
        assert!(is_media_file("music.flac"));
        assert!(is_media_file("a.b.c.m4a"));
        assert!(is_media_file("bluray.iso"));
        assert!(!is_media_file("说明.txt"));
        assert!(!is_media_file("字幕.srt"));
        assert!(!is_media_file("noext"));
        assert!(!is_media_file(".mp4"));
        assert!(!is_media_file("ends."));
    }

    #[test]
    fn subtitle_file_detection() {
        assert!(is_subtitle_file("a.srt"));
        assert!(is_subtitle_file("A.ASS"));
        assert!(is_subtitle_file("x.vtt"));
        assert!(!is_subtitle_file("a.mp4"));
        assert!(!is_subtitle_file("srt"));
    }

    #[test]
    fn trim_speed_is_compact() {
        assert_eq!(trim_speed(1.0), "1");
        assert_eq!(trim_speed(1.5), "1.5");
        assert_eq!(trim_speed(0.75), "0.75");
        assert_eq!(trim_speed(2.0), "2");
    }

    #[test]
    fn mock_engine_open_sets_media_state() {
        let mut ui = fresh_ui();
        assert!(!ui.has_media());
        ui.open_source(
            MediaSource::Local(PathBuf::from("/tmp/demo.mp4")),
            "demo.mp4".into(),
        );
        assert!(ui.has_media());
        assert_eq!(ui.backend, EngineBackend::Mock);
        assert_eq!(ui.source_label, "demo.mp4");
        // Mock 起播于 2s，时长 1h，初始可见演示字幕
        assert!((ui.position - 2.0).abs() < 1e-6);
        // 打开时 MockEngine 同步发出 Opened 事件 → 进 App 事件通道，由 tick/on_event 消费；
        // 这里直接验证引擎已可用。
        assert!(ui.engine.is_some());
        ui.close();
        assert!(!ui.has_media());
        assert_eq!(ui.position, 0.0);
    }

    #[test]
    fn seek_clamps_to_duration() {
        let mut ui = fresh_ui();
        ui.duration = 100.0;
        ui.seek_absolute(120.0);
        assert!((ui.position - 100.0).abs() < 1e-6);
        ui.seek_absolute(-5.0);
        assert_eq!(ui.position, 0.0);
        ui.seek_relative(30.0);
        assert!((ui.position - 30.0).abs() < 1e-6);
        ui.seek_relative(-100.0);
        assert_eq!(ui.position, 0.0);
    }

    #[test]
    fn commands_without_engine_are_noop() {
        let mut ui = fresh_ui();
        ui.send(PlayerCommand::TogglePause); // 不应 panic
        ui.toggle_play();
        ui.set_volume(50.0);
        assert_eq!(ui.volume, 50.0);
        ui.toggle_mute();
        assert!(ui.muted);
    }

    #[test]
    fn tracks_summary_counts() {
        let tracks = vec![
            TrackInfo {
                kind: Some(TrackKind::Video),
                ..Default::default()
            },
            TrackInfo {
                kind: Some(TrackKind::Audio),
                ..Default::default()
            },
            TrackInfo {
                kind: Some(TrackKind::Subtitle),
                ..Default::default()
            },
            TrackInfo {
                kind: None,
                ..Default::default()
            },
        ];
        assert_eq!(tracks_summary(&tracks), "视频 1 · 音频 1 · 字幕 1");
        assert_eq!(track_kind_label(Some(TrackKind::Subtitle)), "字幕");
        assert_eq!(track_kind_label(None), "未知");
    }

    #[test]
    fn panel_labels_and_cyclic_switch() {
        assert_eq!(PlayerPanel::Subtitles.label(), "字幕");
        assert_eq!(PlayerPanel::Ai.icon(), "🤖");
    }

    #[test]
    fn buffered_percent_mock_and_ranges() {
        let mut snap = PlaybackSnapshot {
            duration_secs: Some(100.0),
            position_secs: 0.0,
            ..Default::default()
        };
        // Mock 补足：120s 但时长 100s → 全部缓冲
        assert_eq!(buffered_percent(&snap), 100);
        snap.buffered = vec![wp_media::BufferedRange {
            start_secs: 0.0,
            end_secs: 50.0,
        }];
        snap.position_secs = 25.0;
        // 25~50 已缓冲 = 25/100
        assert_eq!(buffered_percent(&snap), 25);
        snap.duration_secs = None;
        assert_eq!(buffered_percent(&snap), 0);
    }
}
