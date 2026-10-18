//! AI 任务队列：tokio 后台 worker（优先级 / 取消 / 超时 / 并发上限 / 播放期策略 / 事件回传）。
//!
//! ## 用法
//!
//! ```no_run
//! use std::time::Duration;
//! use wp_ai::provider::build_provider_registry;
//! use wp_ai::service::{AiService, AiServiceConfig};
//! use wp_ai::{AiConfig, TranslateOptions};
//!
//! # async fn demo() -> wp_core::Result<()> {
//! let ai = AiConfig::default();
//! let (registry, _warnings) = build_provider_registry(&ai);
//! let config = AiServiceConfig::new(ai)
//!     .with_max_concurrent(1)              // 播放优先：默认串行，避免抢 CPU
//!     .with_task_timeout(Duration::from_secs(120));
//! let (service, mut events) = AiService::spawn(config, registry)?;
//! service.set_playing(true);               // 播放开始 → 非紧急任务会被拒绝
//! let id = service.submit_translate(vec![], TranslateOptions::default(), None)?;
//! while let Some(event) = events.recv().await {
//!     println!("任务 {} 完成：{:?}", event.task_id, event.outcome);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## 队列语义
//! - **优先级**：`TaskSpec::priority` 数值越小越先执行；同优先级按提交顺序（FIFO）。
//! - **并发上限**：`max_concurrent`（默认 1）。播放优先设计：本地/远端推理默认串行。
//! - **超时**：`task_timeout` 到点中止任务并回传 `Failed{retryable:true}`。
//! - **取消**：`cancel(id)`；排队中的直接移除，执行中的通过 `select!` 协作取消并回传 `Canceled`。
//! - **播放期策略**：`set_playing(true)` 后，若 `AiConfig::allow_during_playback == false`，
//!   非 `urgent` 任务在提交时立刻返回明确错误（[`crate::is_playback_rejected`] 可识别），
//!   已在队列中的任务在执行前会再次校验并回传 `Rejected`。
//! - **结果回传**：所有任务状态通过 `mpsc::UnboundedReceiver<AiEvent>` 回传（同 id 只回传一次）。
//! - **缓存**：翻译任务带 `CacheKey` 时先查缓存（命中跳过 Provider），成功后写回缓存。
//!
//! ## 与「独立 worker 进程」的关系
//! 架构 §7 要求重活（本地大模型）最终跑在独立进程做隔离；本模块是进程内异步队列，
//! 提供相同的任务语义（id/优先级/取消/超时/事件），后续把 `execute_task` 换成 IPC 调用即可，
//! 上层 GUI 接口不变。

