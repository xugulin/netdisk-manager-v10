//! 远端 OpenAI 兼容 Provider（`POST {base_url}/chat/completions`）。
//!
//! 兼容 OpenAI / DeepSeek / Qwen(DashScope 兼容模式) / Ollama / vLLM / llama-server 等
//! 提供 OpenAI Chat Completions 协议的端点。配置示例：
//!
//! ```json
//! { "id": "deepseek", "kind": "RemoteOpenAi", "enabled": true, "priority": 20,
//!   "base_url": "https://api.deepseek.com/v1", "model": "deepseek-chat",
//!   "api_key": "sk-…", "timeout_secs": 60, "max_retries": 2, "batch_size": 20 }
//! ```
//!
//! 行为要点：
//! - **分批翻译**：每批默认 20 条（[`ProviderConfig::batch_size`]），prompt 带编号 + 时间轴，
//!   要求模型按 `编号|译文` 逐行输出；解析容错（多种编号格式、markdown 围栏、缺行用原文补）。
//! - **错误分类**（映射到 [`wp_core::Error`]）：
//!   | 情况 | 错误 | 是否重试 |
//!   |---|---|---|
//!   | 连接失败 / 超时 / 网络中断 | `Network` | 是 |
//!   | HTTP 401/403 | `Auth` | 否 |
//!   | HTTP 404 | `NotFound` | 否 |
//!   | HTTP 429 / 5xx | `Server{code}` | 是 |
//!   | 其它非 2xx | `Server{code}` | 否 |
//!   | 响应不是合法 JSON / 缺 choices | `Other`（解析错误） | 否 |
//! - **密钥安全**：`api_key` 只进 `Authorization: Bearer` 头；错误信息只含状态码/响应片段，
//!   不含请求头；[`ProviderConfig`] 的 `Debug` 已脱敏。
//! - **重试**：仅对瞬时错误（网络/超时/429/5xx）按指数退避重试 `max_retries` 次。

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use wp_core::{Error, ErrorKind, Result};
use wp_subtitle::{Cue, SubtitleDoc};

use crate::{AiProvider, ProviderConfig, ProviderKind, TranslateOptions};

/// 未配置 `base_url` 时的默认 OpenAI 端点。
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// 远端请求的可调参数。
#[derive(Debug, Clone)]
pub struct RemoteOptions {
    /// 单次 HTTP 请求总超时。
    pub timeout: Duration,
    /// 瞬时错误重试次数（0=不重试）。
    pub max_retries: u32,
    /// 字幕翻译分批大小。
    pub batch_size: usize,
    /// 重试退避基数（指数退避：base, 2*base, 4*base…）。
    pub retry_backoff: Duration,
    /// 采样温度（None=不发送该字段）。
    pub temperature: Option<f32>,
    /// 最大输出 token（None=不发送）。
    pub max_tokens: Option<u32>,
    /// 禁用系统代理（测试/本地端点用；默认 false=尊重系统代理）。
    pub no_proxy: bool,
}

impl Default for RemoteOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            max_retries: 2,
            batch_size: 20,
            retry_backoff: Duration::from_millis(500),
            temperature: Some(0.2),
            max_tokens: None,
            no_proxy: false,
        }
    }
}

impl RemoteOptions {
    /// 从 Provider 配置构造（timeout/max_retries/batch_size 取自配置）。
    pub fn from_config(cfg: &ProviderConfig) -> Self {
        Self {
            timeout: cfg.effective_timeout(),
            max_retries: cfg.max_retries,
            batch_size: cfg.effective_batch_size(),
            ..Self::default()
        }
    }

    /// 关闭系统代理（本地 mock/本地服务端点测试用）。
    pub fn without_proxy(mut self) -> Self {
        self.no_proxy = true;
        self
    }
}

/// OpenAI 兼容远端 Provider。
pub struct RemoteOpenAiProvider {
    cfg: ProviderConfig,
    client: reqwest::Client,
    opts: RemoteOptions,
}

