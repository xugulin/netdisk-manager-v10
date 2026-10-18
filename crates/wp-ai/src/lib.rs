//! # wp-ai —— AI 子系统：Provider 抽象 / 任务队列 / 结果缓存 / 字幕翻译管线
//!
//! ## 设计原则（见 `docs/PLAYER_ARCHITECTURE.md` §7）
//!
//! 1. **播放优先**：AI 任务默认只在暂停/空闲时执行；播放期间由
//!    [`AiConfig::allow_during_playback`] 全局控制，单任务可用 `urgent` 标记临时放行。
//!    队列默认并发 1，避免与 4K 解码抢 CPU；本地大模型后续跑在独立 worker 进程（架构 §7）。
//! 2. **本地 + 远端两者都要**：[`ProviderKind`] 统一抽象，可分别 `enabled`（启停）、
//!    `priority`（数值越小越优先，失败按优先级回退下一个）。
//! 3. **隐私可控**：[`AiConfig::offline_only`] = true 时只使用本地 Provider，
//!    远端 `api_key` 从不出现在日志/Debug/错误信息里（见 [`ProviderConfig`] 的自定义 `Debug`）。
//! 4. **缓存可复用**：[`cache::AiCache`] 按（文件指纹 + 模型 + 目标语言 + 任务类型）缓存结果，
//!    二次翻译零耗时。
//!
//! ## 分层
//!
//! | 层 | 入口 |
//! |---|---|
//! | 契约/配置 | [`ProviderKind`] [`ProviderConfig`] [`AiConfig`] [`TranslateOptions`] [`AiProvider`] |
//! | Provider 实现 | [`remote::RemoteOpenAiProvider`]（OpenAI 兼容 REST）、[`mock::MockProvider`]（测试/演示）、`local_llm::LocalLlmProvider`（本地 llama-server 等 OpenAI 兼容服务，feature `local-llm`）、`local_whisper::LocalWhisperProvider`（whisper-cli 外部进程，feature `local-whisper`） |
//! | 选择/回退 | [`select_provider`]、[`provider::translate_with_fallback`] / `complete_with_fallback` / `transcribe_with_fallback` |
//! | 任务队列 | [`service::AiService`]（id/优先级/取消/超时/并发上限/播放期策略/事件回传） |
//! | 结果缓存 | [`cache::AiCache`]（JSON 文件、原子写、LRU 上限） |
//! | 字幕翻译管线 | [`pipeline::translate_subtitle_document`]（分批、术语表、缓存、失败降级原文） |
//! | ASR 字幕管线 | [`pipeline::transcribe_to_subtitle`] / [`pipeline::transcribe_file_to_subtitle`]（whisper.cpp、缓存、失败降级） |
//! | 集成闭环 | [`integration`]：翻译 → 双语 ASS → 原子写文件（漏译统计/失败降级）；ffmpeg 抽 16kHz wav（ASR 前置） |
//! | P3 摘要/章节 | [`summarize::summarize_subtitles`]：分批窗口 → JSON 章节（容错/回退）→ 时间轴合法化 → 缓存 |
//!
//! ## 配置示例
//!
//! `AiConfig` 直接序列化为应用配置 JSON（`ai` 字段）：
//!
//! ```json
//! {
//!   "providers": [
//!     {
//!       "id": "deepseek",
//!       "kind": "RemoteOpenAi",
//!       "enabled": true,
//!       "priority": 20,
//!       "base_url": "https://api.deepseek.com/v1",
//!       "model": "deepseek-chat",
//!       "api_key": "sk-…",
//!       "timeout_secs": 60,
//!       "max_retries": 2,
//!       "batch_size": 20
//!     },
//!     {
//!       "id": "openai",
//!       "kind": "RemoteOpenAi",
//!       "enabled": false,
//!       "priority": 30,
//!       "base_url": "https://api.openai.com/v1",
//!       "model": "gpt-4o-mini",
//!       "api_key": ""
//!     },
//!     {
//!       "id": "local-llama",
//!       "kind": "LocalLlm",
//!       "enabled": true,
//!       "priority": 10,
//!       "base_url": "http://127.0.0.1:8080/v1",
//!       "model": "qwen2.5-7b-instruct",
//!       "timeout_secs": 300
//!     },
//!     {
//!       "id": "local-whisper",
//!       "kind": "LocalWhisper",
//!       "enabled": false,
//!       "priority": 30,
//!       "model": "/opt/models/ggml-medium.bin",
//!       "cli_path": "/opt/whisper.cpp/build/bin/whisper-cli",
//!       "threads": 8,
//!       "timeout_secs": 600
//!     }
//!   ],
//!   "offline_only": false,
//!   "allow_during_playback": false
//! }
//! ```
//!
//! 说明：
//! - `base_url` 应指向 **API 版本前缀**（如 `/v1`）。若只写主机（`https://api.deepseek.com`），
//!   会自动补 `/v1`；请求地址 = `{base_url}/chat/completions`。
//! - `priority` 越小越优先；优先级最高的失败后按顺序回退到下一个**已启用且能力匹配**的 Provider。
//! - 本地 LLM：`LocalLlm` 走 OpenAI 兼容本地服务（llama-server / vLLM / Ollama），`base_url` 空时
//!   默认 `http://127.0.0.1:8080/v1`；连接失败会按优先级回退到下一个 Provider。
//! - 本地 ASR：`LocalWhisper` 调 `whisper-cli` 外部进程；`model` 填 ggml 模型路径、`cli_path` 填
//!   可执行文件（空=`PATH` 查找或环境变量 `WP_WHISPER_CLI`）、`threads` 填 `-t`。详见 [`local_whisper`]。
//!
//! ## 快速开始（字幕翻译，带缓存/降级）
//!
//! ```no_run
//! use wp_ai::cache::AiCache;
//! use wp_ai::pipeline::translate_subtitle_document;
//! use wp_ai::provider::build_provider_registry;
//! use wp_ai::{AiConfig, TranslateOptions};
//! use wp_subtitle::SubtitleDoc;
//!
//! # async fn demo(doc: &SubtitleDoc, fingerprint: &str) -> wp_core::Result<()> {
//! let ai = AiConfig::default(); // 从配置.json 反序列化得到
//! let (registry, warnings) = build_provider_registry(&ai);
//! for w in warnings {
//!     eprintln!("AI Provider 不可用：{w}");
//! }
//! let cache = AiCache::new(std::env::temp_dir().join("wp-ai-demo"));
//! let opts = TranslateOptions::default();
//! let outcome = translate_subtitle_document(&registry, &cache, fingerprint, doc, &opts).await;
//! if outcome.degraded {
//!     eprintln!("翻译降级：{:?}", outcome.errors);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## 任务路线
//!
//! P1 字幕翻译（已完成）→ P2 ASR 生成字幕（本版完成本地 whisper-cli 管线；远端 ASR 待接）→
//! P3 摘要/章节（原型已完成；问答待做）→ P4 OCR/超分。