use std::collections::{BinaryHeap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use wp_core::{Error, Result};
use wp_subtitle::{Cue, SubtitleDoc};

use crate::cache::{AiCache, CacheKey};
use crate::provider::{
    build_provider_registry, complete_with_fallback, provider_statuses,
    transcribe_file_with_fallback, transcribe_with_fallback, translate_with_fallback,
};
use crate::remote::is_transient_error;
use crate::{
    playback_rejected, AiConfig, AiTaskKind, ProviderRegistry, ProviderStatus, TranslateOptions,
};

/// 任务 id（单调递增；从 1 开始）。
pub type TaskId = u64;

/// 音频来源（ASR 任务）。
#[derive(Debug, Clone)]
pub enum AudioSource {
    /// 内存中的音频字节（wav/mp3/flac 等，本地 Provider 会按文件头猜扩展名写临时文件）。
    Bytes(Vec<u8>),
    /// 音频文件路径（推荐；whisper-cli 直接读取，不复制内容）。
    File(PathBuf),
}

/// 任务负载（与 [`AiTaskKind`] 配合使用）。
#[derive(Debug, Clone)]
pub enum TaskPayload {
    /// 字幕/弹幕翻译。
    Translate {
        cues: Vec<Cue>,
        opts: TranslateOptions,
        /// 有指纹时启用结果缓存。
        cache_key: Option<CacheKey>,
    },
    /// 音频转写（P2）：字节或文件路径，可选缓存键。
    Transcribe {
        source: AudioSource,
        lang: String,
        /// 有指纹时启用结果缓存。
        cache_key: Option<CacheKey>,
    },
    /// 文本生成（P3）。
    Complete { prompt: String },
}

/// 任务提交描述。
#[derive(Debug, Clone)]
pub struct TaskSpec {
    pub kind: AiTaskKind,
    /// 优先级：数值越小越先执行（默认 100）。
    pub priority: u8,
    /// 紧急任务：播放期间也允许执行（如实时字幕前瞻）。
    pub urgent: bool,
    pub payload: TaskPayload,
}

impl TaskSpec {
    /// 翻译字幕（默认优先级 100，非紧急）。
    pub fn translate_subtitles(
        cues: Vec<Cue>,
        opts: TranslateOptions,
        cache_key: Option<CacheKey>,
    ) -> Self {
        Self::translate(AiTaskKind::TranslateSubtitles, cues, opts, cache_key)
    }

    /// 翻译弹幕（复用翻译能力）。
    pub fn translate_danmaku(
        cues: Vec<Cue>,
        opts: TranslateOptions,
        cache_key: Option<CacheKey>,
    ) -> Self {
        Self::translate(AiTaskKind::TranslateDanmaku, cues, opts, cache_key)
    }

    /// 指定任务类型的翻译。
    pub fn translate(
        kind: AiTaskKind,
        cues: Vec<Cue>,
        opts: TranslateOptions,
        cache_key: Option<CacheKey>,
    ) -> Self {
        Self {
            kind,
            priority: 100,
            urgent: false,
            payload: TaskPayload::Translate {
                cues,
                opts,
                cache_key,
            },
        }
    }

    /// 音频转写（内存字节）。
    pub fn transcribe(audio: Vec<u8>, lang: impl Into<String>) -> Self {
        Self::transcribe_source(AudioSource::Bytes(audio), lang, None)
    }

    /// 音频文件转写（推荐：`whisper-cli` 直接读路径，不复制音频内容）。
    pub fn transcribe_file(path: impl Into<PathBuf>, lang: impl Into<String>) -> Self {
        Self::transcribe_source(AudioSource::File(path.into()), lang, None)
    }

    /// 带缓存键的音频转写（缓存键 = 文件指纹 + 模型 + 语言 + 任务类型）。
    pub fn transcribe_cached(
        source: AudioSource,
        lang: impl Into<String>,
        cache_key: Option<CacheKey>,
    ) -> Self {
        Self::transcribe_source(source, lang, cache_key)
    }

    fn transcribe_source(
        source: AudioSource,
        lang: impl Into<String>,
        cache_key: Option<CacheKey>,
    ) -> Self {
        Self {
            kind: AiTaskKind::TranscribeAudio,
            priority: 100,
            urgent: false,
            payload: TaskPayload::Transcribe {
                source,
                lang: lang.into(),
                cache_key,
            },
        }
    }

    /// 文本生成（摘要/问答）。
    pub fn complete(prompt: impl Into<String>) -> Self {
        Self {
            kind: AiTaskKind::Summarize,
            priority: 100,
            urgent: false,
            payload: TaskPayload::Complete {
                prompt: prompt.into(),
            },
        }
    }

    /// 设置优先级（数值越小越优先）。
    pub fn with_priority(mut self, priority: u8) -> Self {
        self.priority = priority;
        self
    }

    /// 标记/取消紧急任务（紧急任务播放期间也允许执行）。
    pub fn with_urgent(mut self, urgent: bool) -> Self {
        self.urgent = urgent;
        self
    }
}

/// 任务成功结果。
#[derive(Debug, Clone)]
pub enum AiResult {
    /// 翻译结果（与输入 cue 等长）。
    Translations {
        translations: Vec<String>,
        /// 实际使用的 Provider（缓存命中为 None）。
        provider_id: Option<String>,
        /// 是否命中缓存。
        cached: bool,
    },
    /// 转写字幕文档。
    Subtitle {
        doc: SubtitleDoc,
        /// 实际使用的 Provider（缓存命中为 None）。
        provider_id: Option<String>,
        /// 是否命中缓存。
        cached: bool,
    },
    /// 文本生成结果。
    Text { text: String, provider_id: String },
}

/// 任务终态。
#[derive(Debug, Clone)]
pub enum AiOutcome {
    /// 成功。
    Done(AiResult),
    /// 失败（`retryable` 表示瞬时错误，可重试）。
    Failed { message: String, retryable: bool },
    /// 用户取消。
    Canceled,
    /// 播放期策略拒绝（提交时拒绝会直接返回 Err，这里用于「排队后开始播放」的复查）。
    Rejected { message: String },
}

impl AiOutcome {
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done(_))
    }

    pub fn is_canceled(&self) -> bool {
        matches!(self, Self::Canceled)
    }

    /// 中文单行摘要（UI 日志/状态栏用）。
    pub fn summary(&self) -> String {
        match self {
            Self::Done(AiResult::Translations {
                translations,
                cached,
                ..
            }) => format!(
                "翻译完成（{} 条{}）",
                translations.len(),
                if *cached { "，命中缓存" } else { "" }
            ),
            Self::Done(AiResult::Subtitle { doc, cached, .. }) => format!(
                "转写完成（{} 条{}）",
                doc.cues.len(),
                if *cached { "，命中缓存" } else { "" }
            ),
            Self::Done(AiResult::Text { text, .. }) => {
                format!("生成完成（{} 字）", text.chars().count())
            }
            Self::Failed { message, .. } => format!("失败：{message}"),
            Self::Canceled => "已取消".to_string(),
            Self::Rejected { message } => format!("被拒绝：{message}"),
        }
    }
}

