//! AI 子系统骨架：Provider 抽象（本地/远端可分别启停与优先级）、任务队列、缓存。
//!
//! 任务 T18–T21：翻译字幕（P1）→ ASR 生成字幕（P2）→ 摘要/问答（P3）→ OCR/超分（P4）。
//! 设计：重活建议跑在独立 worker 进程（避免影响 4K 解码）；本 crate 先定义接口与纯逻辑。

use serde::{Deserialize, Serialize};
use wp_core::{Error, Result};
use wp_subtitle::{Cue, SubtitleDoc};

/// AI 功能类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AiTaskKind {
    TranslateSubtitles,
    TranslateDanmaku,
    TranscribeAudio,
    Summarize,
    AnswerQuestion,
    OcrSubtitles,
}

/// Provider 类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderKind {
    /// 远端 OpenAI 兼容 API（DeepSeek/OpenAI/Qwen 等）。
    RemoteOpenAi,
    /// 本地 LLM（llama.cpp/candle，feature 控制）。
    LocalLlm,
    /// 本地 Whisper（ASR）。
    LocalWhisper,
    /// ONNX Runtime（OCR/超分等专用模型）。
    Onnx,
}

/// 单个 Provider 配置（可分别启用/停用/调优先级）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub kind: ProviderKind,
    pub enabled: bool,
    /// 数值越小优先级越高（负载/失败时按优先级回退）。
    pub priority: u8,
    pub base_url: String,
    pub model: String,
    /// API Key（空=本地/免密钥）。注意：不打印到日志。
    #[serde(default, skip_serializing)]
    pub api_key: String,
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
        }
    }
}

/// AI 全局配置。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AiConfig {
    pub providers: Vec<ProviderConfig>,
    /// 离线优先（true=只在本地 provider 可用时执行，不自动走远端）。
    pub offline_only: bool,
    /// 播放期间是否允许 AI 重任务（false=只在暂停/空闲跑）。
    pub allow_during_playback: bool,
}

/// 翻译选项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranslateOptions {
    pub source_lang: String,
    pub target_lang: String,
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

/// Provider 抽象：实现者可为远端 HTTP、本地进程、本地库。
#[async_trait::async_trait]
pub trait AiProvider: Send + Sync {
    fn id(&self) -> &str;
    fn kind(&self) -> ProviderKind;

    /// 翻译字幕 cue（返回与输入等长的译文）。
    async fn translate(&self, _cues: &[Cue], _opts: &TranslateOptions) -> Result<Vec<String>> {
        Err(Error::unsupported("该 Provider 未实现字幕翻译"))
    }

    /// 音频转写（P2；返回字幕文档）。
    async fn transcribe(&self, _audio: &[u8], _lang: &str) -> Result<SubtitleDoc> {
        Err(Error::unsupported("该 Provider 未实现语音转写"))
    }

    /// 文本摘要/问答（P3）。
    async fn complete(&self, _prompt: &str) -> Result<String> {
        Err(Error::unsupported("该 Provider 未实现文本生成"))
    }
}

/// 简单健康检查（远端 provider 用；本地 provider 可返回 true）。
pub async fn health_check(_cfg: &ProviderConfig) -> bool {
    false
}

/// 按优先级选择可用 provider（T18 实现）。
pub fn select_provider(_cfg: &AiConfig, _kind: ProviderKind) -> Option<&ProviderConfig> {
    None
}