pub mod cache;
pub mod integration;
pub mod mock;
pub mod pipeline;
pub mod provider;
pub mod remote;
pub mod service;
pub mod summarize;

#[cfg(feature = "local-llm")]
pub mod local_llm;
#[cfg(feature = "local-whisper")]
pub mod local_whisper;

use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;
use wp_core::{Error, Result};

pub use cache::{fingerprint_bytes, fingerprint_file_metadata, AiCache, CacheKey};
pub use integration::{
    extract_audio_ffmpeg, extract_audio_ffmpeg_with, extract_audio_for_asr,
    translate_doc_and_write_ass, translate_to_bilingual_ass, translate_to_bilingual_ass_report,
    BilingualResult, FfmpegOptions,
};
pub use mock::MockProvider;
pub use pipeline::{
    build_translated_doc, transcribe_file_to_subtitle, transcribe_to_subtitle,
    translate_subtitle_document, TranscribeOutcome, TranslateOutcome,
};
pub use provider::{
    build_provider, build_provider_registry, complete_with_fallback, provider_statuses,
    transcribe_file_with_fallback, transcribe_with_fallback, translate_with_fallback,
    ProviderRegistry, ProviderStatus,
};
pub use remote::{RemoteOpenAiProvider, RemoteOptions};
pub use service::{
    AiEvent, AiOutcome, AiResult, AiService, AiServiceConfig, AudioSource, TaskId, TaskPayload,
    TaskSpec,
};
pub use summarize::{
    normalize_chapters, summarize_subtitles, summary_to_markdown, AiSummary, Chapter,
    SummarizeOptions, SummarizeOutcome,
};