/// 任务状态事件（结果回传 channel 的唯一消息类型）。
#[derive(Debug, Clone)]
pub struct AiEvent {
    pub task_id: TaskId,
    pub kind: AiTaskKind,
    pub outcome: AiOutcome,
}

/// 队列/缓存可调配置。
#[derive(Debug, Clone)]
pub struct AiServiceConfig {
    /// Provider 配置（enabled/priority/offline_only/allow_during_playback）。
    pub ai: AiConfig,
    /// 同时执行任务上限（默认 1；播放优先设计）。
    pub max_concurrent: usize,
    /// 单任务超时（默认 120s）。
    pub task_timeout: Duration,
    /// 缓存目录（None=`数据/缓存/ai`）。
    pub cache_dir: Option<PathBuf>,
    /// 缓存条目上限（默认 200；0=禁用缓存）。
    pub cache_max_entries: usize,
}

impl Default for AiServiceConfig {
    fn default() -> Self {
        Self {
            ai: AiConfig::default(),
            max_concurrent: 1,
            task_timeout: Duration::from_secs(120),
            cache_dir: None,
            cache_max_entries: AiCache::DEFAULT_MAX_ENTRIES,
        }
    }
}

impl AiServiceConfig {
    /// 基于 [`AiConfig`] 构造（其余取默认：并发 1 / 超时 120s / 缓存上限 200）。
    pub fn new(ai: AiConfig) -> Self {
        Self {
            ai,
            ..Self::default()
        }
    }

    pub fn with_max_concurrent(mut self, max_concurrent: usize) -> Self {
        self.max_concurrent = max_concurrent.max(1);
        self
    }

    pub fn with_task_timeout(mut self, task_timeout: Duration) -> Self {
        self.task_timeout = task_timeout;
        self
    }

    pub fn with_cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = Some(dir.into());
        self
    }

    pub fn with_cache_max_entries(mut self, max_entries: usize) -> Self {
        self.cache_max_entries = max_entries;
        self
    }
}

/// 默认缓存目录：`数据/缓存/ai`（绿色版/U 盘/系统用户目录，见 `wp_core::Paths`）。
pub fn default_cache_dir() -> PathBuf {
    wp_core::Paths::detect().cache_dir().join("ai")
}

struct ServiceShared {
    cfg: AiServiceConfig,
    registry: ProviderRegistry,
    cache: AiCache,
    playing: AtomicBool,
}

enum WorkerCmd {
    Submit { id: TaskId, spec: TaskSpec },
    Cancel { id: TaskId },
}

struct QueuedTask {
    id: TaskId,
    spec: TaskSpec,
}

