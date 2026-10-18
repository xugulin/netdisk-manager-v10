//! 字幕翻译管线（P1 核心）：`SubtitleDoc` → 分批翻译 → 译文列表 / 译文 `SubtitleDoc`。
//!
//! 分层：
//! - Provider 层负责「一次 API 调用 + 分批 + prompt/解析」（见 [`crate::remote`]）；
//! - 本模块负责「整片编排」：缓存查询/回写、优先级回退（[`crate::provider::translate_with_fallback`]）、
//!   **失败降级为原文并标记 error**、生成保留时间轴的译文 [`SubtitleDoc`]。
//!
//! 失败降级约定：任何 Provider 全部失败时**不返回错误**，而是返回
//! `translations = 原文`、`degraded = true`、`errors = [失败原因]`，
//! GUI 可提示「AI 翻译失败，已保留原字幕」；调用方可按需改用双语 ASS 的纯原文模式。
//!
//! 生成双语 ASS 由字幕专家（`wp-subtitle`）负责：用 [`build_translated_doc`] 得到译文 cue，
//! 再调用 `wp_subtitle::build_bilingual_ass(original, translated, opts)`。
//!
//! ASR 部分（P2）：[`transcribe_to_subtitle`]（音频字节）/ [`transcribe_file_to_subtitle`]（文件路径）
//! 走 whisper.cpp 等 Provider，带缓存/优先级回退；失败降级为空 `SubtitleDoc` 并标记 error。

use std::path::Path;

use wp_subtitle::{Cue, SubtitleDoc};

use crate::cache::{AiCache, CacheKey};
use crate::provider::{
    transcribe_file_with_fallback, transcribe_with_fallback, translate_with_fallback,
};
use crate::{AiTaskKind, ProviderRegistry, TranslateOptions};

/// 整片字幕翻译结果（永不因 Provider 失败而报错，失败降级原文）。
#[derive(Debug, Clone)]
pub struct TranslateOutcome {
    /// 与 `doc.cues` 等长的译文（降级时=原文）。
    pub translations: Vec<String>,
    /// 实际使用的 Provider id（命中缓存时为 None）。
    pub provider_id: Option<String>,
    /// 是否命中缓存（零耗时路径）。
    pub cached: bool,
    /// 是否有降级（至少一条/全部为原文）。
    pub degraded: bool,
    /// 失败/警告详情（中文可读，不含密钥）。
    pub errors: Vec<String>,
}

impl TranslateOutcome {
    /// 是否完整成功（无降级、无警告）。
    pub fn is_complete(&self) -> bool {
        !self.degraded && self.errors.is_empty()
    }

    /// 是否需要提示用户（降级或缓存写入失败等警告）。
    pub fn has_warning(&self) -> bool {
        self.degraded || !self.errors.is_empty()
    }
}

/// 把翻译结果写回 cue，生成**译文 SubtitleDoc**（时间轴/原始样式/ASS 头完整保留）。
///
/// 空译文条目自动回退原文；`translations` 短于 cues 时多出的 cue 保留原文。
pub fn build_translated_doc(doc: &SubtitleDoc, translations: &[String]) -> SubtitleDoc {
    let mut out = doc.clone();
    for (i, cue) in out.cues.iter_mut().enumerate() {
        if let Some(text) = translations.get(i) {
            let text = text.trim();
            if !text.is_empty() {
                cue.text = text.to_string();
            }
        }
    }
    out
}

/// 整片字幕翻译（带缓存 + 优先级回退 + 失败降级）。
///
/// - `fingerprint`：文件指纹（用 [`crate::cache::fingerprint_file_metadata`] 生成）；
///   为空字符串时**跳过缓存**（例如字幕来源无法稳定指纹）。
/// - 缓存键 =（指纹, 当前最高优先级候选的 model, `opts.target_lang`, `TranslateSubtitles`）。
/// - 命中缓存直接返回，不触发任何网络/Provider 调用。
pub async fn translate_subtitle_document(
    registry: &ProviderRegistry,
    cache: &AiCache,
    fingerprint: &str,
    doc: &SubtitleDoc,
    opts: &TranslateOptions,
) -> TranslateOutcome {
    let cues: &[Cue] = &doc.cues;
    if cues.is_empty() {
        return TranslateOutcome {
            translations: Vec::new(),
            provider_id: None,
            cached: false,
            degraded: false,
            errors: Vec::new(),
        };
    }
    let task = AiTaskKind::TranslateSubtitles;
    let model = registry
        .candidate_configs(task)
        .first()
        .map(|p| p.model.clone())
        .unwrap_or_default();
    let use_cache = !fingerprint.trim().is_empty();
    let key = CacheKey::new(fingerprint, model, opts.target_lang.clone(), task);
    if use_cache {
        if let Some(translations) = cache.get_translations(&key) {
            if translations.len() == cues.len() {
                return TranslateOutcome {
                    translations,
                    provider_id: None,
                    cached: true,
                    degraded: false,
                    errors: Vec::new(),
                };
            }
        }
    }
    match translate_with_fallback(registry, task, cues, opts).await {
        Ok((translations, provider_id)) => {
            let mut errors = Vec::new();
            if use_cache {
                if let Err(e) = cache.put_translations(&key, &translations) {
                    errors.push(format!("翻译成功但缓存写入失败：{e}"));
                }
            }
            TranslateOutcome {
                translations,
                provider_id: Some(provider_id),
                cached: false,
                degraded: false,
                errors,
            }
        }
        Err(e) => {
            // 失败降级：返回原文，标记 error（UI 可提示，不中断播放）。
            TranslateOutcome {
                translations: cues.iter().map(|c| c.text.clone()).collect(),
                provider_id: None,
                cached: false,
                degraded: true,
                errors: vec![format!("AI 翻译失败，已保留原字幕：{e}")],
            }
        }
    }
}