impl RemoteOpenAiProvider {
    /// 用配置构造（超时/重试/分批大小来自 [`ProviderConfig`]）。
    pub fn from_config(cfg: &ProviderConfig) -> Result<Self> {
        Self::with_options(cfg, RemoteOptions::from_config(cfg))
    }

    /// 用自定义参数构造。
    pub fn with_options(cfg: &ProviderConfig, opts: RemoteOptions) -> Result<Self> {
        let mut builder = reqwest::Client::builder().timeout(opts.timeout);
        if opts.no_proxy {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|e| Error::other(format!("构建 AI HTTP 客户端失败：{e}")))?;
        Ok(Self {
            cfg: cfg.clone(),
            client,
            opts,
        })
    }

    /// 配置（`Debug` 已脱敏；不要打印内部字段到日志）。
    pub fn config(&self) -> &ProviderConfig {
        &self.cfg
    }

    /// 实际请求地址。
    pub fn chat_completions_url(&self) -> String {
        chat_completions_url(&self.cfg.base_url)
    }

    /// 实际分批大小。
    pub fn effective_batch_size(&self) -> usize {
        if self.opts.batch_size == 0 {
            20
        } else {
            self.opts.batch_size
        }
    }

    /// 分批翻译整段 cue；返回与 `cues` 等长的译文。
    ///
    /// 单批 HTTP 失败 → 整个调用返回错误（由上层回退到别的 Provider 或降级原文）；
    /// 单批内个别行解析缺失 → 该条用原文填充（容错，不中断）。
    pub async fn translate_cues(
        &self,
        cues: &[Cue],
        opts: &TranslateOptions,
    ) -> Result<Vec<String>> {
        if cues.is_empty() {
            return Ok(Vec::new());
        }
        let batch_size = self.effective_batch_size();
        let mut out: Vec<String> = cues.iter().map(|c| c.text.clone()).collect();
        for (batch_no, chunk) in cues.chunks(batch_size).enumerate() {
            let start = batch_no * batch_size;
            let indices: Vec<usize> = (start..start + chunk.len()).collect();
            let pairs = self.translate_batch(&indices, chunk, opts).await?;
            for (idx, text) in pairs {
                if let Some(slot) = out.get_mut(idx) {
                    *slot = text;
                }
            }
        }
        Ok(out)
    }

    /// 翻译一批 cue（`indices` 与 `cues` 一一对应，为全局下标）。
    /// 返回 `(全局下标, 译文)`；解析不出的行不返回（由 [`Self::translate_cues`] 用原文补）。
    pub async fn translate_batch(
        &self,
        indices: &[usize],
        cues: &[Cue],
        opts: &TranslateOptions,
    ) -> Result<Vec<(usize, String)>> {
        let (system, user) = build_translate_prompt(indices, cues, opts);
        let body = self.chat_body(Some(&system), &user);
        let reply = self.chat_with_retry(&body).await?;
        let pairs = parse_translation_reply(&reply, indices);
        if pairs.is_empty() {
            return Err(Error::other(format!(
                "AI 翻译输出无法解析（期望 {} 行 `编号|译文`；响应前 200 字符：{}）",
                indices.len(),
                truncate_chars(&reply, 200)
            )));
        }
        Ok(pairs)
    }

    /// 文本生成（P3 摘要/问答的最小实现）。
    pub async fn complete_prompt(&self, prompt: &str) -> Result<String> {
        let body = self.chat_body(None, prompt);
        self.chat_with_retry(&body).await
    }