#[cfg(feature = "local-llm")]
pub use local_llm::LocalLlmProvider;
#[cfg(feature = "local-whisper")]
pub use local_whisper::LocalWhisperProvider;

/// AI 功能类别（P1 → P4 路线）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AiTaskKind {
    /// P1：字幕翻译（双语输出）。
    TranslateSubtitles,
    /// 弹幕翻译（复用翻译能力，结果注入 ASS/egui 弹幕层）。
    TranslateDanmaku,
    /// P2：音频转写生成字幕。
    TranscribeAudio,
    /// P3：摘要/章节。
    Summarize,
    /// P3：针对字幕/画面的问答。
    AnswerQuestion,
    /// P4：位图字幕 OCR。
    OcrSubtitles,
}

impl AiTaskKind {
    /// 中文显示名（UI 用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::TranslateSubtitles => "字幕翻译",
            Self::TranslateDanmaku => "弹幕翻译",
            Self::TranscribeAudio => "语音转写",
            Self::Summarize => "摘要/章节",
            Self::AnswerQuestion => "问答",
            Self::OcrSubtitles => "字幕 OCR",
        }
    }

    /// 能力矩阵：该任务允许由哪些 Provider 类型执行（其余类型不进入候选/回退链）。
    pub fn supports_provider(self, kind: ProviderKind) -> bool {
        use ProviderKind::{LocalLlm, LocalWhisper, Onnx, RemoteOpenAi};
        match self {
            Self::TranslateSubtitles | Self::TranslateDanmaku => {
                matches!(kind, RemoteOpenAi | LocalLlm)
            }
            Self::Summarize | Self::AnswerQuestion => matches!(kind, RemoteOpenAi | LocalLlm),
            Self::TranscribeAudio => matches!(kind, LocalWhisper | RemoteOpenAi),
            Self::OcrSubtitles => matches!(kind, Onnx),
        }
    }
}

/// Provider 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProviderKind {
    /// 远端 OpenAI 兼容 API（DeepSeek/OpenAI/Qwen/Ollama 等）。
    RemoteOpenAi,
    /// 本地 LLM（llama.cpp / llama-server，feature `local-llm` 门控）。
    LocalLlm,
    /// 本地 Whisper（whisper.cpp，feature `local-whisper` 门控）。
    LocalWhisper,
    /// ONNX Runtime（OCR/超分等专用模型，后续）。
    Onnx,
}

impl ProviderKind {
    /// 中文显示名（UI 用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::RemoteOpenAi => "远端 OpenAI 兼容",
            Self::LocalLlm => "本地 LLM",
            Self::LocalWhisper => "本地 Whisper",
            Self::Onnx => "ONNX 本地模型",
        }
    }

    /// 是否本地 Provider（离线可用）。
    pub fn is_local(self) -> bool {
        matches!(self, Self::LocalLlm | Self::LocalWhisper | Self::Onnx)
    }

    /// 是否远端（需要网络，可能把字幕内容外发）。
    pub fn is_remote(self) -> bool {
        !self.is_local()
    }
}

fn default_timeout_secs() -> u64 {
    60
}

fn default_max_retries() -> u32 {
    2
}

fn default_batch_size() -> usize {
    20
}

