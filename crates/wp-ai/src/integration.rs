//! AI × 字幕/播放器集成闭环（T11）：翻译 → 双语 ASS → 落盘；ffmpeg 抽音轨（ASR 前置）。
//!
//! 这是给 GUI/播放器用的「一个调用拿到成品字幕文件」层，底层组合：
//! [`crate::pipeline::translate_subtitle_document`]（Provider 回退 + 缓存 + 失败降级）
//! + [`wp_subtitle`]（保留原 ASS 头/样式、双轨布局、漏译统计）
//! + 本模块（原子写文件、ffmpeg 外部进程）。
//!
//! # 快速使用（GUI 翻译按钮 → 拿到可 `sub-add` 的 ASS）
//!
//! ```no_run
//! use wp_ai::cache::AiCache;
//! use wp_ai::integration::{translate_doc_and_write_ass, BilingualResult};
//! use wp_ai::provider::build_provider_registry;
//! use wp_ai::{AiConfig, TranslateOptions};
//! use wp_subtitle::{AssOptions, SubtitleDoc};
//!
//! # async fn demo(ai: AiConfig, doc: SubtitleDoc, cache: AiCache) -> wp_core::Result<()> {
//! let (registry, _warnings) = build_provider_registry(&ai);
//! let result = translate_doc_and_write_ass(
//!     &registry,
//!     &cache,
//!     "文件指纹",                       // 用于结果缓存（空=不缓存）
//!     &doc,
//!     &TranslateOptions::default(),     // 源/目标语言 + 术语表
//!     &AssOptions::default(),           // 字体/字号/双轨布局
//!     std::path::Path::new("/tmp/bilingual.ass"),
//! )
//! .await?;
//! println!(
//!     "匹配 {} 条 / 漏译 {} 条，ASS：{}",
//!     result.matched,
//!     result.missing,
//!     result.path.display()
//! );
//! # Ok(())
//! # }
//! ```
//!
//! # ASR 前置（视频 → 16kHz 单声道 wav）
//!
//! ```no_run
//! # async fn demo(video: &std::path::Path) -> wp_core::Result<()> {
//! use wp_ai::integration::{cleanup_temp_audio, extract_audio_for_asr};
//!
//! let wav = extract_audio_for_asr(video).await?; // 调外部 ffmpeg：-vn -ac 1 -ar 16000 -c:a pcm_s16le
//! // ……交给 wp_ai::pipeline::transcribe_file_to_subtitle(&registry, &cache, fp, &wav, "zh").await……
//! cleanup_temp_audio(&wav); // 用完删除临时音频
//! # Ok(())
//! # }
//! ```
//!
//! # 失败语义
//! - **翻译失败**（全部 Provider 失败）：不返回 Err，而是降级为「仅原文」ASS，
//!   [`BilingualResult::degraded`] = true、`matched = 0`、`missing = 全部`、`errors` 带原因；
//! - **写文件失败 / ffmpeg 失败**：返回 `Err`（调用方无法继续，需要提示/重试）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use wp_core::{Error, Result};
use wp_subtitle::{AssOptions, BilingualAss, Cue, SubtitleDoc};

use crate::cache::AiCache;
use crate::pipeline::translate_subtitle_document;
use crate::provider::ProviderRegistry;
use crate::TranslateOptions;

// ---------------------------------------------------------------------------
// 翻译 → 双语 ASS
// ---------------------------------------------------------------------------

/// 双语 ASS 写入结果（GUI 展示用；字段不含密钥/隐私内容）。
#[derive(Debug, Clone)]
pub struct BilingualResult {
    /// 实际写入的 ASS 文件路径。
    pub path: PathBuf,
    /// 原字幕总条数。
    pub total: usize,
    /// 成功配上译文的条数。
    pub matched: usize,
    /// 漏译条数（只输出原文行）。
    pub missing: usize,
    /// 实际使用的 Provider id（缓存命中/降级时为 None）。
    pub provider_id: Option<String>,
    /// 是否命中翻译缓存（二次播放零耗时）。
    pub cached: bool,
    /// 是否降级（翻译失败 → 仅原文 ASS）。
    pub degraded: bool,
    /// 错误/警告（中文可读，不含密钥）。
    pub errors: Vec<String>,
    /// ASS 文本（方便调试/直接交给 mpv，无需再读文件）。
    pub content: String,
}

