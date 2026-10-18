//! Provider 注册、选择与失败回退。
//!
//! 候选选择规则（与 [`AiConfig::sorted_candidates`] 一致）：
//! 1. `enabled == true`；
//! 2. `offline_only == true` 时排除远端 Provider（隐私优先，即使它已启用）；
//! 3. 能力矩阵 [`AiTaskKind::supports_provider`] 匹配该任务；
//! 4. 已注册实例（[`build_provider_registry`] 会为启用的 Provider 自动构造实例）；
//! 5. 按 `priority` 升序（数值小优先），同级保持配置顺序。
//!
//! 回退语义：依次尝试候选，任何错误（含鉴权失败）都继续尝试下一个；用户取消
//! （[`wp_core::Error::is_canceled`]）不触发回退。全部失败返回聚合错误（**不含 api_key**），
//! 调用方（`pipeline`）负责降级为原文并标记 error。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use wp_core::{Error, Result};
use wp_subtitle::{Cue, SubtitleDoc};

use crate::{AiConfig, AiProvider, AiTaskKind, ProviderConfig, ProviderKind, TranslateOptions};

/// 已注册的 Provider 实例集合（配置 + 实例）。
#[derive(Default)]
pub struct ProviderRegistry {
    ai: AiConfig,
    providers: HashMap<String, Arc<dyn AiProvider>>,
}

impl ProviderRegistry {
    /// 用配置构造空注册表（尚无实例；需调用 [`Self::register`] 或走 [`build_provider_registry`]）。
    pub fn from_config(ai: &AiConfig) -> Self {
        Self {
            ai: ai.clone(),
            providers: HashMap::new(),
        }
    }

    /// 注册一个 Provider 实例。`id` 必须与配置里的 [`ProviderConfig::id`] 一致才会被选中。
    pub fn register(&mut self, provider: Arc<dyn AiProvider>) {
        self.providers.insert(provider.id().to_string(), provider);
    }

    /// 当前配置。
    pub fn ai_config(&self) -> &AiConfig {
        &self.ai
    }

    /// 当前配置中的全部 Provider（含未启用）。
    pub fn configs(&self) -> &[ProviderConfig] {
        &self.ai.providers
    }

    /// 已注册实例。
    pub fn provider(&self, id: &str) -> Option<&Arc<dyn AiProvider>> {
        self.providers.get(id)
    }

    pub fn is_registered(&self, id: &str) -> bool {
        self.providers.contains_key(id)
    }

    pub fn registered_count(&self) -> usize {
        self.providers.len()
    }

    /// 某任务的可用候选（过滤 + 排序 + 实例存在），返回 `(配置, 实例)` 列表。
    pub fn candidates(&self, task: AiTaskKind) -> Vec<(&ProviderConfig, Arc<dyn AiProvider>)> {
        let mut list: Vec<(&ProviderConfig, Arc<dyn AiProvider>)> = self
            .ai
            .providers
            .iter()
            .filter(|p| p.enabled)
            .filter(|p| !(self.ai.offline_only && p.kind.is_remote()))
            .filter(|p| task.supports_provider(p.kind))
            .filter_map(|p| self.providers.get(&p.id).map(|inst| (p, inst.clone())))
            .collect();
        list.sort_by_key(|(p, _)| p.priority);
        list
    }

    /// 某任务的能力匹配候选配置（不要求实例存在；UI 提示用）。
    pub fn candidate_configs(&self, task: AiTaskKind) -> Vec<&ProviderConfig> {
        let mut list: Vec<&ProviderConfig> = self
            .ai
            .providers
            .iter()
            .filter(|p| p.enabled)
            .filter(|p| !(self.ai.offline_only && p.kind.is_remote()))
            .filter(|p| task.supports_provider(p.kind))
            .collect();
        list.sort_by_key(|p| p.priority);
        list
    }
}

/// Provider 在 UI（AI 面板）里的状态快照（不含密钥）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderStatus {
    pub id: String,
    pub kind: ProviderKind,
    pub enabled: bool,
    pub priority: u8,
    pub model: String,
    pub is_local: bool,
    /// 是否已成功构造实例。
    pub registered: bool,
    /// 当前配置下是否会被用于请求（enabled + 已注册 + offline_only 允许）。
    pub usable: bool,
}

impl ProviderStatus {
    /// UI 用单行描述（不含密钥）。
    pub fn label(&self) -> String {
        format!(
            "{}{}（{}，优先级 {}）",
            self.id,
            if self.enabled { "" } else { " [已停用]" },
            self.kind.label(),
            self.priority
        )
    }
}

