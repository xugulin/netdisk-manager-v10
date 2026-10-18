//! 字幕摘要 / 章节生成（P3 原型）：分批窗口 → JSON 章节 → 合法化时间轴 → 缓存。
//!
//! 设计（架构 §7 P3：视频摘要/章节/问答）：
//! - **分批窗口**：把整片字幕按（默认 5 分钟，可配 1–10 分钟）时间窗切批，每批一次 `complete` 调用，
//!   避免超长 prompt/上下文超限；窗口同时受 `max_cues_per_window` 条数保护；
//! - **结构化输出**：要求模型输出 JSON
//!   `{chapters:[{start_ms,end_ms,title,summary}],overview,keywords[]}`；
//!   解析容错（markdown 围栏/前后废话/字段别名/字符串时间戳），失败回退纯文本；
//! - **失败降级**：某窗口 Provider 失败 → 该窗口生成「标题=首句」的降级章节，不中断整片；
//! - **时间轴合法化**：[`normalize_chapters`] 排序 / 裁剪到媒体时长 / 去重叠 / 丢弃空章节；
//! - **缓存**：复用 [`crate::cache::AiCache`]，键 =（文件指纹 + 模型 + 语言 + [`AiTaskKind::Summarize`]），
//!   二次播放零调用。
//!
//! # 数据结构（序列化 JSON，可直接给 GUI/前端）
//!
//! ```json
//! {
//!   "chapters": [
//!     { "start_ms": 0, "end_ms": 300000, "title": "开场与背景", "summary": "介绍主角与世界观" }
//!   ],
//!   "overview": "本片讲述……",
//!   "keywords": ["主角", "冒险"]
//! }
//! ```
//!
//! # 快速使用
//!
//! ```no_run
//! use wp_ai::cache::AiCache;
//! use wp_ai::provider::build_provider_registry;
//! use wp_ai::summarize::{summarize_subtitles, summary_to_markdown, SummarizeOptions};
//! use wp_ai::AiConfig;
//! use wp_subtitle::SubtitleDoc;
//!
//! # async fn demo(ai: AiConfig, doc: SubtitleDoc, cache: AiCache) -> wp_core::Result<()> {
//! let (registry, _warnings) = build_provider_registry(&ai);
//! let outcome = summarize_subtitles(
//!     &registry,
//!     &cache,
//!     "文件指纹",
//!     &doc,
//!     &SummarizeOptions::default(),
//! )
//! .await;
//! println!("{}", outcome.summary.to_json_pretty()?);      // 章节 JSON
//! println!("{}", summary_to_markdown(&outcome.summary));   // 可读 markdown
//! if outcome.degraded {
//!     eprintln!("部分窗口降级：{:?}", outcome.errors);
//! }
//! # Ok(())
//! # }
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;
use wp_core::Result;
use wp_subtitle::{Cue, SubtitleDoc};

use crate::cache::{AiCache, CacheKey};
use crate::provider::complete_with_fallback;
use crate::{AiTaskKind, ProviderRegistry};

/// 默认窗口时长（5 分钟）。
pub const DEFAULT_WINDOW_MS: i64 = 300_000;
/// 窗口时长下限（1 分钟；测试/短视频可用更小窗口）。
pub const MIN_WINDOW_MS: i64 = 60_000;
/// 窗口时长上限（10 分钟）。
pub const MAX_WINDOW_MS: i64 = 600_000;
/// 每窗口默认最多送给模型的 cue 条数（防止单窗口字幕过密导致 prompt 过大）。
pub const DEFAULT_MAX_CUES_PER_WINDOW: usize = 240;
/// 每窗口请求模型生成的章节数上限。
pub const MAX_CHAPTERS_PER_WINDOW: usize = 4;
/// 关键词总数上限。
pub const MAX_KEYWORDS: usize = 20;
/// 单窗口 transcript 字符上限（超长截断，避免上下文超限）。
const MAX_TRANSCRIPT_CHARS: usize = 12_000;

/// 单个章节（时间轴为整片绝对毫秒）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chapter {
    /// 章节开始（绝对毫秒）。
    pub start_ms: i64,
    /// 章节结束（绝对毫秒，保证 > start_ms）。
    pub end_ms: i64,
    /// 章节标题（短）。
    pub title: String,
    /// 章节摘要（一句话）。
    pub summary: String,
}

impl Chapter {
    /// 章节时长（毫秒，非负）。
    pub fn duration_ms(&self) -> i64 {
        (self.end_ms - self.start_ms).max(0)
    }

    /// 时间点是否落在本章节内（`[start, end)`）。
    pub fn contains_ms(&self, time_ms: i64) -> bool {
        time_ms >= self.start_ms && time_ms < self.end_ms
    }
}

/// 字幕摘要结果（可直接序列化为 JSON 给 GUI）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiSummary {
    /// 章节列表（按时间排序、不重叠）。
    #[serde(default)]
    pub chapters: Vec<Chapter>,
    /// 整片总览（1–3 句）。
    #[serde(default)]
    pub overview: String,
    /// 关键词（去重、最多 [`MAX_KEYWORDS`] 个）。
    #[serde(default)]
    pub keywords: Vec<String>,
}

impl AiSummary {
    /// 是否为空摘要（无章节且无总览）。
    pub fn is_empty(&self) -> bool {
        self.chapters.is_empty() && self.overview.trim().is_empty()
    }

    /// 章节总数。
    pub fn chapter_count(&self) -> usize {
        self.chapters.len()
    }

    /// 找到时间点所在章节（按 `[start, end)`）。
    pub fn find_chapter(&self, time_ms: i64) -> Option<&Chapter> {
        self.chapters.iter().find(|c| c.contains_ms(time_ms))
    }