impl BilingualResult {
    /// 是否完整成功（无降级、无漏译、无警告）。
    pub fn is_complete(&self) -> bool {
        !self.degraded && self.missing == 0 && self.errors.is_empty()
    }

    /// 是否有需要提示用户的情况（降级/漏译/缓存写失败等）。
    pub fn has_warning(&self) -> bool {
        self.degraded || self.missing > 0 || !self.errors.is_empty()
    }

    /// 双语轨覆盖率（0.0~1.0；空字幕返回 1.0）。
    pub fn coverage(&self) -> f32 {
        if self.total == 0 {
            1.0
        } else {
            self.matched as f32 / self.total as f32
        }
    }
}

/// 构造译文 cue 列表（与原文 cue 等长，供 wp-subtitle 双轨对齐）。
///
/// 规则：
/// - `degraded` = true → 全部空文本（只输出原文轨）；
/// - 译文为空 / 与原文相同 → 空文本（计为漏译，避免双语行重复原文）；
/// - 其余保留译文，时间轴沿用原文。
fn translated_cue_list(doc: &SubtitleDoc, translations: &[String], degraded: bool) -> Vec<Cue> {
    doc.cues
        .iter()
        .enumerate()
        .map(|(i, cue)| {
            let text = if degraded {
                String::new()
            } else {
                translations
                    .get(i)
                    .map(|t| t.trim())
                    .filter(|t| !t.is_empty() && *t != cue.text.trim())
                    .unwrap_or("")
                    .to_string()
            };
            Cue {
                start_ms: cue.start_ms,
                end_ms: cue.end_ms,
                text,
                raw_style: None,
            }
        })
        .collect()
}

/// 生成双语 ASS 报告（保留原 ASS 头/样式；含 matched/missing 统计）。
///
/// 与 [`translate_to_bilingual_ass`] 相同，但返回 [`BilingualAss`]（content/matched/missing）。
pub fn translate_to_bilingual_ass_report(
    doc: &SubtitleDoc,
    translated: &[String],
    style: &AssOptions,
) -> BilingualAss {
    let cues = translated_cue_list(doc, translated, false);
    wp_subtitle::build_bilingual_ass_from_doc(doc, &cues, style)
}

/// 翻译结果 → 双语 ASS 文本（保留原 ASS 头/样式；漏译的 cue 只输出原文）。
///
/// `translated` 应来自 [`crate::pipeline`] 的等长译文列表（缺失/空/同原文视为漏译）。
pub fn translate_to_bilingual_ass(
    doc: &SubtitleDoc,
    translated: &[String],
    style: &AssOptions,
) -> Result<String> {
    Ok(translate_to_bilingual_ass_report(doc, translated, style).content)
}

/// 整片字幕：翻译（带缓存/回退/降级）→ 生成双语 ASS → **原子写入** `out_path`。
///
/// - `fingerprint`：视频/字幕文件指纹（空=不缓存）；
/// - 输出目录不存在会自动创建；
/// - 翻译失败不报错而是降级仅原文 ASS（见模块文档「失败语义」）。
#[allow(clippy::too_many_arguments)]
pub async fn translate_doc_and_write_ass(
    registry: &ProviderRegistry,
    cache: &AiCache,
    fingerprint: &str,
    doc: &SubtitleDoc,
    opts: &TranslateOptions,
    style: &AssOptions,
    out_path: &Path,
) -> Result<BilingualResult> {
    let outcome = translate_subtitle_document(registry, cache, fingerprint, doc, opts).await;
    let translated_cues = translated_cue_list(doc, &outcome.translations, outcome.degraded);
    let report = wp_subtitle::build_bilingual_ass_from_doc(doc, &translated_cues, style);
    write_ass_atomic(out_path, &report.content)?;
    Ok(BilingualResult {
        path: out_path.to_path_buf(),
        total: doc.cues.len(),
        matched: report.matched,
        missing: report.missing,
        provider_id: outcome.provider_id,
        cached: outcome.cached,
        degraded: outcome.degraded,
        errors: outcome.errors,
        content: report.content,
    })
}

/// 写 ASS 文本到文件（同目录临时文件 + rename，避免播放器读到半截文件）。
fn write_ass_atomic(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("subtitle.ass");
    let tmp = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        unix_nanos()
    ));
    std::fs::write(&tmp, content.as_bytes())?;
    if path.exists() {
        // Windows 上 rename 不能覆盖已存在文件；先删旧（原子性由 Linux rename 保证）。
        let _ = std::fs::remove_file(path);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Io(e));
    }
    Ok(())
}