/// 汇总 AI 面板状态。
pub fn provider_statuses(registry: &ProviderRegistry) -> Vec<ProviderStatus> {
    let ai = registry.ai_config();
    let mut list: Vec<ProviderStatus> = ai
        .providers
        .iter()
        .map(|p| {
            let registered = registry.is_registered(&p.id);
            ProviderStatus {
                id: p.id.clone(),
                kind: p.kind,
                enabled: p.enabled,
                priority: p.priority,
                model: p.model.clone(),
                is_local: p.kind.is_local(),
                registered,
                usable: p.enabled && registered && !(ai.offline_only && p.kind.is_remote()),
            }
        })
        .collect();
    list.sort_by_key(|p| p.priority);
    list
}

/// 按配置构造 Provider 实例。
///
/// 本地 Provider 需要对应 feature（`local-llm` / `local-whisper`）；未编译时返回
/// [`wp_core::ErrorKind::Unsupported`]，由 [`build_provider_registry`] 收集成警告。
pub fn build_provider(cfg: &ProviderConfig) -> Result<Arc<dyn AiProvider>> {
    match cfg.kind {
        ProviderKind::RemoteOpenAi => Ok(Arc::new(
            crate::remote::RemoteOpenAiProvider::from_config(cfg)?,
        )),
        ProviderKind::LocalLlm => build_local_llm(cfg),
        ProviderKind::LocalWhisper => build_local_whisper(cfg),
        ProviderKind::Onnx => Err(Error::unsupported(format!(
            "Provider {} 是 ONNX 本地模型，当前版本尚未实现（P4 规划）",
            cfg.id
        ))),
    }
}

#[cfg(feature = "local-llm")]
fn build_local_llm(cfg: &ProviderConfig) -> Result<Arc<dyn AiProvider>> {
    Ok(Arc::new(crate::local_llm::LocalLlmProvider::new(
        cfg.clone(),
    )?))
}

#[cfg(not(feature = "local-llm"))]
fn build_local_llm(cfg: &ProviderConfig) -> Result<Arc<dyn AiProvider>> {
    Err(Error::unsupported(format!(
        "Provider {} 是本地 LLM，但本构建未编译 feature local-llm",
        cfg.id
    )))
}

#[cfg(feature = "local-whisper")]
fn build_local_whisper(cfg: &ProviderConfig) -> Result<Arc<dyn AiProvider>> {
    Ok(Arc::new(crate::local_whisper::LocalWhisperProvider::new(
        cfg.clone(),
    )?))
}

#[cfg(not(feature = "local-whisper"))]
fn build_local_whisper(cfg: &ProviderConfig) -> Result<Arc<dyn AiProvider>> {
    Err(Error::unsupported(format!(
        "Provider {} 是本地 Whisper，但本构建未编译 feature local-whisper",
        cfg.id
    )))
}

/// 根据 [`AiConfig`] 构造注册表：为每个**已启用**的 Provider 构造实例。
///
/// 返回 `(registry, warnings)`；`warnings` 是中文可读提示（本地 feature 未编译、
/// 远端缺 model/base_url 等），UI 可直接展示。未启用的 Provider 不构造实例。
pub fn build_provider_registry(ai: &AiConfig) -> (ProviderRegistry, Vec<String>) {
    let mut registry = ProviderRegistry::from_config(ai);
    let mut warnings = Vec::new();
    for cfg in ai.providers.iter().filter(|p| p.enabled) {
        if cfg.kind.is_remote() {
            if cfg.model.trim().is_empty() {
                warnings.push(format!("{}：远端 Provider 未设置 model", cfg.id));
            }
            if cfg.base_url.trim().is_empty() {
                warnings.push(format!("{}：远端 Provider 未设置 base_url", cfg.id));
            }
        }
        match build_provider(cfg) {
            Ok(provider) => registry.register(provider),
            Err(e) => warnings.push(format!("{}：{e}", cfg.id)),
        }
    }
    (registry, warnings)
}

/// 构造「无可用 Provider」的可读错误（含排查提示）。
fn no_provider_error(registry: &ProviderRegistry, task: AiTaskKind) -> Error {
    let ai = registry.ai_config();
    let mut hints: Vec<&str> = Vec::new();
    if !ai.providers.iter().any(|p| p.enabled) {
        hints.push("没有任何已启用的 Provider");
    }
    if ai.offline_only && ai.providers.iter().any(|p| p.enabled && p.kind.is_remote()) {
        hints.push("offline_only 已开启，远端 Provider 被排除");
    }
    if !ai
        .providers
        .iter()
        .any(|p| p.enabled && task.supports_provider(p.kind))
    {
        hints.push("没有能力匹配的 Provider 类型");
    }
    if registry.registered_count() == 0 {
        hints.push("没有成功构造的 Provider 实例");
    }
    Error::other(format!(
        "没有可用的 AI Provider（任务：{}；已注册实例 {}）{}",
        task.label(),
        registry.registered_count(),
        if hints.is_empty() {
            String::new()
        } else {
            format!("：{}", hints.join("；"))
        }
    ))
}