    /// 序列化为带缩进的 JSON（结构化输出/GUI 展示用）。
    pub fn to_json_pretty(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// 从 JSON 反序列化（缓存/前端回读）。
    pub fn from_json(text: &str) -> Result<Self> {
        Ok(serde_json::from_str(text)?)
    }
}

/// 摘要生成参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummarizeOptions {
    /// 每批窗口时长（毫秒；默认 5 分钟，实际使用前会 clamp 到 1–10 分钟）。
    pub window_ms: i64,
    /// 每窗口最多 cue 条数（默认 240）。
    pub max_cues_per_window: usize,
    /// 输出语言（`zh` 简体中文 / `zh-TW` / `en`…）。
    pub target_lang: String,
}

impl Default for SummarizeOptions {
    fn default() -> Self {
        Self {
            window_ms: DEFAULT_WINDOW_MS,
            max_cues_per_window: DEFAULT_MAX_CUES_PER_WINDOW,
            target_lang: "zh".into(),
        }
    }
}

impl SummarizeOptions {
    /// 实际窗口时长（clamp 到 [`MIN_WINDOW_MS`]..=[`MAX_WINDOW_MS`]）。
    pub fn effective_window_ms(&self) -> i64 {
        self.window_ms.clamp(MIN_WINDOW_MS, MAX_WINDOW_MS)
    }

    /// 实际每窗口 cue 上限（至少 1）。
    pub fn effective_max_cues(&self) -> usize {
        self.max_cues_per_window.max(1)
    }
}

impl SummarizeOptions {
    /// 指定窗口时长（会 clamp）。
    pub fn with_window_ms(mut self, window_ms: i64) -> Self {
        self.window_ms = window_ms;
        self
    }

    /// 指定每窗口 cue 上限。
    pub fn with_max_cues(mut self, max_cues: usize) -> Self {
        self.max_cues_per_window = max_cues;
        self
    }

    /// 指定输出语言。
    pub fn with_target_lang(mut self, lang: impl Into<String>) -> Self {
        self.target_lang = lang.into();
        self
    }
}

/// 摘要任务结果（失败降级信息 + 缓存/Provider 元数据）。
#[derive(Debug, Clone)]
pub struct SummarizeOutcome {
    /// 章节 JSON 结构。
    pub summary: AiSummary,
    /// 分批窗口数（≈ Provider 调用次数；缓存命中时为文档应有的窗口数）。
    pub windows: usize,
    /// 实际使用的 Provider id（缓存命中/全部降级时为 None）。
    pub provider_id: Option<String>,
    /// 是否命中缓存。
    pub cached: bool,
    /// 是否有窗口降级（Provider 失败，已用「标题=首句」兜底）。
    pub degraded: bool,
    /// 失败原因（中文可读，不含密钥）。
    pub errors: Vec<String>,
    /// 非致命警告（模型输出非 JSON 回退、缓存写失败等）。
    pub warnings: Vec<String>,
}

impl SummarizeOutcome {
    /// 是否完整成功（无降级、无警告）。
    pub fn is_complete(&self) -> bool {
        !self.degraded && self.errors.is_empty() && self.warnings.is_empty()
    }

    /// 是否需要提示用户（降级/警告）。
    pub fn has_warning(&self) -> bool {
        self.degraded || !self.errors.is_empty() || !self.warnings.is_empty()
    }

    /// 章节数。
    pub fn chapter_count(&self) -> usize {
        self.summary.chapter_count()
    }
}

// ---------------------------------------------------------------------------
// 窗口切分
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Window {
    start_ms: i64,
    end_ms: i64,
    cues: Vec<Cue>,
}

impl Window {
    fn new(cues: Vec<Cue>) -> Self {
        let start_ms = cues.first().map(|c| c.start_ms.max(0)).unwrap_or(0);
        let end_ms = cues
            .iter()
            .map(|c| c.end_ms.max(c.start_ms))
            .max()
            .unwrap_or(start_ms)
            .max(start_ms);
        Self {
            start_ms,
            end_ms,
            cues,
        }
    }
}

/// 按时间窗 + 条数上限切分字幕（跳过空文本 cue；时间升序）。
fn build_windows(cues: &[Cue], window_ms: i64, max_cues_per_window: usize) -> Vec<Window> {
    let window_ms = window_ms.max(1);
    let max_cues = max_cues_per_window.max(1);
    let mut sorted: Vec<Cue> = cues
        .iter()
        .filter(|cue| !cue.text.trim().is_empty())
        .cloned()
        .collect();
    sorted.sort_by_key(|cue| (cue.start_ms, cue.end_ms));

    let mut windows: Vec<Window> = Vec::new();
    let mut current: Vec<Cue> = Vec::new();
    let mut window_start: Option<i64> = None;
    for cue in sorted {
        let start_new = match window_start {
            Some(start) => {
                cue.start_ms.saturating_sub(start) >= window_ms || current.len() >= max_cues
            }
            None => false,
        };
        if start_new {
            windows.push(Window::new(std::mem::take(&mut current)));
            window_start = None;
        }
        if window_start.is_none() {
            window_start = Some(cue.start_ms.max(0));
        }
        current.push(cue);
    }
    if !current.is_empty() {
        windows.push(Window::new(current));
    }
    windows
}

// ---------------------------------------------------------------------------
// Prompt 构造
// ---------------------------------------------------------------------------

fn target_lang_name(lang: &str) -> &str {
    match lang {
        "zh" | "zh-CN" | "zh-Hans" => "简体中文",
        "zh-TW" | "zh-HK" | "zh-Hant" => "繁体中文",
        "en" => "英语",
        "ja" => "日语",
        "ko" => "韩语",
        other => other,
    }
}

