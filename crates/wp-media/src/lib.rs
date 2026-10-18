//! 媒体内核抽象（播放器）：与具体引擎（libmpv）解耦的类型、命令与事件。
//!
//! 设计目标：GUI / 字幕 / 弹幕 / AI 只依赖本 crate 的类型，不直接碰 FFI。
//! - [`MediaSource`]：本地文件 / 网盘流（经 wp-stream 代理）/ 蓝光 ISO·BDMV
//! - [`MediaEngine`]：播放引擎 trait（MpvEngine、MockEngine 实现）
//! - [`PlaybackSnapshot`]：UI 每帧读取的只读快照
//! - [`PlayerEvent`]：引擎 → UI/字幕/AI 的异步事件

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use wp_core::Result;

/// 播放源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaSource {
    /// 本地文件路径。
    Local(PathBuf),
    /// 网盘流：已通过 wp-stream 包装成本地 HTTP 地址。
    Network(NetworkSource),
    /// 蓝光 ISO / BDMV 目录（v1 只承诺主标题播放）。
    BluRay(BluRaySource),
}

/// 网盘流参数（由 wp-stream 生成，播放器不关心网盘鉴权细节）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkSource {
    /// 展示名（文件路径）。
    pub display: String,
    /// 本地代理 URL（http://127.0.0.1:port/s/token）。
    pub url: String,
    /// 透传请求头（代理模式下可为空）。
    pub headers: Vec<(String, String)>,
    /// 缓存键（drive:path:size）。
    pub cache_key: String,
    /// 文件大小（0=未知）。
    pub size: u64,
}

/// 蓝光来源。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BluRaySource {
    /// ISO 文件或 BDMV 所在目录。
    pub iso_or_dir: PathBuf,
    /// 指定 title/playlist（None = 主标题）。
    pub title_id: Option<u32>,
}

/// 播放状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PlayerState {
    #[default]
    Idle,
    Opening,
    Buffering,
    Playing,
    Paused,
    Seeking,
    Ended,
    Error,
}

/// 轨道类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

/// 轨道信息。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackInfo {
    /// 引擎侧轨道 id（mpv: `--sid/--aid/--vid` 的值）。
    pub id: i64,
    pub kind: Option<TrackKind>,
    /// 标题（可能带语言/编码信息）。
    pub title: String,
    /// ISO 语言码（如 zh/en）。
    pub lang: String,
    /// 编码（如 hevc/aac/ass/pgs）。
    pub codec: String,
    /// 是否默认轨道。
    pub default: bool,
    /// 是否当前选中。
    pub selected: bool,
    /// 是否外部字幕（true=外挂）。
    pub external: bool,
}

/// 蓝光标题（通过 libbluray 枚举，主标题播放用）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DiscTitle {
    /// title 序号（1-based）。
    pub id: u32,
    /// playlist（如 00001.mpls）。
    pub playlist: String,
    pub duration_secs: f64,
    pub chapters: u32,
    pub name: String,
}

/// 蓝光碟片信息。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DiscInfo {
    pub titles: Vec<DiscTitle>,
    pub current: Option<u32>,
    pub total_duration_secs: f64,
}

/// 已缓冲区间（秒）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BufferedRange {
    pub start_secs: f64,
    pub end_secs: f64,
}

/// 兼容旧代码的默认值。
impl Default for BufferedRange {
    fn default() -> Self {
        Self {
            start_secs: 0.0,
            end_secs: 0.0,
        }
    }
}

/// 播放器只读快照（UI 每帧读取；字段都是廉价数据）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlaybackSnapshot {
    pub state: PlayerState,
    pub position_secs: f64,
    pub duration_secs: Option<f64>,
    /// 已缓冲区间（由 wp-stream 上报）。
    pub buffered: Vec<BufferedRange>,
    pub speed: f64,
    pub volume: f64,
    pub muted: bool,
    pub tracks: Vec<TrackInfo>,
    pub audio_track: Option<i64>,
    pub subtitle_track: Option<i64>,
    pub secondary_subtitle_visible: bool,
    pub disc: Option<DiscInfo>,
    /// 实际使用的硬件解码器（如 vaapi/d3d11va/vulkan），用于诊断面板。
    pub hwdec: Option<String>,
    /// 视频尺寸（解码后）。
    pub video_size: Option<(u32, u32)>,
    /// 累计丢帧数（性能面板）。
    pub dropped_frames: u64,
    /// 最近一次错误信息（state=Error 时）。
    pub error: Option<String>,
}

/// 播放命令（UI → 引擎）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PlayerCommand {
    Play,
    Pause,
    TogglePause,
    Stop,
    SeekAbsolute(f64),
    SeekRelative(f64),
    SetSpeed(f64),
    SetVolume(f64),
    SetMuted(bool),
    /// 选择轨道（`id=-1` 关闭该类型轨道）。
    SelectTrack {
        kind: TrackKind,
        id: i64,
    },
    /// 主/副字幕可见性。
    SetSecondarySubtitleVisible(bool),
    /// 字幕延迟（秒，正=延后）。
    SetSubtitleDelay(f64),
    /// 外挂字幕文件（AI 翻译/下载的字幕也走这里注入）。
    AddSubtitleFile {
        path: PathBuf,
        select: bool,
    },
    /// 重新加载外部字幕（AI 更新内容后）。
    ReloadSubtitles,
    /// 截图保存到指定路径。
    Screenshot {
        path: PathBuf,
    },
    /// 蓝光：切换到指定 title。
    SelectDiscTitle {
        id: u32,
    },
    /// 请求重绘（引擎有新帧时也会经事件通知）。
    RequestRepaint,
}