fn unix_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// ffmpeg 抽音轨（ASR 前置）
// ---------------------------------------------------------------------------

/// 指定 ffmpeg 可执行文件的环境变量。
pub const ENV_FFMPEG: &str = "WP_FFMPEG";

/// ffmpeg 抽音轨参数（默认值可用；测试/进阶部署可自定义）。
#[derive(Debug, Clone)]
pub struct FfmpegOptions {
    /// ffmpeg 可执行文件路径或命令名（默认 `WP_FFMPEG` 环境变量 → `ffmpeg`）。
    pub cli_path: String,
    /// 单次抽轨超时（到点 kill 子进程；整片长视频建议 ≥600s）。
    pub timeout: Duration,
    /// 追加到 `-i` 之前的输入参数（如 `-ss 60 -t 30` 抽片段、`-f mp4` 等）。
    pub extra_args: Vec<String>,
}

impl Default for FfmpegOptions {
    fn default() -> Self {
        Self {
            cli_path: std::env::var(ENV_FFMPEG)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "ffmpeg".to_string()),
            timeout: Duration::from_secs(600),
            extra_args: Vec::new(),
        }
    }
}

impl FfmpegOptions {
    /// 指定可执行文件路径。
    pub fn with_cli(mut self, cli_path: impl Into<String>) -> Self {
        self.cli_path = cli_path.into();
        self
    }

    /// 指定超时。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 追加输入参数（放在 `-i` 之前）。
    pub fn with_extra_args(mut self, extra_args: Vec<String>) -> Self {
        self.extra_args = extra_args;
        self
    }
}

/// 从视频/音频文件抽取 **16kHz 单声道 PCM wav**（whisper.cpp 前置），默认参数。
///
/// 等价命令：`ffmpeg -hide_banner -nostdin -y -i <input> -vn -ac 1 -ar 16000 -c:a pcm_s16le <out_wav>`。
pub async fn extract_audio_ffmpeg(input: &Path, out_wav: &Path) -> Result<()> {
    extract_audio_ffmpeg_with(&FfmpegOptions::default(), input, out_wav).await
}

/// 带自定义参数/超时的抽音轨（测试用伪造 ffmpeg 脚本也走这里）。
pub async fn extract_audio_ffmpeg_with(
    options: &FfmpegOptions,
    input: &Path,
    out_wav: &Path,
) -> Result<()> {
    if !input.is_file() {
        return Err(Error::not_found(format!(
            "输入媒体文件不存在：{}（请先用本地/网盘流打开或下载）",
            input.display()
        )));
    }
    let parent = out_wav
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let mut command = tokio::process::Command::new(&options.cli_path);
    command.arg("-hide_banner").arg("-nostdin").arg("-y");
    for extra in &options.extra_args {
        command.arg(extra);
    }
    command
        .arg("-i")
        .arg(input)
        .arg("-vn")
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg("16000")
        .arg("-c:a")
        .arg("pcm_s16le")
        .arg(out_wav)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = command
        .spawn()
        .map_err(|e| map_ffmpeg_spawn_error(&options.cli_path, &e))?;
    let output = match tokio::time::timeout(options.timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return Err(Error::other(format!(
                "ffmpeg 运行失败（{}）：{e}",
                options.cli_path
            )))
        }
        Err(_) => {
            // 超时后 future 被丢弃 → Child 被 drop → kill_on_drop 终止 ffmpeg。
            return Err(Error::network(format!(
                "ffmpeg 抽音轨超时（{}s），已终止子进程：{}",
                options.timeout.as_secs().max(1),
                input.display()
            )));
        }
    };
    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        let stderr = truncate_chars(&String::from_utf8_lossy(&output.stderr), 400);
        return Err(Error::other(format!(
            "ffmpeg 抽音轨失败（退出码 {code}）{}",
            if stderr.is_empty() {
                String::new()
            } else {
                format!("：{stderr}")
            }
        )));
    }
    if !out_wav.is_file() {
        return Err(Error::other(format!(
            "ffmpeg 报告成功但未生成音频文件：{}",
            out_wav.display()
        )));
    }
    let size = std::fs::metadata(out_wav).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return Err(Error::other(format!(
            "ffmpeg 生成的音频文件为空：{}（可能输入没有音轨）",
            out_wav.display()
        )));
    }
    Ok(())
}