fn format_hms(ms: i64) -> String {
    let ms = ms.max(0);
    let hours = ms / 3_600_000;
    let minutes = (ms % 3_600_000) / 60_000;
    let seconds = (ms % 60_000) / 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn truncate_chars(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

/// 构造单窗口摘要 prompt（要求 JSON 输出，含绝对时间范围约束）。
fn build_summary_prompt(
    window_index: usize,
    total_windows: usize,
    window: &Window,
    opts: &SummarizeOptions,
) -> String {
    let mut transcript = String::new();
    for cue in &window.cues {
        let text = cue.text.replace(['\r', '\n'], " ");
        transcript.push_str(&format!("[{}ms] {}\n", cue.start_ms.max(0), text));
    }
    let transcript = truncate_chars(&transcript, MAX_TRANSCRIPT_CHARS);
    format!(
        "你是视频章节与摘要生成器。下面是第 {index}/{total} 段字幕（该片段的绝对时间范围：{start}ms 到 {end}ms）。\n\
         只输出一个 JSON 对象，不要 markdown 代码块、不要解释。格式：\n\
         {{\"chapters\":[{{\"start_ms\":<整数绝对毫秒>,\"end_ms\":<整数绝对毫秒>,\"title\":\"<12 字以内标题>\",\"summary\":\"<40 字以内摘要>\"}}],\"overview\":\"<1-2 句本段总览>\",\"keywords\":[\"<关键词>\"]}}\n\
         要求：\n\
         1. 生成 1-{max_chapters} 个章节，按时间顺序；start_ms/end_ms 为整部视频的绝对毫秒，必须落在 [{start},{end}] 内且 start_ms < end_ms；\n\
         2. title/summary/overview/keywords 全部使用{lang}；\n\
         3. 不要臆测字幕之外的信息；纯语气词、重复内容可跳过。\n\n\
         字幕：\n{transcript}",
        index = window_index + 1,
        total = total_windows,
        start = window.start_ms,
        end = window.end_ms,
        max_chapters = MAX_CHAPTERS_PER_WINDOW,
        lang = target_lang_name(&opts.target_lang),
        transcript = transcript,
    )
}

// ---------------------------------------------------------------------------
// JSON 解析（容错）
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct ParsedWindow {
    chapters: Vec<Chapter>,
    overview: String,
    keywords: Vec<String>,
    json_ok: bool,
}

/// 解析模型回复：优先 JSON（允许围栏/前后废话/字段别名/字符串时间戳），失败回退纯文本。
fn parse_summary_response(reply: &str, window: &Window) -> ParsedWindow {
    if let Some(value) = extract_json_value(reply) {
        let chapters = parse_chapters_from_value(&value, window);
        let overview =
            first_non_empty_string(&value, &["overview", "总览", "概要", "summary", "摘要"]);
        let keywords = first_keywords(&value, &["keywords", "关键词", "tags"]);
        if !chapters.is_empty() || !overview.is_empty() || !keywords.is_empty() {
            return ParsedWindow {
                chapters,
                overview,
                keywords,
                json_ok: true,
            };
        }
        return ParsedWindow {
            json_ok: true,
            ..Default::default()
        };
    }
    // 纯文本回退：标题=首句，摘要=模型输出。
    let mut chapter = fallback_chapter(window);
    chapter.summary = truncate_chars(reply, 500);
    ParsedWindow {
        chapters: vec![chapter],
        overview: String::new(),
        keywords: Vec::new(),
        json_ok: false,
    }
}

/// 从任意文本中提取 JSON（整段 / markdown 围栏内 / 首个 `{...}` / 首个 `[...]`）。
fn extract_json_value(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Some(value);
    }
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if end > start {
            if let Ok(value) = serde_json::from_str::<Value>(trimmed[start..=end].trim()) {
                return Some(value);
            }
        }
    }
    if let (Some(start), Some(end)) = (trimmed.find('['), trimmed.rfind(']')) {
        if end > start {
            if let Ok(value) = serde_json::from_str::<Value>(trimmed[start..=end].trim()) {
                return Some(value);
            }
        }
    }
    None
}

fn parse_chapters_from_value(value: &Value, window: &Window) -> Vec<Chapter> {
    let raw_chapters: Vec<&Value> = if let Some(array) = value.as_array() {
        array.iter().collect()
    } else if let Some(array) = value
        .get("chapters")
        .or_else(|| value.get("chapter"))
        .and_then(Value::as_array)
    {
        array.iter().collect()
    } else if value.get("title").is_some() || value.get("start_ms").is_some() {
        // 单个章节对象
        vec![value]
    } else {
        Vec::new()
    };
    let mut chapters: Vec<Chapter> = Vec::new();
    for raw in raw_chapters {
        if let Some(chapter) = parse_chapter(raw, window) {
            if !(chapter.title.is_empty() && chapter.summary.is_empty()) {
                chapters.push(chapter);
            }
        }
    }
    chapters
}

fn parse_chapter(value: &Value, window: &Window) -> Option<Chapter> {
    if !value.is_object() {
        return None;
    }
    let start = first_i64(value, &["start_ms", "start", "startTime", "开始"]);
    let end = first_i64(value, &["end_ms", "end", "endTime", "结束"]);
    let title = first_non_empty_string(value, &["title", "name", "标题"]);
    let summary = first_non_empty_string(value, &["summary", "description", "摘要", "内容"]);
    if title.is_empty() && summary.is_empty() {
        return None;
    }
    Some(Chapter {
        start_ms: start.unwrap_or(window.start_ms),
        end_ms: end.unwrap_or(window.end_ms),
        title,
        summary,
    })
}