/// 单个 Provider 配置（可分别启用/停用/调优先级）。
///
/// 安全：`api_key` 不参与序列化输出（[`Serialize`] 时跳过），`Debug` 输出用 `***` 脱敏，
/// 任何日志/错误路径都不会打印明文密钥。
#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// 唯一 id（注册/事件/UI 都靠它）。
    pub id: String,
    pub kind: ProviderKind,
    /// 是否启用（停用的 Provider 不进入候选/回退链）。
    pub enabled: bool,
    /// 数值越小优先级越高（失败时按优先级回退下一个）。
    pub priority: u8,
    /// API 根地址（含版本前缀，如 `https://api.deepseek.com/v1`；本地如 `http://127.0.0.1:8080/v1`）。
    pub base_url: String,
    /// 模型名（远端如 `deepseek-chat`；本地如模型文件名）。
    pub model: String,
    /// API Key（空=本地/免密钥）。注意：不打印到日志。
    #[serde(default, skip_serializing)]
    pub api_key: String,
    /// 单次 HTTP 请求超时（秒，0=用默认 60s）。
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// 瞬时错误（网络/429/5xx/超时）重试次数（0=不重试）。
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// 字幕翻译分批大小（0=用默认 20）。
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    /// 本地 Provider 可执行文件路径（如 `whisper-cli`）。
    /// `None`/空 = 用环境变量 `WP_WHISPER_CLI` 或 `PATH` 查找；远端 Provider 忽略该字段。
    #[serde(default)]
    pub cli_path: Option<String>,
    /// 本地 Provider 线程数（`whisper.cpp` 的 `-t`；0=用工具默认）。远端 Provider 忽略该字段。
    #[serde(default)]
    pub threads: u32,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            id: "remote".into(),
            kind: ProviderKind::RemoteOpenAi,
            enabled: false,
            priority: 100,
            base_url: "https://api.openai.com/v1".into(),
            model: String::new(),
            api_key: String::new(),
            timeout_secs: default_timeout_secs(),
            max_retries: default_max_retries(),
            batch_size: default_batch_size(),
            cli_path: None,
            threads: 0,
        }
    }
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 手写 Debug：api_key 永久脱敏，避免 `{:?}` 泄漏到日志/错误信息。
        f.debug_struct("ProviderConfig")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("enabled", &self.enabled)
            .field("priority", &self.priority)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "<空>"
                } else {
                    "***"
                },
            )
            .field("timeout_secs", &self.timeout_secs)
            .field("max_retries", &self.max_retries)
            .field("batch_size", &self.batch_size)
            .field("cli_path", &self.cli_path)
            .field("threads", &self.threads)
            .finish()
    }
}

impl ProviderConfig {
    /// 构造一个远端 OpenAI 兼容 Provider 配置。
    pub fn remote_openai(
        id: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: ProviderKind::RemoteOpenAi,
            enabled: true,
            priority: 20,
            base_url: base_url.into(),
            model: model.into(),
            api_key: api_key.into(),
            ..Self::default()
        }
    }

    /// 构造一个本地 Provider 配置（默认不启用，等模型接好再开）。
    pub fn local(
        id: impl Into<String>,
        kind: ProviderKind,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            enabled: false,
            priority: 10,
            base_url: base_url.into(),
            model: model.into(),
            api_key: String::new(),
            ..Self::default()
        }
    }

    /// 实际使用的超时（`timeout_secs=0` 时取默认 60s）。
    pub fn effective_timeout(&self) -> Duration {
        Duration::from_secs(if self.timeout_secs == 0 {
            default_timeout_secs()
        } else {
            self.timeout_secs
        })
    }

    /// 实际使用的分批大小（`batch_size=0` 时取默认 20）。
    pub fn effective_batch_size(&self) -> usize {
        if self.batch_size == 0 {
            default_batch_size()
        } else {
            self.batch_size
        }
    }

    /// 是否配置了 API Key（只返回布尔，不暴露内容）。
    pub fn has_api_key(&self) -> bool {
        !self.api_key.trim().is_empty()
    }

    /// 安全的单行描述（用于 UI/日志，永不包含密钥）。
    pub fn display(&self) -> String {
        format!(
            "{}（{}，模型 {}，优先级 {}）",
            self.id,
            self.kind.label(),
            if self.model.is_empty() {
                "未设置"
            } else {
                &self.model
            },
            self.priority
        )
    }
}

/// AI 全局配置。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AiConfig {
    pub providers: Vec<ProviderConfig>,
    /// 离线优先（true=只用本地 Provider，不自动走远端；远端即使 enabled 也不会被选中）。
    pub offline_only: bool,
    /// 播放期间是否允许 AI 重任务（false=默认只在暂停/空闲跑；紧急任务可单独放行）。
    pub allow_during_playback: bool,
}