/// 引擎 → 上层事件。
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    /// 媒体已打开（解复用成功，拿到时长/轨道）。
    Opened {
        duration_secs: Option<f64>,
        tracks: Vec<TrackInfo>,
        video_size: Option<(u32, u32)>,
    },
    StateChanged(PlayerState),
    PositionChanged(f64),
    DurationChanged(Option<f64>),
    TracksChanged(Vec<TrackInfo>),
    /// 当前字幕 cue（AI 翻译/弹幕对齐/字幕面板用）。
    SubtitleCue {
        start_ms: i64,
        text: String,
        secondary: bool,
    },
    /// 缓冲进度（0~100，来自引擎或 wp-stream）。
    Buffering(u8),
    /// 首帧已渲染（可用于隐藏封面/loading）。
    FirstFrame,
    EndOfFile,
    Error {
        message: String,
        /// 是否可恢复（如链接过期 → 换链重试）。
        recoverable: bool,
    },
    /// 自定义扩展事件（调试/未来扩展）。
    Custom(String),
}

/// 事件处理器。
pub type EventHandler = Box<dyn Fn(PlayerEvent) + Send + Sync>;

/// 播放引擎抽象。
///
/// 实现者需保证：`snapshot` 可随时调用（线程安全/低开销）；`command` 非阻塞或短阻塞；
/// 事件通过 [`EventHandler`] 回调（可在引擎线程回调，上层负责转 channel）。
pub trait MediaEngine: Send {
    fn open(&mut self, source: MediaSource) -> Result<()>;
    fn command(&mut self, command: PlayerCommand) -> Result<()>;
    fn snapshot(&self) -> PlaybackSnapshot;
    fn set_event_handler(&mut self, handler: EventHandler);
    fn shutdown(&mut self);
}

/// 演示/测试用引擎：不加载真实媒体，供 GUI 开发与截图回归使用。
#[derive(Default)]
pub struct MockEngine {
    snapshot: PlaybackSnapshot,
    handler: Option<EventHandler>,
}

impl MockEngine {
    pub fn new() -> Self {
        Self::default()
    }

    fn emit(&self, event: PlayerEvent) {
        if let Some(h) = &self.handler {
            h(event);
        }
    }
}

impl MediaEngine for MockEngine {
    fn open(&mut self, source: MediaSource) -> Result<()> {
        let name = match &source {
            MediaSource::Local(p) => p.display().to_string(),
            MediaSource::Network(n) => n.display.clone(),
            MediaSource::BluRay(b) => b.iso_or_dir.display().to_string(),
        };
        self.snapshot = PlaybackSnapshot {
            state: PlayerState::Playing,
            position_secs: 0.0,
            duration_secs: Some(3600.0),
            speed: 1.0,
            volume: 100.0,
            muted: false,
            tracks: vec![
                TrackInfo {
                    id: 1,
                    kind: Some(TrackKind::Video),
                    title: name.clone(),
                    codec: "hevc".into(),
                    selected: true,
                    default: true,
                    ..Default::default()
                },
                TrackInfo {
                    id: 1,
                    kind: Some(TrackKind::Audio),
                    title: "中文 5.1".into(),
                    lang: "zh".into(),
                    codec: "ac3".into(),
                    selected: true,
                    default: true,
                    ..Default::default()
                },
                TrackInfo {
                    id: 1,
                    kind: Some(TrackKind::Subtitle),
                    title: "简体中文".into(),
                    lang: "zh".into(),
                    codec: "ass".into(),
                    selected: true,
                    default: true,
                    ..Default::default()
                },
            ],
            subtitle_track: Some(1),
            audio_track: Some(1),
            video_size: Some((3840, 2160)),
            ..Default::default()
        };
        self.emit(PlayerEvent::Opened {
            duration_secs: Some(3600.0),
            tracks: self.snapshot.tracks.clone(),
            video_size: Some((3840, 2160)),
        });
        self.emit(PlayerEvent::StateChanged(PlayerState::Playing));
        Ok(())
    }

    fn command(&mut self, command: PlayerCommand) -> Result<()> {
        match command {
            PlayerCommand::Play => self.snapshot.state = PlayerState::Playing,
            PlayerCommand::Pause | PlayerCommand::TogglePause => {
                self.snapshot.state = match self.snapshot.state {
                    PlayerState::Playing => PlayerState::Paused,
                    _ => PlayerState::Playing,
                }
            }
            PlayerCommand::Stop => {
                self.snapshot.state = PlayerState::Idle;
                self.snapshot.position_secs = 0.0;
            }
            PlayerCommand::SeekAbsolute(t) => self.snapshot.position_secs = t.max(0.0),
            PlayerCommand::SeekRelative(d) => {
                self.snapshot.position_secs = (self.snapshot.position_secs + d).max(0.0)
            }
            PlayerCommand::SetVolume(v) => self.snapshot.volume = v.clamp(0.0, 100.0),
            PlayerCommand::SetMuted(m) => self.snapshot.muted = m,
            PlayerCommand::SetSpeed(s) => self.snapshot.speed = s,
            PlayerCommand::SetSecondarySubtitleVisible(v) => {
                self.snapshot.secondary_subtitle_visible = v
            }
            _ => {}
        }
        self.emit(PlayerEvent::StateChanged(self.snapshot.state));
        Ok(())
    }

    fn snapshot(&self) -> PlaybackSnapshot {
        self.snapshot.clone()
    }

    fn set_event_handler(&mut self, handler: EventHandler) {
        self.handler = Some(handler);
    }

    fn shutdown(&mut self) {}
}