/// 字幕翻译 + 失败回退：按优先级依次尝试，返回 `(与输入等长的译文, 实际使用的 provider id)`。
pub async fn translate_with_fallback(
    registry: &ProviderRegistry,
    task: AiTaskKind,
    cues: &[Cue],
    opts: &TranslateOptions,
) -> Result<(Vec<String>, String)> {
    if cues.is_empty() {
        return Ok((Vec::new(), String::new()));
    }
    let candidates = registry.candidates(task);
    if candidates.is_empty() {
        return Err(no_provider_error(registry, task));
    }
    let total = candidates.len();
    let mut errors: Vec<String> = Vec::new();
    for (cfg, provider) in candidates {
        match provider.translate(cues, opts).await {
            Ok(translations) if translations.len() == cues.len() => {
                return Ok((translations, cfg.id.clone()));
            }
            Ok(translations) => {
                errors.push(format!(
                    "{}：返回 {} 条译文，与输入 {} 条不一致",
                    cfg.id,
                    translations.len(),
                    cues.len()
                ));
            }
            Err(e) if e.is_canceled() => return Err(e),
            Err(e) => errors.push(format!("{}：{e}", cfg.id)),
        }
    }
    Err(Error::other(format!(
        "全部 {total} 个 AI Provider 执行失败：{}",
        errors.join("；")
    )))
}

/// 文本生成（摘要/问答）+ 失败回退。
pub async fn complete_with_fallback(
    registry: &ProviderRegistry,
    prompt: &str,
) -> Result<(String, String)> {
    let task = AiTaskKind::Summarize;
    let candidates = registry.candidates(task);
    if candidates.is_empty() {
        return Err(no_provider_error(registry, task));
    }
    let mut errors: Vec<String> = Vec::new();
    for (cfg, provider) in candidates {
        match provider.complete(prompt).await {
            Ok(text) => return Ok((text, cfg.id.clone())),
            Err(e) if e.is_canceled() => return Err(e),
            Err(e) => errors.push(format!("{}：{e}", cfg.id)),
        }
    }
    Err(Error::other(format!(
        "全部 AI Provider 文本生成失败：{}",
        errors.join("；")
    )))
}

/// 音频转写（P2）+ 失败回退。
pub async fn transcribe_with_fallback(
    registry: &ProviderRegistry,
    audio: &[u8],
    lang: &str,
) -> Result<(SubtitleDoc, String)> {
    let task = AiTaskKind::TranscribeAudio;
    let candidates = registry.candidates(task);
    if candidates.is_empty() {
        return Err(no_provider_error(registry, task));
    }
    let mut errors: Vec<String> = Vec::new();
    for (cfg, provider) in candidates {
        match provider.transcribe(audio, lang).await {
            Ok(doc) => return Ok((doc, cfg.id.clone())),
            Err(e) if e.is_canceled() => return Err(e),
            Err(e) => errors.push(format!("{}：{e}", cfg.id)),
        }
    }
    Err(Error::other(format!(
        "全部 AI Provider 转写失败：{}",
        errors.join("；")
    )))
}

/// 音频**文件**转写（P2）+ 失败回退。
///
/// 与 [`transcribe_with_fallback`] 的区别：走 [`AiProvider::transcribe_file`]，
/// whisper.cpp Provider 会直接把路径交给 `whisper-cli`，避免整段音频读进内存。
pub async fn transcribe_file_with_fallback(
    registry: &ProviderRegistry,
    path: &Path,
    lang: &str,
) -> Result<(SubtitleDoc, String)> {
    let task = AiTaskKind::TranscribeAudio;
    let candidates = registry.candidates(task);
    if candidates.is_empty() {
        return Err(no_provider_error(registry, task));
    }
    let mut errors: Vec<String> = Vec::new();
    for (cfg, provider) in candidates {
        match provider.transcribe_file(path, lang).await {
            Ok(doc) => return Ok((doc, cfg.id.clone())),
            Err(e) if e.is_canceled() => return Err(e),
            Err(e) => errors.push(format!("{}：{e}", cfg.id)),
        }
    }
    Err(Error::other(format!(
        "全部 AI Provider 文件转写失败：{}",
        errors.join("；")
    )))
}