impl AiConfig {
    /// 已启用的 Provider。
    pub fn enabled_providers(&self) -> impl Iterator<Item = &ProviderConfig> {
        self.providers.iter().filter(|p| p.enabled)
    }

    /// 按优先级升序（相同优先级保持配置顺序）的候选配置，已应用 `enabled`/`offline_only` 过滤。
    pub fn sorted_candidates(&self) -> Vec<&ProviderConfig> {
        let mut list: Vec<&ProviderConfig> = self
            .providers
            .iter()
            .filter(|p| p.enabled)
            .filter(|p| !(self.offline_only && p.kind.is_remote()))
            .collect();
        list.sort_by_key(|p| p.priority);
        list
    }

    /// 是否有启用的本地 Provider。
    pub fn has_enabled_local(&self) -> bool {
        self.providers
            .iter()
            .any(|p| p.enabled && p.kind.is_local())
    }

    /// 是否有启用的远端 Provider（`offline_only` 不影响该判断，用于 UI 提示隐私）。
    pub fn has_enabled_remote(&self) -> bool {
        self.providers
            .iter()
            .any(|p| p.enabled && p.kind.is_remote())
    }

    /// 播放期间是否允许执行该任务（全局开关，或任务自身 urgent）。
    pub fn allows_task_during_playback(&self, urgent: bool) -> bool {
        self.allow_during_playback || urgent
    }
}

/// 翻译选项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranslateOptions {
    /// 源语言（`auto` = 自动检测）。
    pub source_lang: String,
    /// 目标语言（如 `zh` / `zh-TW` / `en` / `ja`）。
    pub target_lang: String,
    /// 术语表（原文 => 译文），翻译时作为强约束注入 prompt。
    pub glossary: Vec<(String, String)>,
}

impl Default for TranslateOptions {
    fn default() -> Self {
        Self {
            source_lang: "auto".into(),
            target_lang: "zh".into(),
            glossary: Vec::new(),
        }
    }
}

impl TranslateOptions {
    /// 快速构造。
    pub fn new(source_lang: impl Into<String>, target_lang: impl Into<String>) -> Self {
        Self {
            source_lang: source_lang.into(),
            target_lang: target_lang.into(),
            glossary: Vec::new(),
        }
    }

    /// 追加术语表并返回自身（链式构造）。
    pub fn with_glossary(mut self, glossary: Vec<(String, String)>) -> Self {
        self.glossary = glossary;
        self
    }

    /// 目标语言的人类可读名（prompt 用；未知语言原样返回）。
    pub fn target_lang_name(&self) -> &str {
        match self.target_lang.as_str() {
            "zh" | "zh-CN" | "zh-Hans" => "简体中文",
            "zh-TW" | "zh-HK" | "zh-Hant" => "繁体中文",
            "en" => "英语",
            "ja" => "日语",
            "ko" => "韩语",
            "fr" => "法语",
            "de" => "德语",
            "es" => "西班牙语",
            "ru" => "俄语",
            other => other,
        }
    }

    /// 源语言的人类可读名（`auto` → 自动检测）。
    pub fn source_lang_name(&self) -> &str {
        match self.source_lang.as_str() {
            "auto" | "" => "自动检测",
            "zh" | "zh-CN" | "zh-Hans" => "简体中文",
            "zh-TW" | "zh-HK" | "zh-Hant" => "繁体中文",
            "en" => "英语",
            "ja" => "日语",
            "ko" => "韩语",
            other => other,
        }
    }
}

/// Provider 抽象：实现者可为远端 HTTP、本地进程、本地库。
///
/// 所有方法都异步；默认实现返回「不支持」，Provider 只需实现自己擅长的能力。
/// 翻译语义：返回与输入 `cues` **等长**的译文列表（空/缺失的条目请用原文填充）。
#[async_trait::async_trait]
pub trait AiProvider: Send + Sync {
    fn id(&self) -> &str;
    fn kind(&self) -> ProviderKind;