impl PartialEq for QueuedTask {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for QueuedTask {}

impl PartialOrd for QueuedTask {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueuedTask {
    /// 最小堆语义：priority 小者优先；同优先级 id 小者（先提交）优先。
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .spec
            .priority
            .cmp(&self.spec.priority)
            .then_with(|| other.id.cmp(&self.id))
    }
}

/// AI 任务队列服务（后台 worker 句柄）。
///
/// 通过 [`AiService::spawn`] 创建，worker 在 tokio 运行时后台执行任务；
/// 结果/状态从返回的 `mpsc::UnboundedReceiver<AiEvent>` 读取。
pub struct AiService {
    shared: Arc<ServiceShared>,
    tx: mpsc::UnboundedSender<WorkerCmd>,
    next_id: AtomicU64,
}

impl AiService {
    /// 启动后台 worker。必须在 tokio 运行时内调用。
    pub fn spawn(
        config: AiServiceConfig,
        registry: ProviderRegistry,
    ) -> Result<(Self, mpsc::UnboundedReceiver<AiEvent>)> {
        tokio::runtime::Handle::try_current()
            .map_err(|_| Error::other("AiService::spawn 必须在 tokio 运行时内调用"))?;
        let cache_dir = config.cache_dir.clone().unwrap_or_else(default_cache_dir);
        let cache = AiCache::new(cache_dir).with_max_entries(config.cache_max_entries);
        let shared = Arc::new(ServiceShared {
            cfg: config,
            registry,
            cache,
            playing: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        tokio::spawn(worker_loop(shared.clone(), rx, event_tx));
        Ok((
            Self {
                shared,
                tx,
                next_id: AtomicU64::new(1),
            },
            event_rx,
        ))
    }

    /// 从 [`AiServiceConfig`] 构造（自动为启用的 Provider 建实例），返回 `(service, events, warnings)`。
    pub fn from_config(
        config: AiServiceConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<AiEvent>, Vec<String>)> {
        let (registry, warnings) = build_provider_registry(&config.ai);
        let (service, events) = Self::spawn(config, registry)?;
        Ok((service, events, warnings))
    }

    /// 提交任务。播放期策略拒绝时返回可读错误（[`crate::is_playback_rejected`] 可识别）。
    pub fn submit(&self, spec: TaskSpec) -> Result<TaskId> {
        self.check_playback(&spec)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.tx
            .send(WorkerCmd::Submit { id, spec })
            .map_err(|_| Error::other("AI 服务已关闭（worker 已退出）"))?;
        Ok(id)
    }

    /// 提交字幕翻译任务（`kind = TranslateSubtitles`）。
    pub fn submit_translate(
        &self,
        cues: Vec<Cue>,
        opts: TranslateOptions,
        cache_key: Option<CacheKey>,
    ) -> Result<TaskId> {
        self.submit(TaskSpec::translate_subtitles(cues, opts, cache_key))
    }

    /// 提交整片字幕翻译（自动按当前最高优先级候选模型生成缓存键；空指纹=不缓存）。
    pub fn submit_subtitle_translation(
        &self,
        doc: &SubtitleDoc,
        opts: TranslateOptions,
        fingerprint: &str,
    ) -> Result<TaskId> {
        let task = AiTaskKind::TranslateSubtitles;
        let model = self
            .shared
            .registry
            .candidate_configs(task)
            .first()
            .map(|p| p.model.clone())
            .unwrap_or_default();
        let cache_key = if fingerprint.trim().is_empty() {
            None
        } else {
            Some(CacheKey::new(
                fingerprint,
                model,
                opts.target_lang.clone(),
                task,
            ))
        };
        self.submit(TaskSpec::translate_subtitles(
            doc.cues.clone(),
            opts,
            cache_key,
        ))
    }

    /// 提交音频转写任务（内存字节）。
    pub fn submit_transcribe(&self, audio: Vec<u8>, lang: impl Into<String>) -> Result<TaskId> {
        self.submit(TaskSpec::transcribe(audio, lang))
    }

    /// 提交音频**文件**转写任务（带结果缓存；`fingerprint` 空=不缓存）。
    pub fn submit_transcribe_file(
        &self,
        path: impl Into<PathBuf>,
        lang: impl Into<String>,
        fingerprint: &str,
    ) -> Result<TaskId> {
        let lang = lang.into();
        let cache_key = self.transcribe_cache_key(fingerprint, &lang);
        self.submit(TaskSpec::transcribe_cached(
            AudioSource::File(path.into()),
            lang,
            cache_key,
        ))
    }

    /// 计算 ASR 缓存键（模型取自当前最高优先级候选配置）。
    fn transcribe_cache_key(&self, fingerprint: &str, lang: &str) -> Option<CacheKey> {
        if fingerprint.trim().is_empty() {
            return None;
        }
        let model = self
            .shared
            .registry
            .candidate_configs(AiTaskKind::TranscribeAudio)
            .first()
            .map(|p| p.model.clone())
            .unwrap_or_default();
        Some(CacheKey::new(
            fingerprint,
            model,
            lang,
            AiTaskKind::TranscribeAudio,
        ))
    }

    /// 提交文本生成任务。
    pub fn submit_complete(&self, prompt: impl Into<String>) -> Result<TaskId> {
        self.submit(TaskSpec::complete(prompt))
    }

    /// 取消任务：排队中的直接移除；执行中的协作取消。未知/已完成 id 忽略（不报错）。
    pub fn cancel(&self, id: TaskId) -> Result<()> {
        self.tx
            .send(WorkerCmd::Cancel { id })
            .map_err(|_| Error::other("AI 服务已关闭（worker 已退出）"))
    }

    /// 更新播放状态；`playing=true` 时按 `allow_during_playback` 拒绝非紧急任务。
    pub fn set_playing(&self, playing: bool) {
        self.shared.playing.store(playing, Ordering::Relaxed);
    }

    /// 当前是否处于播放状态。
    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    /// 服务配置。
    pub fn config(&self) -> &AiServiceConfig {
        &self.shared.cfg
    }

    /// Provider 注册表（只读）。
    pub fn providers(&self) -> &ProviderRegistry {
        &self.shared.registry
    }

    /// 结果缓存（只读）。
    pub fn cache(&self) -> &AiCache {
        &self.shared.cache
    }

    /// AI 面板用 Provider 状态列表（不含密钥）。
    pub fn provider_statuses(&self) -> Vec<ProviderStatus> {
        provider_statuses(&self.shared.registry)
    }

    /// 提交通道是否已关闭（worker 已退出）。
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    fn check_playback(&self, spec: &TaskSpec) -> Result<()> {
        if self.shared.playing.load(Ordering::Relaxed)
            && !self.shared.cfg.ai.allows_task_during_playback(spec.urgent)
        {
            return Err(playback_rejected(format!("任务「{}」", spec.kind.label())));
        }
        Ok(())
    }
}

type TaskRunResult = (TaskId, AiTaskKind, AiOutcome);

async fn worker_loop(
    shared: Arc<ServiceShared>,
    mut cmd_rx: mpsc::UnboundedReceiver<WorkerCmd>,
    events: mpsc::UnboundedSender<AiEvent>,
) {
    let max_concurrent = shared.cfg.max_concurrent.max(1);
    let mut queue: BinaryHeap<QueuedTask> = BinaryHeap::new();
    let mut inflight: JoinSet<TaskRunResult> = JoinSet::new();
    let mut cancel_senders: HashMap<TaskId, oneshot::Sender<()>> = HashMap::new();
    let mut closed = false;
    loop {
        // 1) 有空闲并发槽则从优先级队列取任务启动。
        while inflight.len() < max_concurrent {
            let Some(task) = queue.pop() else {
                break;
            };
            // 播放期复查：提交后被切到播放态的任务在此被拒绝（回传 Rejected 而非静默丢弃）。
            if shared.playing.load(Ordering::Relaxed)
                && !shared.cfg.ai.allows_task_during_playback(task.spec.urgent)
            {
                let _ = events.send(AiEvent {
                    task_id: task.id,
                    kind: task.spec.kind,
                    outcome: AiOutcome::Rejected {
                        message: playback_rejected(format!("任务「{}」", task.spec.kind.label()))
                            .to_string(),
                    },
                });
                continue;
            }
            let (cancel_tx, cancel_rx) = oneshot::channel();
            cancel_senders.insert(task.id, cancel_tx);
            let task_id = task.id;
            let kind = task.spec.kind;
            let spec = task.spec;
            let shared_for_task = shared.clone();
            inflight.spawn(async move {
                let outcome = run_task(&shared_for_task, spec, cancel_rx).await;
                (task_id, kind, outcome)
            });
        }

        // 2) 全部排空且提交端关闭 → 退出 worker。
        if closed && queue.is_empty() && inflight.is_empty() {
            break;
        }

        // 3) 等待新命令 / 任务完成。
        tokio::select! {
            cmd = cmd_rx.recv(), if !closed => {
                match cmd {
                    Some(WorkerCmd::Submit { id, spec }) => queue.push(QueuedTask { id, spec }),
                    Some(WorkerCmd::Cancel { id }) => {
                        if let Some(tx) = cancel_senders.remove(&id) {
                            // 执行中：通知协作取消（任务会在 select 中立即返回 Canceled）。
                            let _ = tx.send(());
                        } else if let Some(kind) = queue.iter().find(|t| t.id == id).map(|t| t.spec.kind) {
                            // 排队中：直接移除并回传 Canceled。
                            queue.retain(|t| t.id != id);
                            let _ = events.send(AiEvent { task_id: id, kind, outcome: AiOutcome::Canceled });
                        }
                        // 未知/已完成 id：忽略。
                    }
                    None => closed = true,
                }
            }
            Some(joined) = inflight.join_next(), if !inflight.is_empty() => {
                match joined {
                    Ok((id, kind, outcome)) => {
                        cancel_senders.remove(&id);
                        let _ = events.send(AiEvent { task_id: id, kind, outcome });
                    }
                    Err(join_err) => {
                        // 任务 panic/运行时中止：不让 worker 崩溃（事件缺失，stderr 留痕）。
                        eprintln!("wp-ai：后台 AI 任务异常结束：{join_err}");
                    }
                }
            }
        }
    }
}

async fn run_task(
    shared: &ServiceShared,
    spec: TaskSpec,
    mut cancel_rx: oneshot::Receiver<()>,
) -> AiOutcome {
    let timeout = shared.cfg.task_timeout;
    let timeout_secs = timeout.as_secs().max(1);
    tokio::select! {
        outcome = execute_task(shared, spec) => outcome,
        _ = &mut cancel_rx => AiOutcome::Canceled,
        _ = tokio::time::sleep(timeout) => AiOutcome::Failed {
            message: format!("AI 任务超时（{timeout_secs}s）：已中止"),
            retryable: true,
        },
    }
}

async fn execute_task(shared: &ServiceShared, spec: TaskSpec) -> AiOutcome {
    match spec.payload {
        TaskPayload::Translate {
            cues,
            opts,
            cache_key,
        } => {
            if let Some(key) = &cache_key {
                if let Some(translations) = shared.cache.get_translations(key) {
                    if translations.len() == cues.len() {
                        return AiOutcome::Done(AiResult::Translations {
                            translations,
                            provider_id: None,
                            cached: true,
                        });
                    }
                }
            }
            match translate_with_fallback(&shared.registry, spec.kind, &cues, &opts).await {
                Ok((translations, provider_id)) => {
                    if let Some(key) = &cache_key {
                        let _ = shared.cache.put_translations(key, &translations);
                    }
                    AiOutcome::Done(AiResult::Translations {
                        translations,
                        provider_id: Some(provider_id),
                        cached: false,
                    })
                }
                Err(e) => failed_from_error(e),
            }
        }
        TaskPayload::Transcribe {
            source,
            lang,
            cache_key,
        } => {
            if let Some(key) = &cache_key {
                if let Some(doc) = shared.cache.get_subtitle_doc(key) {
                    return AiOutcome::Done(AiResult::Subtitle {
                        doc,
                        provider_id: None,
                        cached: true,
                    });
                }
            }
            let result = match &source {
                AudioSource::Bytes(audio) => {
                    transcribe_with_fallback(&shared.registry, audio, &lang).await
                }
                AudioSource::File(path) => {
                    transcribe_file_with_fallback(&shared.registry, path, &lang).await
                }
            };
            match result {
                Ok((doc, provider_id)) => {
                    if let Some(key) = &cache_key {
                        let _ = shared.cache.put_subtitle_doc(key, &doc);
                    }
                    AiOutcome::Done(AiResult::Subtitle {
                        doc,
                        provider_id: Some(provider_id),
                        cached: false,
                    })
                }
                Err(e) => failed_from_error(e),
            }
        }
        TaskPayload::Complete { prompt } => {
            match complete_with_fallback(&shared.registry, &prompt).await {
                Ok((text, provider_id)) => AiOutcome::Done(AiResult::Text { text, provider_id }),
                Err(e) => failed_from_error(e),
            }
        }
    }
}

fn failed_from_error(err: Error) -> AiOutcome {
    if err.is_canceled() {
        AiOutcome::Canceled
    } else {
        AiOutcome::Failed {
            retryable: is_transient_error(&err),
            message: err.to_string(),
        }
    }
}