/// 直接翻译 cue 列表（不写缓存；实时前瞻/弹幕翻译用）。
pub async fn translate_cues(
    registry: &ProviderRegistry,
    task: AiTaskKind,
    cues: &[Cue],
    opts: &TranslateOptions,
) -> crate::Result<Vec<String>> {
    let (translations, _provider) = translate_with_fallback(registry, task, cues, opts).await?;
    Ok(translations)
}

/// ASR（语音转写）整片结果（Provider 失败时降级为空文档 + errors，不返回 Err）。
#[derive(Debug, Clone)]
pub struct TranscribeOutcome {
    /// 转写字幕文档（降级时为空文档）。
    pub doc: SubtitleDoc,
    /// 实际使用的 Provider id（命中缓存时为 None）。
    pub provider_id: Option<String>,
    /// 是否命中缓存。
    pub cached: bool,
    /// 是否降级（Provider 不可用/失败）。
    pub degraded: bool,
    /// 失败/警告详情（中文可读，不含密钥）。
    pub errors: Vec<String>,
}

impl TranscribeOutcome {
    /// 是否完整成功（无降级、无警告）。
    pub fn is_complete(&self) -> bool {
        !self.degraded && self.errors.is_empty()
    }

    /// 是否需要提示用户。
    pub fn has_warning(&self) -> bool {
        self.degraded || !self.errors.is_empty()
    }

    /// 字幕条数。
    pub fn cue_count(&self) -> usize {
        self.doc.cues.len()
    }
}

fn transcribe_cache_key(
    registry: &ProviderRegistry,
    fingerprint: &str,
    lang: &str,
) -> Option<CacheKey> {
    if fingerprint.trim().is_empty() {
        return None;
    }
    let model = registry
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

fn degraded_transcribe(errors: Vec<String>) -> TranscribeOutcome {
    TranscribeOutcome {
        doc: SubtitleDoc::default(),
        provider_id: None,
        cached: false,
        degraded: true,
        errors,
    }
}

fn finish_transcribe(
    cache: &AiCache,
    cache_key: Option<&CacheKey>,
    doc: SubtitleDoc,
    provider_id: String,
) -> TranscribeOutcome {
    let mut errors = Vec::new();
    if let Some(key) = cache_key {
        if let Err(e) = cache.put_subtitle_doc(key, &doc) {
            errors.push(format!("转写成功但缓存写入失败：{e}"));
        }
    }
    TranscribeOutcome {
        doc,
        provider_id: Some(provider_id),
        cached: false,
        degraded: false,
        errors,
    }
}

/// 从**音频字节**转写为字幕（P2，带缓存/回退/失败降级）。
///
/// - `fingerprint`：音频/视频文件指纹（空=不缓存）；
/// - 缓存键 =（指纹, 候选模型, `lang`, `TranscribeAudio`）；
/// - 字节输入会由本地 Provider 写临时文件（whisper-cli 只吃文件）。
pub async fn transcribe_to_subtitle(
    registry: &ProviderRegistry,
    cache: &AiCache,
    fingerprint: &str,
    audio: &[u8],
    lang: &str,
) -> TranscribeOutcome {
    if audio.is_empty() {
        return degraded_transcribe(vec!["音频为空，无法转写".to_string()]);
    }
    let cache_key = transcribe_cache_key(registry, fingerprint, lang);
    if let Some(key) = &cache_key {
        if let Some(doc) = cache.get_subtitle_doc(key) {
            return TranscribeOutcome {
                doc,
                provider_id: None,
                cached: true,
                degraded: false,
                errors: Vec::new(),
            };
        }
    }
    match transcribe_with_fallback(registry, audio, lang).await {
        Ok((doc, provider_id)) => finish_transcribe(cache, cache_key.as_ref(), doc, provider_id),
        Err(e) => degraded_transcribe(vec![format!("AI 转写失败，未生成字幕：{e}")]),
    }
}

/// 从**音频文件**转写为字幕（P2，推荐：whisper-cli 直接读路径，不复制音频内容）。
pub async fn transcribe_file_to_subtitle(
    registry: &ProviderRegistry,
    cache: &AiCache,
    fingerprint: &str,
    path: &Path,
    lang: &str,
) -> TranscribeOutcome {
    if path.as_os_str().is_empty() {
        return degraded_transcribe(vec!["音频文件路径为空".to_string()]);
    }
    if !path.is_file() {
        return degraded_transcribe(vec![format!("音频文件不存在：{}", path.display())]);
    }
    let cache_key = transcribe_cache_key(registry, fingerprint, lang);
    if let Some(key) = &cache_key {
        if let Some(doc) = cache.get_subtitle_doc(key) {
            return TranscribeOutcome {
                doc,
                provider_id: None,
                cached: true,
                degraded: false,
                errors: Vec::new(),
            };
        }
    }
    match transcribe_file_with_fallback(registry, path, lang).await {
        Ok((doc, provider_id)) => finish_transcribe(cache, cache_key.as_ref(), doc, provider_id),
        Err(e) => degraded_transcribe(vec![format!("AI 转写失败，未生成字幕：{e}")]),
    }
}