    /// 翻译字幕 cue（返回与输入等长的译文）。
    async fn translate(
        &self,
        _cues: &[wp_subtitle::Cue],
        _opts: &TranslateOptions,
    ) -> Result<Vec<String>> {
        Err(Error::unsupported("该 Provider 未实现字幕翻译"))
    }

    /// 音频转写（P2；返回字幕文档）。
    async fn transcribe(&self, _audio: &[u8], _lang: &str) -> Result<wp_subtitle::SubtitleDoc> {
        Err(Error::unsupported("该 Provider 未实现语音转写"))
    }

    /// 从音频文件转写（P2）：默认读文件后调用 [`Self::transcribe`]；
    /// Provider（如 whisper.cpp）可覆盖为直接把路径交给外部进程，避免整段音频进内存。
    async fn transcribe_file(
        &self,
        path: &std::path::Path,
        lang: &str,
    ) -> Result<wp_subtitle::SubtitleDoc> {
        let audio = tokio::fs::read(path).await.map_err(Error::Io)?;
        self.transcribe(&audio, lang).await
    }

    /// 文本摘要/问答（P3）。
    async fn complete(&self, _prompt: &str) -> Result<String> {
        Err(Error::unsupported("该 Provider 未实现文本生成"))
    }
}

/// 播放期间拒绝 AI 任务的错误消息前缀（UI 可据此给出引导文案）。
pub const PLAYBACK_REJECTED_PREFIX: &str = "播放期间不执行 AI 任务";

/// 构造「播放期拒绝」错误（明确提示用户如何放行）。
pub fn playback_rejected(detail: impl AsRef<str>) -> Error {
    Error::other(format!(
        "{PLAYBACK_REJECTED_PREFIX}（{}）：请在暂停后重试，或在 AI 设置中开启 allow_during_playback / 将任务标记为紧急",
        detail.as_ref()
    ))
}

/// 判断错误是否属于「播放期策略拒绝」（区别于 Provider 失败，UI 不应提示重试）。
pub fn is_playback_rejected(err: &Error) -> bool {
    err.to_string().contains(PLAYBACK_REJECTED_PREFIX)
}

/// 健康检查：
/// - 远端 OpenAI 兼容：`GET {base_url}/models`（2xx 即可用）；
/// - 本地 LLM：`GET {base_url}/models`（绕过系统代理；llama-server / vLLM / Ollama 均支持）；
/// - 本地 Whisper：检查 `whisper-cli` 可执行文件 + ggml 模型文件是否存在；
/// - ONNX：未实现，固定 false。
pub async fn health_check(cfg: &ProviderConfig) -> bool {
    match cfg.kind {
        ProviderKind::RemoteOpenAi => remote::health_check(cfg).await,
        ProviderKind::LocalLlm => local_llm_health(cfg).await,
        ProviderKind::LocalWhisper => local_whisper_health(cfg).await,
        ProviderKind::Onnx => false,
    }
}

#[cfg(feature = "local-llm")]
async fn local_llm_health(cfg: &ProviderConfig) -> bool {
    local_llm::health_check(cfg).await
}

#[cfg(not(feature = "local-llm"))]
async fn local_llm_health(_cfg: &ProviderConfig) -> bool {
    false
}

#[cfg(feature = "local-whisper")]
async fn local_whisper_health(cfg: &ProviderConfig) -> bool {
    local_whisper::health_check(cfg).await
}

#[cfg(not(feature = "local-whisper"))]
async fn local_whisper_health(_cfg: &ProviderConfig) -> bool {
    false
}

/// 按优先级选择指定类型中可用的 Provider（T18 契约）：
/// `enabled` 过滤 → `offline_only` 过滤远端 → `priority` 升序（相同优先级取配置中靠前者）。
pub fn select_provider(cfg: &AiConfig, kind: ProviderKind) -> Option<&ProviderConfig> {
    cfg.providers
        .iter()
        .filter(|p| p.enabled && p.kind == kind)
        .filter(|p| !(cfg.offline_only && p.kind.is_remote()))
        .min_by_key(|p| p.priority)
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_local;