    fn chat_body(&self, system: Option<&str>, user: &str) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        if let Some(system) = system {
            messages.push(json!({ "role": "system", "content": system }));
        }
        messages.push(json!({ "role": "user", "content": user }));
        let mut body = json!({
            "model": self.cfg.model,
            "messages": messages,
            "stream": false,
        });
        if let Some(t) = self.opts.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(mt) = self.opts.max_tokens {
            body["max_tokens"] = json!(mt);
        }
        body
    }

    async fn chat_with_retry(&self, body: &Value) -> Result<String> {
        let mut attempt: u32 = 0;
        loop {
            match self.post_chat(body).await {
                Ok(content) => return Ok(content),
                Err(err) if attempt < self.opts.max_retries && is_transient_error(&err) => {
                    attempt += 1;
                    let shift = (attempt - 1).min(5);
                    let backoff = self.opts.retry_backoff.saturating_mul(1u32 << shift);
                    if !backoff.is_zero() {
                        tokio::time::sleep(backoff).await;
                    }
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn post_chat(&self, body: &Value) -> Result<String> {
        let url = self.chat_completions_url();
        let mut request = self.client.post(&url);
        if self.cfg.has_api_key() {
            request = request.bearer_auth(self.cfg.api_key.trim());
        }
        let response = request
            .json(body)
            .send()
            .await
            .map_err(|e| map_reqwest_error(&e, self.opts.timeout))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .await
            .map_err(|e| map_reqwest_error(&e, self.opts.timeout))?;
        if !(200..=299).contains(&status) {
            return Err(map_http_status(status, &text));
        }
        parse_chat_response(&text)
    }
}

#[async_trait]
impl AiProvider for RemoteOpenAiProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::RemoteOpenAi
    }

    async fn translate(&self, cues: &[Cue], opts: &TranslateOptions) -> Result<Vec<String>> {
        self.translate_cues(cues, opts).await
    }

    async fn complete(&self, prompt: &str) -> Result<String> {
        self.complete_prompt(prompt).await
    }

    async fn transcribe(&self, _audio: &[u8], _lang: &str) -> Result<SubtitleDoc> {
        Err(Error::unsupported(
            "远端 OpenAI 兼容 ASR（/audio/transcriptions）尚未实现（P2 规划）；当前 v1 仅支持 chat/completions 翻译与文本生成",
        ))
    }
}

/// 归一化 `base_url`：去尾部 `/`；空值用默认；只有主机（无路径）时自动补 `/v1`。
pub fn normalize_base_url(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        DEFAULT_BASE_URL
    } else {
        base
    };
    if is_host_only(base) {
        format!("{base}/v1")
    } else {
        base.to_string()
    }
}

fn is_host_only(url: &str) -> bool {
    match url.split_once("://") {
        Some((_scheme, rest)) => !rest.contains('/') || rest.trim_end_matches('/').is_empty(),
        None => !url.contains('/'),
    }
}

/// 完整 chat completions 地址。
pub fn chat_completions_url(base_url: &str) -> String {
    let base = normalize_base_url(base_url);
    if base.ends_with("/chat/completions") {
        base
    } else {
        format!("{base}/chat/completions")
    }
}

/// 完整 models 地址（健康检查用）。
pub fn models_url(base_url: &str) -> String {
    let base = normalize_base_url(base_url);
    if base.ends_with("/models") {
        base
    } else {
        format!("{base}/models")
    }
}

/// 是否瞬时错误（值得重试）：网络/超时、HTTP 429、HTTP 5xx。
///
/// 注意与 [`wp_core::Error::retryable`] 的区别：后者把所有 `Server{code}` 都视为可重试，
/// 这里只认 429/5xx，避免对 400（参数错误）/401（鉴权）做无意义重试。
pub fn is_transient_error(err: &Error) -> bool {
    match err {
        Error::Network(_) => true,
        Error::Server { code, .. } => *code == 429 || (500..=599).contains(code),
        _ => false,
    }
}