fn first_i64(value: &Value, keys: &[&str]) -> Option<i64> {
    for key in keys {
        if let Some(inner) = value.get(*key) {
            if let Some(parsed) = json_i64(inner) {
                return Some(parsed);
            }
        }
    }
    None
}

fn json_i64(value: &Value) -> Option<i64> {
    if let Some(number) = value.as_i64() {
        return Some(number);
    }
    if let Some(number) = value.as_f64() {
        return Some(number.round() as i64);
    }
    if let Some(text) = value.as_str() {
        let text = text.trim();
        if let Ok(number) = text.parse::<i64>() {
            return Some(number);
        }
        if let Ok(number) = text.parse::<f64>() {
            return Some(number.round() as i64);
        }
        return wp_subtitle::parse_timestamp(text);
    }
    None
}

fn first_non_empty_string(value: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(inner) = value.get(*key) {
            match inner {
                Value::String(text) => {
                    let text = text.trim();
                    if !text.is_empty() {
                        return text.to_string();
                    }
                }
                Value::Number(_) | Value::Bool(_) => return inner.to_string(),
                _ => {}
            }
        }
    }
    String::new()
}

fn first_keywords(value: &Value, keys: &[&str]) -> Vec<String> {
    for key in keys {
        if let Some(inner) = value.get(*key) {
            let list = parse_keywords_value(inner);
            if !list.is_empty() {
                return list;
            }
        }
    }
    Vec::new()
}

fn parse_keywords_value(value: &Value) -> Vec<String> {
    let mut keywords = Vec::new();
    match value {
        Value::Array(items) => {
            for item in items {
                let text = match item {
                    Value::String(text) => text.trim().to_string(),
                    Value::Number(_) | Value::Bool(_) => item.to_string(),
                    _ => continue,
                };
                if !text.is_empty() {
                    keywords.push(text);
                }
            }
        }
        Value::String(text) => {
            for part in text.split([',', '，', '、', ';', '；', '/']) {
                let part = part.trim();
                if !part.is_empty() {
                    keywords.push(part.to_string());
                }
            }
        }
        _ => {}
    }
    keywords
}

// ---------------------------------------------------------------------------
// 章节合法化 / 合并
// ---------------------------------------------------------------------------

/// 章节时间轴合法化：排序 → 裁剪到 `[0, media_end_ms]` → 去重叠 → 丢弃空章节。
///
/// 不合法输入的处理规则（文档化）：
/// - 负时间裁剪为 0；越界结束裁剪为 `media_end_ms`；
/// - 与前章重叠的起始时间后移；无可用空间（起点已到片尾）则丢弃该章节；
/// - `end <= start` 时补到 `start + 1ms`（保留信息）；空标题且空摘要的章节丢弃。
pub fn normalize_chapters(chapters: Vec<Chapter>, media_end_ms: i64) -> Vec<Chapter> {
    let limit = media_end_ms.max(0);
    let mut list: Vec<Chapter> = chapters
        .into_iter()
        .filter(|chapter| !(chapter.title.trim().is_empty() && chapter.summary.trim().is_empty()))
        .collect();
    list.sort_by_key(|chapter| (chapter.start_ms, chapter.end_ms));

    let mut out: Vec<Chapter> = Vec::with_capacity(list.len());
    let mut prev_end: i64 = 0;
    for mut chapter in list {
        chapter.title = chapter.title.trim().to_string();
        chapter.summary = chapter.summary.trim().to_string();
        let mut start = chapter.start_ms.max(0);
        let mut end = chapter.end_ms.max(start);
        if limit > 0 {
            start = start.min(limit);
            end = end.min(limit);
        }
        if start < prev_end {
            start = prev_end;
        }
        if end <= start {
            if limit > 0 && start >= limit {
                continue; // 片尾之后，丢弃
            }
            if limit > 0 {
                end = limit.min(start.saturating_add(1));
            }
            if end <= start {
                end = start.saturating_add(1);
            }
        }
        chapter.start_ms = start;
        chapter.end_ms = end;
        prev_end = end;
        out.push(chapter);
    }
    out
}

fn fallback_chapter(window: &Window) -> Chapter {
    let title = window
        .cues
        .iter()
        .map(|cue| cue.text.trim())
        .find(|text| !text.is_empty())
        .map(|text| truncate_chars(text, 40))
        .unwrap_or_else(|| format!("片段 {}", format_hms(window.start_ms)));
    Chapter {
        start_ms: window.start_ms,
        end_ms: window.end_ms.max(window.start_ms.saturating_add(1)),
        title,
        summary: String::new(),
    }
}

fn merge_overviews(parts: &[String]) -> String {
    let mut unique: Vec<&str> = Vec::new();
    for part in parts {
        let part = part.trim().trim_end_matches(['。', '.', '；', ';']).trim();
        if part.is_empty() {
            continue;
        }
        if !unique.contains(&part) {
            unique.push(part);
        }
    }
    if unique.is_empty() {
        return String::new();
    }
    truncate_chars(&format!("{}。", unique.join("；")), 500)
}

