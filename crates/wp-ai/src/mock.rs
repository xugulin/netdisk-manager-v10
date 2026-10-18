//! 测试/演示用 Provider：可注入固定译文、延迟、失败次数。
//!
//! 典型用法：
//! - 单测里替代真实 HTTP Provider（不联网、结果确定）；
//! - GUI 的「AI 面板」在未配置密钥时演示翻译流程；
//! - 验证优先级回退：让高优先级 Mock 失败、低优先级 Mock 成功。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use wp_core::{Error, Result};
use wp_subtitle::Cue;

use crate::{AiProvider, ProviderKind, TranslateOptions};

#[derive(Default)]
struct MockState {
    delay: Duration,
    always_fail: AtomicBool,
    fail_remaining: AtomicUsize,
    translations: Vec<String>,
    echo_prefix: String,
    translate_calls: AtomicUsize,
    complete_calls: AtomicUsize,
    transcribe_calls: AtomicUsize,
}

impl MockState {
    fn fail_message(&self) -> String {
        if self.always_fail.load(Ordering::SeqCst) {
            "Mock Provider 固定失败".to_string()
        } else {
            "Mock Provider 注入失败".to_string()
        }
    }
}

/// Mock Provider —— 不联网、行为可控。
pub struct MockProvider {
    id: String,
    kind: ProviderKind,
    state: Arc<MockState>,
}

impl MockProvider {
    /// 构造：默认本地 LLM 类型、无延迟、echo 原文并加 `[译]` 前缀。
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: ProviderKind::LocalLlm,
            state: Arc::new(MockState {
                echo_prefix: "[译]".into(),
                ..Default::default()
            }),
        }
    }

    fn state_mut(&mut self) -> &mut MockState {
        Arc::get_mut(&mut self.state).expect("MockProvider 已被共享，不能再配置")
    }

    /// 指定 Provider 类型（影响能力矩阵，例如设为 LocalWhisper 后不参与翻译候选）。
    pub fn with_kind(mut self, kind: ProviderKind) -> Self {
        self.kind = kind;
        self
    }

    /// 注入固定译文（按 cue 下标一一对应；不足部分用 echo 前缀 + 原文补齐）。
    pub fn with_translations(mut self, translations: Vec<String>) -> Self {
        self.state_mut().translations = translations;
        self
    }

    /// 每次调用前注入延迟（用于取消/超时/并发测试）。
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.state_mut().delay = delay;
        self
    }

    /// 前 `n` 次翻译调用失败（之后成功），用于验证回退/重试编排。
    pub fn fail_times(mut self, n: usize) -> Self {
        self.state_mut().fail_remaining = AtomicUsize::new(n);
        self
    }

    /// 永远失败（用于验证「所有 Provider 失败」的降级路径）。
    pub fn always_fail(mut self) -> Self {
        self.state_mut().always_fail = AtomicBool::new(true);
        self
    }

    /// 自定义 echo 前缀（默认 `[译]`）。
    pub fn with_echo_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.state_mut().echo_prefix = prefix.into();
        self
    }

    /// 已发生的翻译调用次数（测试断言用）。
    pub fn translate_calls(&self) -> usize {
        self.state.translate_calls.load(Ordering::SeqCst)
    }

    /// 已发生的文本生成调用次数。
    pub fn complete_calls(&self) -> usize {
        self.state.complete_calls.load(Ordering::SeqCst)
    }

    /// 已发生的转写调用次数。
    pub fn transcribe_calls(&self) -> usize {
        self.state.transcribe_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AiProvider for MockProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> ProviderKind {
        self.kind
    }

    async fn translate(&self, cues: &[Cue], _opts: &TranslateOptions) -> Result<Vec<String>> {
        self.state.translate_calls.fetch_add(1, Ordering::SeqCst);
        if !self.state.delay.is_zero() {
            tokio::time::sleep(self.state.delay).await;
        }
        let remaining = self.state.fail_remaining.load(Ordering::SeqCst);
        if remaining > 0 {
            self.state.fail_remaining.fetch_sub(1, Ordering::SeqCst);
            return Err(Error::network(self.state.fail_message()));
        }
        if self.state.always_fail.load(Ordering::SeqCst) {
            return Err(Error::network(self.state.fail_message()));
        }
        Ok(cues
            .iter()
            .enumerate()
            .map(|(i, cue)| {
                self.state
                    .translations
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| format!("{}{}", self.state.echo_prefix, cue.text))
            })
            .collect())
    }

    async fn complete(&self, _prompt: &str) -> Result<String> {
        self.state.complete_calls.fetch_add(1, Ordering::SeqCst);
        if !self.state.delay.is_zero() {
            tokio::time::sleep(self.state.delay).await;
        }
        if self.state.fail_remaining.load(Ordering::SeqCst) > 0
            || self.state.always_fail.load(Ordering::SeqCst)
        {
            return Err(Error::network(self.state.fail_message()));
        }
        Ok("Mock 摘要结果".to_string())
    }

    async fn transcribe(&self, _audio: &[u8], _lang: &str) -> Result<wp_subtitle::SubtitleDoc> {
        self.state.transcribe_calls.fetch_add(1, Ordering::SeqCst);
        if self.state.always_fail.load(Ordering::SeqCst) {
            return Err(Error::network(self.state.fail_message()));
        }
        Err(Error::unsupported("Mock Provider 未实现语音转写"))
    }
}