/// 把 HTTP 状态码映射为统一错误（分类见模块文档表格）。
pub fn map_http_status(status: u16, body: &str) -> Error {
    let snippet = if body.trim().is_empty() {
        String::new()
    } else {
        format!("：{}", truncate_chars(body.trim(), 300))
    };
    match status {
        401 | 403 => Error::auth(format!("AI 鉴权失败（HTTP {status}）{snippet}")),
        404 => Error::not_found(format!(
            "AI 端点不存在（HTTP 404），请检查 base_url/模型名{snippet}"
        )),
        408 => Error::network(format!("AI 服务端请求超时（HTTP 408）{snippet}")),
        429 => Error::server(429, format!("AI 限流（HTTP 429，稍后重试）{snippet}")),
        s if (500..=599).contains(&s) => {
            Error::server(s as i64, format!("AI 服务端错误（HTTP {s}）{snippet}"))
        }
        s => Error::server(s as i64, format!("AI 请求失败（HTTP {s}）{snippet}")),
    }
}

fn map_reqwest_error(err: &reqwest::Error, timeout: Duration) -> Error {
    if err.is_timeout() {
        // 只保留秒数，不拼接请求详情（避免把 URL/请求头带进日志）。
        Error::network(format!(
            "AI 请求超时（{}s）：模型未在超时时间内响应",
            timeout.as_secs().max(1)
        ))
    } else if err.is_connect() {
        Error::network(format!("无法连接 AI 服务（连接失败/被拒绝）：{err}"))
    } else {
        Error::network(format!("AI 网络请求失败：{err}"))
    }
}

/// 解析 chat completions 响应，取 `choices[0].message.content`（兼容少见的 `choices[0].text`）。
pub fn parse_chat_response(body: &str) -> Result<String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| Error::other(format!("AI 响应不是合法 JSON：{e}")))?;
    if let Some(err) = value.get("error") {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| truncate_chars(&err.to_string(), 200));
        return Err(Error::other(format!("AI 服务返回错误：{message}")));
    }
    let content = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/choices/0/text").and_then(Value::as_str));
    match content {
        Some(content) if !content.trim().is_empty() => Ok(content.to_string()),
        Some(_) => Err(Error::other("AI 响应内容为空（choices[0] 无文本）")),
        None => Err(Error::other(format!(
            "AI 响应缺少 choices[0].message.content（前 200 字符：{}）",
            truncate_chars(body, 200)
        ))),
    }
}

/// 构造字幕翻译 prompt，返回 `(system, user)`。
///
/// user 每行格式：`全局编号|HH:MM:SS,mmm --> HH:MM:SS,mmm|原文`，
/// 要求模型按 `全局编号|译文` 输出，保证结果可与时间轴对齐。
pub fn build_translate_prompt(
    indices: &[usize],
    cues: &[Cue],
    opts: &TranslateOptions,
) -> (String, String) {
    let system = format!(
        "你是专业的字幕翻译引擎，把字幕逐条翻译成{}。规则：\n\
         1. 只输出译文，不解释、不总结、不合并或拆分条目；\n\
         2. 每条输出一行，格式严格为 `编号|译文`（编号取自输入行首）；\n\
         3. 不要输出时间轴、markdown 代码块、空行或其它前缀；\n\
         4. 译文简洁口语化、适合屏幕字幕；人名/专有名词/数字按术语表优先保留；\n\
         5. 即使某条无需翻译（纯数字/符号），也要输出对应编号行并原样给出内容。",
        opts.target_lang_name()
    );
    let mut user = String::new();
    user.push_str(&format!(
        "源语言：{}\n目标语言：{}\n",
        opts.source_lang_name(),
        opts.target_lang_name()
    ));
    if !opts.glossary.is_empty() {
        user.push_str("术语表（必须遵守，左侧原文 => 右侧固定译文）：\n");
        for (src, dst) in &opts.glossary {
            user.push_str(&format!("- {src} => {dst}\n"));
        }
    }
    user.push_str(&format!(
        "共 {} 条字幕，输入格式为 `编号|开始时间 --> 结束时间|原文`：\n",
        indices.len()
    ));
    for (i, cue) in cues.iter().enumerate() {
        let idx = indices.get(i).copied().unwrap_or(i);
        let text = cue.text.replace(['\r', '\n'], " ");
        user.push_str(&format!(
            "{}|{} --> {}|{}\n",
            idx,
            format_timestamp(cue.start_ms),
            format_timestamp(cue.end_ms),
            text
        ));
    }
    user.push_str("\n输出（每行 `编号|译文`）：");
    (system, user)
}

