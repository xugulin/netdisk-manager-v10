//! 本地 LLM Provider（feature `local-llm`）：走 **OpenAI 兼容的本地推理服务**。
//!
//! 支持 llama.cpp 的 `llama-server`、vLLM、Ollama、LM Studio、text-generation-webui 等
//! 暴露 `POST {base_url}/chat/completions` 的本地服务 —— 字幕内容不出本机（配合
//! [`crate::AiConfig::offline_only`] 可完全离线）。复用 [`crate::remote`] 的 HTTP/解析/重试逻辑，
//! 但按 [`crate::ProviderKind::LocalLlm`] 注册，因此可参与「本地 vs 远端」的启停/优先级/回退。
//!
//! # 启动本地服务（三选一）
//!
//! **llama.cpp（推荐，C/C++ 无 Python 依赖）**
//! ```text
//! git clone https://github.com/ggml-org/llama.cpp && cd llama.cpp
//! cmake -B build && cmake --build build -j
//! ./build/bin/llama-server -m qwen2.5-7b-instruct-q4_k_m.gguf --port 8080 -c 8192 -ngl 99
//! ```
//! → `base_url = "http://127.0.0.1:8080/v1"`，`model` 与 `-m` 文件名对应即可。
//!
//! **Ollama（最省事）**
//! ```text
//! ollama serve
//! ollama pull qwen2.5:7b
//! ```
//! → `base_url = "http://127.0.0.1:11434/v1"`，`model = "qwen2.5:7b"`。
//!
//! **vLLM（有 NVIDIA GPU 时吞吐最好）**
//! ```text
//! python -m vllm.entrypoints.openai.api_server --model Qwen/Qwen2.5-7B-Instruct --port 8000
//! ```
//! → `base_url = "http://127.0.0.1:8000/v1"`，`model = "Qwen/Qwen2.5-7B-Instruct"`。
//!
//! # 配置示例
//! ```json
//! { "id": "local-llama", "kind": "LocalLlm", "enabled": true, "priority": 10,
//!   "base_url": "http://127.0.0.1:8080/v1", "model": "qwen2.5-7b-instruct",
//!   "timeout_secs": 300, "batch_size": 20 }
//! ```
//! - `base_url` 留空 → 默认 `http://127.0.0.1:8080/v1`；`model` 留空 → `local-model`
//!   （仅 llama-server 可忽略模型名；vLLM/Ollama 会校验，务必填对）。
//! - `api_key` 会被清空（本地服务不需要，也避免误发到本地日志）。
//! - 本地 CPU 推理慢：建议 `timeout_secs = 300`；`max_retries` 默认 2。
//! - 连接失败（服务未启动）→ `Network` 错误，按优先级回退到下一个 Provider（如远端）。
//! - 健康检查：`GET {base_url}/models`，**绕过系统代理**（127.0.0.1 不能走代理）。
//!
//! # 进阶：进程内 llama.cpp 绑定（未做）
//! 可后续引入 `llama-cpp-2` 在专用线程/独立 worker 进程推理（架构 §7 的进程隔离目标）。

use async_trait::async_trait;
use std::time::Duration;
use wp_core::{Error, Result};
use wp_subtitle::{Cue, SubtitleDoc};

use crate::remote::{health_check_with_options, models_url, RemoteOpenAiProvider, RemoteOptions};
use crate::{AiProvider, ProviderConfig, ProviderKind, TranslateOptions};

/// 本地 LLM 服务默认地址（llama-server 默认端口）。
pub const DEFAULT_LOCAL_BASE_URL: &str = "http://127.0.0.1:8080/v1";

/// `model` 未配置时的兜底名（llama-server 可忽略；vLLM/Ollama 请务必配置正确模型名）。
pub const DEFAULT_LOCAL_MODEL: &str = "local-model";

/// 本地 LLM Provider 是否已实现真实推理链路。
pub const IMPLEMENTED: bool = true;

/// 本地 LLM Provider（OpenAI 兼容本地服务）。
pub struct LocalLlmProvider {
    cfg: ProviderConfig,
    inner: RemoteOpenAiProvider,
}

impl LocalLlmProvider {
    /// 用配置构造（自动补默认 base_url/model、清空 api_key、绕过系统代理）。
    pub fn new(cfg: ProviderConfig) -> Result<Self> {
        let mut opts = RemoteOptions::from_config(&cfg);
        opts.no_proxy = true;
        Self::with_options(cfg, opts)
    }

    /// 用自定义 HTTP 参数构造（测试/高级场景）。
    pub fn with_options(cfg: ProviderConfig, opts: RemoteOptions) -> Result<Self> {
        let mut cfg = cfg;
        if cfg.base_url.trim().is_empty() {
            cfg.base_url = DEFAULT_LOCAL_BASE_URL.to_string();
        }
        if cfg.model.trim().is_empty() {
            cfg.model = DEFAULT_LOCAL_MODEL.to_string();
        }
        // 本地服务不需要密钥：清空避免误发/误展示。
        cfg.api_key.clear();
        let inner = RemoteOpenAiProvider::with_options(&cfg, opts)?;
        Ok(Self { cfg, inner })
    }

    /// 配置（`Debug` 已脱敏）。
    pub fn config(&self) -> &ProviderConfig {
        &self.cfg
    }

    /// 归一化后的 base_url（补 `/v1` 后的地址）。
    pub fn resolved_base_url(&self) -> String {
        crate::remote::normalize_base_url(&self.cfg.base_url)
    }

    /// 实际 chat completions 地址。
    pub fn chat_completions_url(&self) -> String {
        self.inner.chat_completions_url()
    }

    /// models 地址（健康检查用）。
    pub fn models_url(&self) -> String {
        models_url(&self.cfg.base_url)
    }

    /// 本地服务是否可探测（仅表示实现存在；真实可用性用 [`Self::health_check`]）。
    pub fn is_available(&self) -> bool {
        IMPLEMENTED
    }

    /// 健康检查（GET `{base_url}/models`，绕过系统代理）。
    pub async fn health_check(&self) -> bool {
        health_check(&self.cfg).await
    }

    /// 实测连接耗时（UI 诊断用；失败返回 None）。
    pub async fn probe_latency(&self) -> Option<Duration> {
        let started = std::time::Instant::now();
        health_check(&self.cfg).await.then(|| started.elapsed())
    }
}

/// 本地 LLM 健康检查：`GET /v1/models` 2xx 即可用（llama-server/vLLM/Ollama 均支持）。
pub async fn health_check(cfg: &ProviderConfig) -> bool {
    let mut cfg = cfg.clone();
    if cfg.base_url.trim().is_empty() {
        cfg.base_url = DEFAULT_LOCAL_BASE_URL.to_string();
    }
    health_check_with_options(&cfg, true).await
}

#[async_trait]
impl AiProvider for LocalLlmProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::LocalLlm
    }

    async fn translate(&self, cues: &[Cue], opts: &TranslateOptions) -> Result<Vec<String>> {
        self.inner.translate_cues(cues, opts).await
    }

    async fn transcribe(&self, _audio: &[u8], _lang: &str) -> Result<SubtitleDoc> {
        Err(Error::unsupported(
            "本地 LLM 只做翻译/文本生成，不支持语音转写；ASR 请配置 LocalWhisper（whisper.cpp）Provider",
        ))
    }

    async fn complete(&self, prompt: &str) -> Result<String> {
        self.inner.complete_prompt(prompt).await
    }
}