fn normalize_keywords(keywords: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for keyword in keywords {
        let keyword = keyword.trim().to_string();
        if keyword.is_empty() {
            continue;
        }
        if out
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&keyword))
        {
            continue;
        }
        out.push(keyword);
        if out.len() >= MAX_KEYWORDS {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------------------

/// 整片字幕 → 章节/摘要（分批窗口 + 缓存 + 失败降级，**永不返回 Err**）。
///
/// - `fingerprint`：视频/字幕文件指纹（空=不缓存）；
/// - Provider 由 registry 按 `priority`/`offline_only`/能力矩阵选择，单个窗口失败会回退下一个
///   Provider（[`complete_with_fallback`]），全部失败则该窗口降级为「标题=首句」；
/// - 缓存键 =（指纹, 候选模型, `opts.target_lang`, `AiTaskKind::Summarize`），降级结果不写缓存。
pub async fn summarize_subtitles(
    registry: &ProviderRegistry,
    cache: &AiCache,
    fingerprint: &str,
    doc: &SubtitleDoc,
    opts: &SummarizeOptions,
) -> SummarizeOutcome {
    let windows = build_windows(
        &doc.cues,
        opts.effective_window_ms(),
        opts.effective_max_cues(),
    );
    if windows.is_empty() {
        return SummarizeOutcome {
            summary: AiSummary::default(),
            windows: 0,
            provider_id: None,
            cached: false,
            degraded: false,
            errors: Vec::new(),
            warnings: Vec::new(),
        };
    }

    let model = registry
        .candidate_configs(AiTaskKind::Summarize)
        .first()
        .map(|cfg| cfg.model.clone())
        .unwrap_or_default();
    let use_cache = !fingerprint.trim().is_empty();
    let cache_key = CacheKey::new(
        fingerprint,
        model,
        opts.target_lang.clone(),
        AiTaskKind::Summarize,
    );
    if use_cache {
        if let Some(value) = cache.get(&cache_key) {
            if let Ok(summary) = serde_json::from_value::<AiSummary>(value) {
                return SummarizeOutcome {
                    summary,
                    windows: windows.len(),
                    provider_id: None,
                    cached: true,
                    degraded: false,
                    errors: Vec::new(),
                    warnings: Vec::new(),
                };
            }
        }
    }

    let media_end_ms = doc
        .cues
        .iter()
        .map(|cue| cue.end_ms.max(cue.start_ms))
        .max()
        .unwrap_or(0)
        .max(0);
    let total = windows.len();
    let mut chapters: Vec<Chapter> = Vec::new();
    let mut overviews: Vec<String> = Vec::new();
    let mut keywords: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut degraded = false;
    let mut provider_id: Option<String> = None;

    for (index, window) in windows.iter().enumerate() {
        let prompt = build_summary_prompt(index, total, window, opts);
        match complete_with_fallback(registry, &prompt).await {
            Ok((reply, provider)) => {
                if provider_id.is_none() {
                    provider_id = Some(provider);
                }
                let parsed = parse_summary_response(&reply, window);
                if !parsed.json_ok {
                    warnings.push(format!(
                        "窗口 {}/{}（{}）模型输出非 JSON，已按纯文本降级",
                        index + 1,
                        total,
                        format_hms(window.start_ms)
                    ));
                }
                if parsed.chapters.is_empty() {
                    warnings.push(format!(
                        "窗口 {}/{} 未解析出章节，已用首句作标题",
                        index + 1,
                        total
                    ));
                    chapters.push(fallback_chapter(window));
                } else {
                    chapters.extend(parsed.chapters);
                }
                if !parsed.overview.is_empty() {
                    overviews.push(parsed.overview);
                }
                keywords.extend(parsed.keywords);
            }
            Err(error) => {
                degraded = true;
                errors.push(format!(
                    "窗口 {}/{}（{}）摘要失败：{error}",
                    index + 1,
                    total,
                    format_hms(window.start_ms)
                ));
                chapters.push(fallback_chapter(window));
            }
        }
    }

    let summary = AiSummary {
        chapters: normalize_chapters(chapters, media_end_ms),
        overview: merge_overviews(&overviews),
        keywords: normalize_keywords(keywords),
    };
    if use_cache && !degraded {
        match serde_json::to_value(&summary) {
            Ok(value) => {
                if let Err(error) = cache.put(&cache_key, value) {
                    warnings.push(format!("摘要成功但缓存写入失败：{error}"));
                }
            }
            Err(error) => warnings.push(format!("摘要结果序列化失败，未写缓存：{error}")),
        }
    }
    SummarizeOutcome {
        summary,
        windows: total,
        provider_id,
        cached: false,
        degraded,
        errors,
        warnings,
    }
}

/// 摘要 → 可读 Markdown（导出/调试/GUI 文本面板用）。
pub fn summary_to_markdown(summary: &AiSummary) -> String {
    let mut out = String::from("# 视频摘要\n\n");
    let overview = summary.overview.trim();
    if !overview.is_empty() {
        out.push_str(&format!("**总览**：{overview}\n\n"));
    }
    if !summary.keywords.is_empty() {
        out.push_str(&format!("**关键词**：{}\n\n", summary.keywords.join("、")));
    }
    out.push_str("## 章节\n\n");
    if summary.chapters.is_empty() {
        out.push_str("（无章节，可能字幕为空或 AI 不可用）\n");
        return out;
    }
    for chapter in &summary.chapters {
        let title = if chapter.title.trim().is_empty() {
            "未命名章节"
        } else {
            chapter.title.trim()
        };
        let range = format!(
            "[{}–{}]",
            format_hms(chapter.start_ms),
            format_hms(chapter.end_ms)
        );
        if chapter.summary.trim().is_empty() {
            out.push_str(&format!("- `{range}` **{title}**\n"));
        } else {
            out.push_str(&format!(
                "- `{range}` **{title}**：{}\n",
                chapter.summary.trim()
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 单测
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use wp_subtitle::SubtitleFormat;

    use super::*;
    use crate::mock::MockProvider;
    use crate::provider::ProviderRegistry;
    use crate::tests::{cfg_and_registry, TempDir};

    fn cue(start_ms: i64, text: &str) -> Cue {
        Cue {
            start_ms,
            end_ms: start_ms + 2_000,
            text: text.to_string(),
            raw_style: None,
        }
    }

    fn doc_with(cues: Vec<Cue>) -> SubtitleDoc {
        SubtitleDoc {
            format: Some(SubtitleFormat::Srt),
            cues,
            ass_header: None,
        }
    }

    /// 两个 5 分钟窗口（默认窗口 300s；第二窗从 6 分钟开始）。
    fn two_window_doc() -> SubtitleDoc {
        let mut cues: Vec<Cue> = Vec::new();
        for window in 0..2 {
            for index in 0..3 {
                let start = window * 360_000 + index * 1_000;
                cues.push(cue(start, &format!("第{window}窗第{index}句")));
            }
        }
        doc_with(cues)
    }

    fn setup(completions: Vec<String>) -> (ProviderRegistry, Arc<MockProvider>) {
        let mock = Arc::new(MockProvider::new("mock-summarize").with_completions(completions));
        let provider: Arc<dyn crate::AiProvider> = mock.clone();
        let (_cfg, registry) = cfg_and_registry(provider, 10);
        (registry, mock)
    }

    fn json_reply(
        chapters: &[(i64, i64, &str, &str)],
        overview: &str,
        keywords: &[&str],
    ) -> String {
        let chapters: Vec<Value> = chapters
            .iter()
            .map(|(start, end, title, summary)| {
                serde_json::json!({
                    "start_ms": start, "end_ms": end, "title": title, "summary": summary
                })
            })
            .collect();
        serde_json::json!({
            "chapters": chapters,
            "overview": overview,
            "keywords": keywords,
        })
        .to_string()
    }

    #[tokio::test]
    async fn windows_are_batched_and_chapters_merged() {
        let temp = TempDir::new("summarize-windows");
        let cache = AiCache::new(temp.path());
        let doc = two_window_doc();
        let replies = vec![
            json_reply(
                &[(0, 180_000, "开场", "介绍背景")],
                "前段总览",
                &["开场", "测试"],
            ),
            json_reply(
                &[(360_000, 362_000, "结尾", "收束剧情")],
                "后段总览",
                &["结尾", "测试"],
            ),
        ];
        let (registry, mock) = setup(replies);
        let outcome = summarize_subtitles(
            &registry,
            &cache,
            "window-fp",
            &doc,
            &SummarizeOptions::default(),
        )
        .await;
        assert_eq!(outcome.windows, 2, "12 分钟字幕应切 2 个 5 分钟窗口");
        assert_eq!(mock.complete_calls(), 2);
        assert!(!outcome.degraded);
        assert!(outcome.errors.is_empty());
        assert_eq!(outcome.chapter_count(), 2);
        assert_eq!(outcome.summary.chapters[0].start_ms, 0);
        assert_eq!(outcome.summary.chapters[1].start_ms, 360_000);
        assert_eq!(outcome.summary.chapters[0].title, "开场");
        assert!(
            outcome.summary.overview.contains("前段总览")
                && outcome.summary.overview.contains("后段总览"),
            "总览应合并：{}",
            outcome.summary.overview
        );
        assert_eq!(
            outcome.summary.keywords,
            vec!["开场".to_string(), "测试".to_string(), "结尾".to_string()]
        );
        assert_eq!(outcome.provider_id.as_deref(), Some("mock-summarize"));
        assert!(!outcome.cached);
    }

    #[tokio::test]
    async fn max_cues_per_window_limits_batches() {
        let temp = TempDir::new("summarize-max-cues");
        let cache = AiCache::new(temp.path());
        let doc = doc_with(vec![
            cue(0, "a"),
            cue(5_000, "b"),
            cue(10_000, "c"),
            cue(15_000, "d"),
            cue(20_000, "e"),
        ]);
        let reply = json_reply(&[(0, 20_000, "整段", "摘要")], "", &[]);
        let (registry, mock) = setup(vec![reply.clone(), reply.clone(), reply]);
        let opts = SummarizeOptions::default()
            .with_window_ms(60_000)
            .with_max_cues(2);
        let outcome = summarize_subtitles(&registry, &cache, "", &doc, &opts).await;
        assert_eq!(outcome.windows, 3, "5 条 / 每窗 2 条 = 3 批");
        assert_eq!(mock.complete_calls(), 3);
    }

    #[test]
    fn window_ms_is_clamped_to_1_10_minutes() {
        assert_eq!(
            SummarizeOptions::default().effective_window_ms(),
            DEFAULT_WINDOW_MS
        );
        let too_small = SummarizeOptions::default().with_window_ms(1_000);
        assert_eq!(too_small.effective_window_ms(), MIN_WINDOW_MS);
        let too_big = SummarizeOptions::default().with_window_ms(i64::MAX / 2);
        assert_eq!(too_big.effective_window_ms(), MAX_WINDOW_MS);
        assert_eq!(
            SummarizeOptions::default()
                .with_max_cues(0)
                .effective_max_cues(),
            1
        );
    }

    #[test]
    fn parses_json_inside_markdown_fence_with_prose() {
        let cues = vec![cue(5_000, "第一句")];
        let windows = build_windows(&cues, DEFAULT_WINDOW_MS, 100);
        let window = &windows[0];
        let json = json_reply(&[(5_000, 7_000, "标题", "摘要")], "总览", &["关键词"]);
        let reply = format!("结果如下：\n```json\n{json}\n```\n以上，希望有帮助。");
        let parsed = parse_summary_response(&reply, window);
        assert!(parsed.json_ok);
        assert_eq!(parsed.chapters.len(), 1);
        assert_eq!(parsed.chapters[0].title, "标题");
        assert_eq!(parsed.overview, "总览");
        assert_eq!(parsed.keywords, vec!["关键词".to_string()]);
    }

    #[test]
    fn parses_alt_keys_and_string_timestamps() {
        let cues = vec![cue(5_000, "第一句")];
        let windows = build_windows(&cues, DEFAULT_WINDOW_MS, 100);
        let window = &windows[0];
        let reply = serde_json::json!({
            "chapters": [{
                "start": "00:00:05,000",
                "end": 8_000,
                "name": "标题A",
                "摘要": "内容B"
            }],
            "概要": "总览C",
            "keywords": "甲, 乙、丙"
        })
        .to_string();
        let parsed = parse_summary_response(&reply, window);
        assert!(parsed.json_ok);
        assert_eq!(parsed.chapters[0].start_ms, 5_000);
        assert_eq!(parsed.chapters[0].end_ms, 8_000);
        assert_eq!(parsed.chapters[0].title, "标题A");
        assert_eq!(parsed.chapters[0].summary, "内容B");
        assert_eq!(parsed.overview, "总览C");
        assert_eq!(
            parsed.keywords,
            vec!["甲".to_string(), "乙".to_string(), "丙".to_string()]
        );
    }

    #[tokio::test]
    async fn plain_text_reply_falls_back_to_first_sentence() {
        let temp = TempDir::new("summarize-plain");
        let cache = AiCache::new(temp.path());
        let doc = doc_with(vec![cue(0, "第一句字幕内容"), cue(3_000, "第二句")]);
        let (registry, mock) = setup(vec!["这是一段纯文本摘要，没有 JSON。".to_string()]);
        let outcome =
            summarize_subtitles(&registry, &cache, "", &doc, &SummarizeOptions::default()).await;
        assert_eq!(mock.complete_calls(), 1);
        assert!(!outcome.degraded, "纯文本回退不算 Provider 失败");
        assert_eq!(outcome.chapter_count(), 1);
        assert_eq!(
            outcome.summary.chapters[0].title, "第一句字幕内容",
            "回退时标题=首句"
        );
        assert!(outcome.summary.chapters[0].summary.contains("纯文本摘要"));
        assert!(
            outcome.warnings.iter().any(|w| w.contains("非 JSON")),
            "应有回退警告：{:?}",
            outcome.warnings
        );
    }

    #[tokio::test]
    async fn provider_failure_degrades_to_first_sentence_titles() {
        let temp = TempDir::new("summarize-degrade");
        let cache = AiCache::new(temp.path());
        let doc = two_window_doc();
        let mock = Arc::new(MockProvider::new("mock-summarize").always_fail_complete());
        let provider: Arc<dyn crate::AiProvider> = mock.clone();
        let (_cfg, registry) = cfg_and_registry(provider, 10);
        let outcome =
            summarize_subtitles(&registry, &cache, "fp", &doc, &SummarizeOptions::default()).await;
        assert!(outcome.degraded);
        assert_eq!(outcome.errors.len(), 2, "两个窗口各一条失败原因");
        assert_eq!(outcome.chapter_count(), 2);
        assert_eq!(outcome.summary.chapters[0].title, "第0窗第0句");
        assert_eq!(outcome.summary.chapters[1].title, "第1窗第0句");
        assert_eq!(outcome.provider_id, None);
        assert!(outcome.errors[0].contains("摘要失败"));
    }

    #[test]
    fn normalize_chapters_sorts_clamps_and_avoids_overlap() {
        let chapters = vec![
            Chapter {
                start_ms: 90_000,
                end_ms: 200_000,
                title: "A".into(),
                summary: "a".into(),
            },
            Chapter {
                start_ms: -500,
                end_ms: 10_000,
                title: "B".into(),
                summary: "b".into(),
            },
            Chapter {
                start_ms: 5_000,
                end_ms: 20_000,
                title: "C".into(),
                summary: "c".into(),
            },
            Chapter {
                start_ms: 30_000,
                end_ms: 40_000,
                title: String::new(),
                summary: String::new(),
            },
            Chapter {
                start_ms: 250_000,
                end_ms: 260_000,
                title: "D".into(),
                summary: "d".into(),
            },
        ];
        let normalized = normalize_chapters(chapters, 120_000);
        assert_eq!(normalized.len(), 3, "空章节/片尾外章节应被丢弃");
        assert_eq!(normalized[0].title, "B");
        assert_eq!(normalized[0].start_ms, 0);
        assert_eq!(normalized[1].title, "C");
        assert_eq!(normalized[1].start_ms, 10_000, "与前章重叠应后移");
        assert_eq!(normalized[2].title, "A");
        assert_eq!(normalized[2].end_ms, 120_000, "结束应裁剪到媒体时长");
        for pair in normalized.windows(2) {
            assert!(
                pair[0].end_ms <= pair[1].start_ms,
                "章节不得重叠：{:?}",
                normalized
            );
        }
        for chapter in &normalized {
            assert!(chapter.start_ms >= 0);
            assert!(chapter.end_ms > chapter.start_ms);
            assert!(chapter.end_ms <= 120_000);
        }
    }

    #[tokio::test]
    async fn cache_hit_avoids_repeat_provider_calls_and_respects_language() {
        let temp = TempDir::new("summarize-cache");
        let cache = AiCache::new(temp.path()).with_max_entries(10);
        let doc = two_window_doc();
        let reply = json_reply(&[(0, 120_000, "标题", "摘要")], "", &[]);
        let (registry, mock) = setup(vec![
            reply.clone(),
            reply.clone(),
            reply.clone(),
            reply.clone(),
        ]);
        let opts = SummarizeOptions::default();

        let first = summarize_subtitles(&registry, &cache, "fp-cache", &doc, &opts).await;
        assert!(!first.cached);
        assert_eq!(mock.complete_calls(), 2);
        assert_eq!(cache.entry_count(), 1);

        let second = summarize_subtitles(&registry, &cache, "fp-cache", &doc, &opts).await;
        assert!(second.cached, "同指纹应命中缓存");
        assert_eq!(second.provider_id, None);
        assert_eq!(mock.complete_calls(), 2, "命中缓存不应再调 Provider");
        assert_eq!(
            second.summary.chapter_count(),
            first.summary.chapter_count()
        );

        // 语言变化 → 缓存键不同 → 重新调用。
        let opts_en = opts.clone().with_target_lang("en");
        let third = summarize_subtitles(&registry, &cache, "fp-cache", &doc, &opts_en).await;
        assert!(!third.cached);
        assert_eq!(mock.complete_calls(), 4);

        // 指纹变化 → 失效。
        let fourth = summarize_subtitles(&registry, &cache, "fp-other", &doc, &opts).await;
        assert!(!fourth.cached);
        assert_eq!(mock.complete_calls(), 6);
    }

    #[tokio::test]
    async fn empty_subtitle_returns_empty_summary_without_calls() {
        let temp = TempDir::new("summarize-empty");
        let cache = AiCache::new(temp.path());
        let doc = doc_with(vec![cue(0, "   "), cue(1_000, "")]);
        let (registry, mock) = setup(vec![]);
        let outcome =
            summarize_subtitles(&registry, &cache, "fp", &doc, &SummarizeOptions::default()).await;
        assert_eq!(mock.complete_calls(), 0);
        assert_eq!(outcome.windows, 0);
        assert!(outcome.summary.is_empty());
        assert!(!outcome.degraded);
        assert!(!outcome.has_warning());
    }

    #[tokio::test]
    async fn cross_hour_timeline_keeps_absolute_ms() {
        let temp = TempDir::new("summarize-cross-hour");
        let cache = AiCache::new(temp.path());
        let doc = doc_with(vec![cue(3_900_000, "一小时后第一句")]);
        let reply = json_reply(
            &[(3_900_000, 3_960_000, "一小时后", "后段内容")],
            "总览",
            &[],
        );
        let (registry, _mock) = setup(vec![reply]);
        let outcome =
            summarize_subtitles(&registry, &cache, "", &doc, &SummarizeOptions::default()).await;
        assert_eq!(outcome.chapter_count(), 1);
        assert_eq!(outcome.summary.chapters[0].start_ms, 3_900_000);
        assert_eq!(format_hms(outcome.summary.chapters[0].start_ms), "01:05:00");
        let markdown = summary_to_markdown(&outcome.summary);
        assert!(markdown.contains("01:05:00"), "markdown 时间轴：{markdown}");
    }

    #[test]
    fn markdown_contains_overview_keywords_and_chapters() {
        let summary = AiSummary {
            chapters: vec![
                Chapter {
                    start_ms: 60_000,
                    end_ms: 120_000,
                    title: "第一章".into(),
                    summary: "内容一".into(),
                },
                Chapter {
                    start_ms: 120_000,
                    end_ms: 180_000,
                    title: String::new(),
                    summary: String::new(),
                },
            ],
            overview: "整片总览".into(),
            keywords: vec!["关键词A".into(), "关键词B".into()],
        };
        let markdown = summary_to_markdown(&summary);
        assert!(markdown.starts_with("# 视频摘要"));
        assert!(markdown.contains("**总览**：整片总览"));
        assert!(markdown.contains("**关键词**：关键词A、关键词B"));
        assert!(markdown.contains("## 章节"));
        assert!(markdown.contains("`[00:01:00–00:02:00]` **第一章**：内容一"));
        assert!(markdown.contains("未命名章节"));
    }

    #[tokio::test]
    async fn empty_json_chapters_falls_back_to_first_sentence() {
        let temp = TempDir::new("summarize-empty-json");
        let cache = AiCache::new(temp.path());
        let doc = doc_with(vec![cue(0, "首句内容")]);
        let reply = serde_json::json!({
            "chapters": [],
            "overview": "只有总览",
            "keywords": []
        })
        .to_string();
        let (registry, _mock) = setup(vec![reply]);
        let outcome =
            summarize_subtitles(&registry, &cache, "", &doc, &SummarizeOptions::default()).await;
        assert!(!outcome.degraded);
        assert_eq!(outcome.chapter_count(), 1);
        assert_eq!(outcome.summary.chapters[0].title, "首句内容");
        assert!(
            outcome.summary.overview.starts_with("只有总览"),
            "总览应保留：{}",
            outcome.summary.overview
        );
        assert!(
            outcome.warnings.iter().any(|w| w.contains("未解析出章节")),
            "{:?}",
            outcome.warnings
        );
    }

    #[test]
    fn summary_json_roundtrip_and_lookup() {
        let summary = AiSummary {
            chapters: vec![Chapter {
                start_ms: 0,
                end_ms: 60_000,
                title: "标题".into(),
                summary: "摘要".into(),
            }],
            overview: "总览".into(),
            keywords: vec!["k".into()],
        };
        let json = summary.to_json_pretty().expect("序列化");
        assert!(json.contains("\"start_ms\""));
        assert!(json.contains("\"chapters\""));
        let parsed = AiSummary::from_json(&json).expect("反序列化");
        assert_eq!(parsed, summary);
        assert!(parsed.find_chapter(30_000).is_some());
        assert!(parsed.find_chapter(60_000).is_none(), "[start,end) 边界");
        assert_eq!(parsed.chapters[0].duration_ms(), 60_000);
    }
}