fn map_ffmpeg_spawn_error(cli: &str, err: &std::io::Error) -> Error {
    if err.kind() == std::io::ErrorKind::NotFound {
        Error::not_found(format!(
            "找不到 ffmpeg：{cli}（请安装 ffmpeg 或设置环境变量 {ENV_FFMPEG}/FfmpegOptions.cli_path）"
        ))
    } else {
        Error::other(format!("启动 ffmpeg 失败（{cli}）：{err}"))
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

static ASR_AUDIO_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 为 ASR 生成一个唯一临时 wav 路径（不创建文件）。
pub fn default_asr_audio_path(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("audio");
    let safe: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    let count = ASR_AUDIO_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "wp-ai-asr-{}-{}-{}-{}.wav",
        std::process::id(),
        unix_nanos(),
        count,
        if safe.is_empty() { "audio" } else { &safe }
    ))
}

/// 视频/音频 → 临时 16kHz 单声道 wav（ASR 前置换行一步完成），返回临时音频路径。
///
/// 用完请调用 [`cleanup_temp_audio`] 删除。
pub async fn extract_audio_for_asr(input: &Path) -> Result<PathBuf> {
    let out = default_asr_audio_path(input);
    extract_audio_ffmpeg(input, &out).await?;
    Ok(out)
}