/// 毫秒 → `HH:MM:SS,mmm`（SRT 风格时间轴，仅用于 prompt 对齐）。
pub fn format_timestamp(ms: i64) -> String {
    let ms = ms.max(0);
    let hours = ms / 3_600_000;
    let minutes = (ms % 3_600_000) / 60_000;
    let seconds = (ms % 60_000) / 1_000;
    let millis = ms % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}

/// 解析模型返回的 `编号|译文` 行（容错多种编号格式）。
///
/// 容错点：markdown 围栏、列表符号、`[12]`/`第12条`/`12.`/`12:`/`12、` 等编号格式、
/// 回声的时间轴行（含 `-->`）直接忽略、非期望编号忽略、重复编号只取第一次。
pub fn parse_translation_reply(reply: &str, expected: &[usize]) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    for raw in reply.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("```") || line.contains("-->") {
            continue;
        }
        let Some((idx, text)) = parse_numbered_line(line) else {
            continue;
        };
        if !expected.contains(&idx) || text.is_empty() {
            continue;
        }
        if out.iter().any(|(existing, _)| *existing == idx) {
            continue;
        }
        out.push((idx, text));
    }
    out
}

fn parse_numbered_line(line: &str) -> Option<(usize, String)> {
    let line = line.trim_start_matches(['-', '*', '•', '·']).trim();
    let line = line.trim_start_matches(['[', '(', '{', '#']);
    let line = line.strip_prefix('第').unwrap_or(line);
    let digits_len = line.chars().take_while(char::is_ascii_digit).count();
    if digits_len == 0 || digits_len > 9 {
        return None;
    }
    let (num, rest) = line.split_at(digits_len);
    let idx: usize = num.parse().ok()?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('条').unwrap_or(rest);
    let rest = rest.trim_start();
    let rest = rest.trim_start_matches([
        '|', '：', ':', ']', '）', ')', '、', '.', '．', '-', '—', '▶', ' ',
    ]);
    let text = rest.trim();
    if text.is_empty() {
        return None;
    }
    Some((idx, text.to_string()))
}

fn truncate_chars(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(max).collect();
    format!("{cut}…")
}

/// 远端健康检查：`GET {base_url}/models`，2xx 视为可用。
pub async fn health_check(cfg: &ProviderConfig) -> bool {
    health_check_with_options(cfg, false).await
}

/// 健康检查（可指定是否绕过系统代理 —— 本地 Provider 的 `127.0.0.1` 端点必须绕过代理）。
pub async fn health_check_with_options(cfg: &ProviderConfig, no_proxy: bool) -> bool {
    if cfg.base_url.trim().is_empty() {
        return false;
    }
    let timeout = Duration::from_secs(cfg.effective_timeout().as_secs().clamp(1, 10));
    let mut builder = reqwest::Client::builder().timeout(timeout);
    if no_proxy {
        builder = builder.no_proxy();
    }
    let Ok(client) = builder.build() else {
        return false;
    };
    let mut request = client.get(models_url(&cfg.base_url));
    if cfg.has_api_key() {
        request = request.bearer_auth(cfg.api_key.trim());
    }
    matches!(request.send().await, Ok(resp) if resp.status().is_success())
}

/// 判断错误是否属于鉴权类（UI 提示「检查 API Key」）。
pub fn is_auth_error(err: &Error) -> bool {
    err.kind() == ErrorKind::Auth
}

/// 判断错误是否属于解析类（模型输出不可用，可提示重试/换模型）。
pub fn is_parse_error(err: &Error) -> bool {
    err.kind() == ErrorKind::Other && err.to_string().contains("AI 响应")
}