/// 删除 ASR 临时音频（不存在视为成功）。
pub fn cleanup_temp_audio(path: &Path) {
    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------------------
// 单测
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use wp_subtitle::SubtitleFormat;

    use super::*;
    use crate::cache::AiCache;
    use crate::mock::MockProvider;
    use crate::provider::ProviderRegistry;
    use crate::service::{AiOutcome, AiResult, AiService, AiServiceConfig};
    use crate::tests::{cfg_and_registry, make_doc, next_event, TempDir};
    use crate::{AiProvider, TranslateOptions};

    fn styled_doc(n: usize) -> SubtitleDoc {
        let mut doc = make_doc(n);
        doc.ass_header = Some(
            "[Script Info]\nTitle: 测试字幕\nScriptType: v4.00+\nPlayResX: 1920\nPlayResY: 1080\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Arial,48,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,1,2,10,10,40,1\n"
                .to_string(),
        );
        doc
    }

    fn mock(translations: Vec<String>) -> Arc<MockProvider> {
        Arc::new(MockProvider::new("mock").with_translations(translations))
    }

    fn registry_of(provider: Arc<dyn AiProvider>) -> ProviderRegistry {
        let (_cfg, registry) = cfg_and_registry(provider, 10);
        registry
    }

    #[test]
    fn bilingual_ass_keeps_original_header_and_counts() {
        let doc = styled_doc(2);
        let translations = vec!["你好".to_string(), "世界".to_string()];
        let report = translate_to_bilingual_ass_report(&doc, &translations, &AssOptions::default());
        assert_eq!((report.matched, report.missing), (2, 0));
        assert!(report.content.contains("Dialogue:"), "应包含对话行");
        assert!(
            report.content.contains("Title: 测试字幕"),
            "应保留原 ASS 头 Script Info"
        );
        assert!(
            report.content.contains("Arial"),
            "应保留原样式（Default→Arial）"
        );
        assert!(report.content.contains("AI-Translated"), "应有译文轨样式");
        assert!(report.content.contains("你好") && report.content.contains("世界"));
        // Result 版本与 report 版本一致
        let ass = translate_to_bilingual_ass(&doc, &translations, &AssOptions::default()).unwrap();
        assert_eq!(ass, report.content);
    }

    #[test]
    fn translated_cue_list_marks_empty_and_identical_as_missing() {
        let doc = make_doc(3);
        let translations = vec!["译0".to_string(), String::new(), doc.cues[2].text.clone()];
        let cues = translated_cue_list(&doc, &translations, false);
        assert_eq!(cues.len(), 3);
        assert_eq!(cues[0].text, "译0");
        assert!(cues[1].text.is_empty(), "空译文应计为漏译");
        assert!(cues[2].text.is_empty(), "与原文相同的译文应计为漏译");
        assert_eq!(cues[0].start_ms, doc.cues[0].start_ms, "时间轴沿用原文");
    }

    #[tokio::test]
    async fn write_ass_counts_missing_and_creates_parent_dirs() {
        let temp = TempDir::new("integration-write");
        let cache = AiCache::new(temp.path());
        let doc = make_doc(3);
        let mock = mock(vec!["译0".to_string(), String::new(), "译2".to_string()]);
        let registry = registry_of(mock.clone());
        let out = temp.path().join("nested/deep/bilingual.ass");
        let result = translate_doc_and_write_ass(
            &registry,
            &cache,
            "fp-missing",
            &doc,
            &TranslateOptions::default(),
            &AssOptions::default(),
            &out,
        )
        .await
        .expect("写 ASS 应成功");

        assert_eq!(result.total, 3);
        assert_eq!(result.matched, 2);
        assert_eq!(result.missing, 1);
        assert!(!result.degraded);
        assert_eq!(result.provider_id.as_deref(), Some("mock"));
        assert!(!result.cached);
        assert!((result.coverage() - 2.0 / 3.0).abs() < 1e-6);
        assert!(out.is_file(), "应写出目标 ASS 文件（自动建父目录）");
        let content = std::fs::read_to_string(&out).expect("读 ASS 文件");
        assert_eq!(content, result.content);
        assert!(content.contains("译0") && content.contains("译2"));
        assert!(content.contains("第1句"), "漏译 cue 应输出原文：{content}");
    }

    #[tokio::test]
    async fn write_ass_degrades_to_original_only_on_provider_failure() {
        let temp = TempDir::new("integration-degrade");
        let cache = AiCache::new(temp.path());
        let doc = make_doc(3);
        let failing = Arc::new(MockProvider::new("mock").always_fail());
        let provider: Arc<dyn AiProvider> = failing.clone();
        let registry = registry_of(provider);
        let out = temp.path().join("degraded.ass");
        let result = translate_doc_and_write_ass(
            &registry,
            &cache,
            "fp-degrade",
            &doc,
            &TranslateOptions::default(),
            &AssOptions::default(),
            &out,
        )
        .await
        .expect("降级仍应写出原文 ASS");
        assert!(result.degraded);
        assert_eq!(result.matched, 0);
        assert_eq!(result.missing, 3);
        assert_eq!(result.provider_id, None);
        assert!(
            result.errors.iter().any(|e| e.contains("AI 翻译失败")),
            "应带失败原因：{:?}",
            result.errors
        );
        let content = std::fs::read_to_string(&out).expect("读降级 ASS");
        assert!(content.contains("第0句") && content.contains("第2句"));
    }

    #[tokio::test]
    async fn written_ass_can_be_parsed_back_by_wp_subtitle() {
        let temp = TempDir::new("integration-readback");
        let cache = AiCache::new(temp.path());
        let doc = make_doc(2);
        let mock = mock(vec!["译A".to_string(), "译B".to_string()]);
        let registry = registry_of(mock);
        let out = temp.path().join("readback.ass");
        let result = translate_doc_and_write_ass(
            &registry,
            &cache,
            "fp-readback",
            &doc,
            &TranslateOptions::default(),
            &AssOptions::default(),
            &out,
        )
        .await
        .expect("写 ASS 应成功");
        let parsed =
            wp_subtitle::parse(SubtitleFormat::Ass, &result.content).expect("ASS 应可回读解析");
        assert!(parsed.cues.len() >= 2, "回读 cue 数：{}", parsed.cues.len());
        assert!(
            parsed.cues.iter().any(|c| c.text.contains("译A")),
            "回读应包含译文：{:?}",
            parsed
                .cues
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
        );
        assert!(parsed.cues.iter().any(|c| c.text.contains("第0句")));
    }

    #[tokio::test]
    async fn second_write_hits_translation_cache() {
        let temp = TempDir::new("integration-cache");
        let cache = AiCache::new(temp.path()).with_max_entries(10);
        let doc = make_doc(2);
        let mock = mock(vec!["译0".to_string(), "译1".to_string()]);
        let registry = registry_of(mock.clone());
        let opts = TranslateOptions::default();
        let out1 = temp.path().join("first.ass");
        let first = translate_doc_and_write_ass(
            &registry,
            &cache,
            "fp-cache",
            &doc,
            &opts,
            &AssOptions::default(),
            &out1,
        )
        .await
        .expect("第一次写 ASS");
        assert!(!first.cached);
        assert_eq!(mock.translate_calls(), 1);

        let out2 = temp.path().join("second.ass");
        let second = translate_doc_and_write_ass(
            &registry,
            &cache,
            "fp-cache",
            &doc,
            &opts,
            &AssOptions::default(),
            &out2,
        )
        .await
        .expect("第二次写 ASS");
        assert!(second.cached, "同指纹应命中翻译缓存");
        assert_eq!(second.provider_id, None);
        assert_eq!(mock.translate_calls(), 1, "命中缓存不应调 Provider");
        assert_eq!(second.matched, first.matched);
        assert!(out2.is_file());
    }

    #[tokio::test]
    async fn service_translations_flow_into_bilingual_ass() {
        let temp = TempDir::new("integration-service");
        let doc = make_doc(2);
        let mock = mock(vec!["服务译0".to_string(), "服务译1".to_string()]);
        let provider: Arc<dyn AiProvider> = mock.clone();
        let (ai, registry) = cfg_and_registry(provider, 10);
        let config = AiServiceConfig::new(ai)
            .with_cache_dir(temp.path())
            .with_task_timeout(Duration::from_secs(5));
        let (service, mut events) = AiService::spawn(config, registry).expect("启动 AI 服务");
        let id = service
            .submit_subtitle_translation(&doc, TranslateOptions::default(), "fp-svc")
            .expect("提交翻译任务");
        let event = next_event(&mut events).await;
        assert_eq!(event.task_id, id);
        let translations = match event.outcome {
            AiOutcome::Done(AiResult::Translations { translations, .. }) => translations,
            other => panic!("期望翻译完成，实际：{other:?}"),
        };
        let ass = translate_to_bilingual_ass(&doc, &translations, &AssOptions::default())
            .expect("生成双语 ASS");
        assert!(ass.contains("服务译0") && ass.contains("服务译1"));
        assert!(ass.contains("Dialogue:"));
    }

    // ---------------- ffmpeg 抽音轨 ----------------

    #[cfg(unix)]
    fn forge_cli(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-ffmpeg");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("写假 ffmpeg 脚本");
        let mut perms = std::fs::metadata(&path)
            .expect("读假 CLI 元数据")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod 假 CLI");
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn extract_audio_success_with_forged_ffmpeg() {
        let temp = TempDir::new("ffmpeg-ok");
        let args_file = temp.path().join("args.txt");
        let body = format!(
            "out=\"\"\nfor a in \"$@\"; do out=\"$a\"; done\nprintf '%s\\n' \"$@\" > '{}'\nprintf 'RIFF....WAVEfmt ' > \"$out\"",
            args_file.display()
        );
        let cli = forge_cli(temp.path(), &body);
        let input = temp.path().join("video.mkv");
        std::fs::write(&input, b"fake-video").expect("写假视频");
        let out = temp.path().join("audio.wav");
        let options = FfmpegOptions::default().with_cli(cli.display().to_string());
        extract_audio_ffmpeg_with(&options, &input, &out)
            .await
            .expect("假 ffmpeg 抽轨应成功");
        assert!(out.is_file());
        assert!(std::fs::metadata(&out).expect("读输出").len() > 0);
        let args = std::fs::read_to_string(&args_file).expect("读取参数记录");
        for expected in [
            "-vn",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-c:a",
            "pcm_s16le",
            &input.display().to_string(),
            &out.display().to_string(),
        ] {
            assert!(args.contains(expected), "参数应包含 {expected}：{args}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn extract_audio_missing_cli_is_not_found() {
        let temp = TempDir::new("ffmpeg-missing-cli");
        let input = temp.path().join("video.mkv");
        std::fs::write(&input, b"fake-video").expect("写假视频");
        let out = temp.path().join("audio.wav");
        let options = FfmpegOptions::default().with_cli("/definitely/not/ffmpeg");
        let err = extract_audio_ffmpeg_with(&options, &input, &out)
            .await
            .expect_err("CLI 缺失应失败");
        assert_eq!(err.kind(), wp_core::ErrorKind::NotFound);
        assert!(
            err.to_string().contains("找不到 ffmpeg"),
            "错误应可读：{err}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn extract_audio_nonzero_exit_reports_stderr() {
        let temp = TempDir::new("ffmpeg-exit");
        let cli = forge_cli(
            temp.path(),
            "echo 'Invalid data found when processing input' >&2\nexit 1",
        );
        let input = temp.path().join("video.mkv");
        std::fs::write(&input, b"not-a-video").expect("写假视频");
        let out = temp.path().join("audio.wav");
        let options = FfmpegOptions::default().with_cli(cli.display().to_string());
        let err = extract_audio_ffmpeg_with(&options, &input, &out)
            .await
            .expect_err("非零退出应失败");
        let message = err.to_string();
        assert!(message.contains("退出码 1"), "{message}");
        assert!(message.contains("Invalid data found"), "{message}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn extract_audio_timeout_kills_process() {
        let temp = TempDir::new("ffmpeg-timeout");
        let cli = forge_cli(temp.path(), "sleep 5\nexit 0");
        let input = temp.path().join("video.mkv");
        std::fs::write(&input, b"fake-video").expect("写假视频");
        let out = temp.path().join("audio.wav");
        let options = FfmpegOptions::default()
            .with_cli(cli.display().to_string())
            .with_timeout(Duration::from_millis(200));
        let started = std::time::Instant::now();
        let err = extract_audio_ffmpeg_with(&options, &input, &out)
            .await
            .expect_err("应超时");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "超时应及时返回并 kill"
        );
        assert_eq!(err.kind(), wp_core::ErrorKind::Network);
        assert!(err.to_string().contains("超时"), "错误应可读：{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn extract_audio_success_without_output_is_error() {
        let temp = TempDir::new("ffmpeg-no-output");
        let cli = forge_cli(temp.path(), "exit 0");
        let input = temp.path().join("video.mkv");
        std::fs::write(&input, b"fake-video").expect("写假视频");
        let out = temp.path().join("audio.wav");
        let options = FfmpegOptions::default().with_cli(cli.display().to_string());
        let err = extract_audio_ffmpeg_with(&options, &input, &out)
            .await
            .expect_err("无输出应失败");
        assert!(
            err.to_string().contains("未生成音频文件"),
            "错误应可读：{err}"
        );
    }

    #[tokio::test]
    async fn extract_audio_missing_input_is_not_found() {
        let temp = TempDir::new("ffmpeg-missing-input");
        let options = FfmpegOptions::default();
        let err = extract_audio_ffmpeg_with(
            &options,
            Path::new("/definitely/not/exists/video.mkv"),
            &temp.path().join("audio.wav"),
        )
        .await
        .expect_err("输入缺失应失败");
        assert_eq!(err.kind(), wp_core::ErrorKind::NotFound);
        assert!(err.to_string().contains("输入媒体文件不存在"));
    }

    #[test]
    fn asr_temp_audio_paths_are_unique_wav_in_temp() {
        let a = default_asr_audio_path(Path::new("/media/movie.mkv"));
        let b = default_asr_audio_path(Path::new("/media/movie.mkv"));
        assert_ne!(a, b);
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("wav"));
        assert_eq!(a.parent(), Some(std::env::temp_dir().as_path()));
        let name = a.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("wp-ai-asr-"));
        assert!(name.contains("movie"));
    }

    #[test]
    fn cleanup_temp_audio_removes_file_best_effort() {
        let temp = TempDir::new("ffmpeg-cleanup");
        let path = temp.path().join("audio.wav");
        std::fs::write(&path, b"RIFF").expect("写临时音频");
        cleanup_temp_audio(&path);
        assert!(!path.exists());
        cleanup_temp_audio(&path); // 重复删除不 panic
    }

    /// 用本机真实 ffmpeg 做一次端到端抽轨（无 ffmpeg 时跳过，不阻塞 CI）。
    #[tokio::test]
    async fn extract_audio_real_ffmpeg_roundtrip() {
        let Ok(version) = std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
        else {
            return;
        };
        if !version.status.success() {
            return;
        }
        let temp = TempDir::new("ffmpeg-real");
        let input = temp.path().join("tone.wav");
        let generated = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-nostdin",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=0.3",
                "-ac",
                "1",
                "-ar",
                "44100",
            ])
            .arg(&input)
            .output()
            .expect("运行真实 ffmpeg 生成测试音频");
        assert!(
            generated.status.success(),
            "生成测试音频失败：{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        let out = temp.path().join("out16k.wav");
        extract_audio_ffmpeg(&input, &out)
            .await
            .expect("真实 ffmpeg 抽轨应成功");
        assert!(std::fs::metadata(&out).expect("读输出").len() > 1000);
    }
}
