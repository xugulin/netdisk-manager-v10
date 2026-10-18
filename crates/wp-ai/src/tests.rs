//! wp-ai 单元测试：**不依赖外网** —— HTTP Provider 用本地 `tokio::net::TcpListener` mock。
//!
//! 覆盖：请求体/鉴权头/模型字段、正常解析、分批拼接、解析容错、500 重试与最终失败、
//! 超时、401 鉴权分类、非法 JSON、URL 归一化、优先级选择、优先级回退、offline_only、
//! 缓存命中/失效/LRU/原子写、字幕翻译失败降级、译文 SubtitleDoc、任务队列（成功/取消/
//! 排队取消/超时/优先级顺序/播放期策略/文本生成）。

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

use wp_core::ErrorKind;
use wp_subtitle::{Cue, SubtitleDoc, SubtitleFormat};

use crate::cache::{fingerprint_file_metadata, AiCache, CacheKey};
use crate::mock::MockProvider;
use crate::pipeline::{build_translated_doc, translate_subtitle_document};
use crate::provider::{build_provider_registry, translate_with_fallback, ProviderRegistry};
use crate::remote::{
    chat_completions_url, models_url, parse_translation_reply, RemoteOpenAiProvider, RemoteOptions,
};
use crate::service::{AiEvent, AiOutcome, AiResult, AiService, AiServiceConfig, TaskSpec};
use crate::{
    is_playback_rejected, select_provider, AiConfig, AiProvider, AiTaskKind, ProviderConfig,
    ProviderKind, TranslateOptions,
};

// ---------------------------------------------------------------------------
// 通用测试基建：本地 mock HTTP server
// ---------------------------------------------------------------------------

/// 捕获到的 HTTP 请求。
#[derive(Debug, Clone)]
pub(crate) struct CapturedRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl CapturedRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("mock server 捕获的请求体应为 JSON")
    }
}

/// 脚本化 HTTP 响应。
#[derive(Debug, Clone)]
pub(crate) struct TestResponse {
    status: u16,
    body: String,
    delay: Duration,
}

impl TestResponse {
    pub(crate) fn ok_json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    pub(crate) fn status(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    pub(crate) fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

type Responder = Arc<dyn Fn(usize, &CapturedRequest) -> TestResponse + Send + Sync>;

/// 本地 mock HTTP server（每条连接读一个请求、返回脚本化响应）。
pub(crate) struct MockServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    handle: JoinHandle<()>,
}

impl MockServer {
    pub(crate) async fn start(responder: Responder) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("mock server 绑定 127.0.0.1:0");
        let addr = listener.local_addr().expect("mock server 本地地址");
        let requests: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let requests_for_server = requests.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _peer)) = listener.accept().await else {
                    break;
                };
                let responder = responder.clone();
                let requests = requests_for_server.clone();
                tokio::spawn(async move {
                    let Some((head, body)) = read_http_request(&mut stream).await else {
                        return;
                    };
                    let request = parse_http_request(&head, &body);
                    let index = {
                        let mut list = requests.lock().expect("mock requests 锁");
                        list.push(request.clone());
                        list.len() - 1
                    };
                    let response = responder(index, &request);
                    if !response.delay.is_zero() {
                        tokio::time::sleep(response.delay).await;
                    }
                    let raw = build_http_response(response.status, &response.body);
                    let _ = stream.write_all(raw.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        Self {
            addr,
            requests,
            handle,
        }
    }

    /// 按顺序返回给定响应（超出脚本的请求返回 500）。
    pub(crate) async fn scripted(responses: Vec<TestResponse>) -> Self {
        let queue: Arc<Mutex<VecDeque<TestResponse>>> =
            Arc::new(Mutex::new(VecDeque::from(responses)));
        Self::start(Arc::new(move |_index, _request| {
            queue
                .lock()
                .expect("mock 脚本队列锁")
                .pop_front()
                .unwrap_or_else(|| TestResponse::status(500, "mock: 没有更多脚本化响应"))
        }))
        .await
    }

    /// OpenAI 兼容 base_url（含 `/v1`）。
    pub(crate) fn url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    pub(crate) fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().expect("mock requests 锁").clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn read_http_request(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut buffer: Vec<u8> = Vec::with_capacity(4096);
    let header_end = loop {
        if let Some(pos) = find_subsequence(&buffer, b"\r\n\r\n") {
            break pos;
        }
        let mut chunk = [0u8; 2048];
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
        if buffer.len() > 1 << 20 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let mut chunk = [0u8; 2048];
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Some((head, String::from_utf8_lossy(&body).to_string()))
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_http_request(head: &str, body: &str) -> CapturedRequest {
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    CapturedRequest {
        method,
        path,
        headers,
        body: body.to_string(),
    }
}

fn build_http_response(status: u16, body: &str) -> String {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Mock",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

// ---------------------------------------------------------------------------
// 通用测试数据 / 配置 helpers
// ---------------------------------------------------------------------------

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 自带清理的临时目录。
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub(crate) fn new(tag: &str) -> Self {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("wp-ai-test-{}-{}-{}", std::process::id(), tag, n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("创建测试临时目录");
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub(crate) fn cue(start_ms: i64, end_ms: i64, text: &str) -> Cue {
    Cue {
        start_ms,
        end_ms,
        text: text.to_string(),
        raw_style: None,
    }
}

pub(crate) fn make_cues(n: usize) -> Vec<Cue> {
    (0..n)
        .map(|i| {
            let start = i as i64 * 1000;
            cue(start, start + 1000, &format!("第{i}句"))
        })
        .collect()
}

pub(crate) fn make_doc(n: usize) -> SubtitleDoc {
    SubtitleDoc {
        format: Some(SubtitleFormat::Srt),
        cues: make_cues(n),
        ass_header: None,
    }
}

pub(crate) fn openai_response(content: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }]
    })
    .to_string()
}

pub(crate) fn provider_cfg(
    id: &str,
    kind: ProviderKind,
    enabled: bool,
    priority: u8,
) -> ProviderConfig {
    ProviderConfig {
        id: id.to_string(),
        kind,
        enabled,
        priority,
        ..ProviderConfig::default()
    }
}

pub(crate) fn ai_config(
    providers: Vec<ProviderConfig>,
    offline_only: bool,
    allow_during_playback: bool,
) -> AiConfig {
    AiConfig {
        providers,
        offline_only,
        allow_during_playback,
    }
}

pub(crate) fn cfg_and_registry(
    provider: Arc<dyn AiProvider>,
    priority: u8,
) -> (AiConfig, ProviderRegistry) {
    let cfg = ai_config(
        vec![provider_cfg(provider.id(), provider.kind(), true, priority)],
        true,
        false,
    );
    let mut registry = ProviderRegistry::from_config(&cfg);
    registry.register(provider);
    (cfg, registry)
}

pub(crate) fn remote_opts(batch_size: usize, max_retries: u32) -> RemoteOptions {
    RemoteOptions {
        timeout: Duration::from_secs(5),
        max_retries,
        batch_size,
        retry_backoff: Duration::from_millis(1),
        temperature: None,
        max_tokens: None,
        no_proxy: true,
    }
}

pub(crate) fn remote_cfg(
    server: &MockServer,
    batch_size: usize,
    max_retries: u32,
) -> ProviderConfig {
    ProviderConfig {
        id: "remote-test".into(),
        kind: ProviderKind::RemoteOpenAi,
        enabled: true,
        priority: 10,
        base_url: server.url(),
        model: "test-model".into(),
        api_key: "sk-test".into(),
        timeout_secs: 5,
        max_retries,
        batch_size,
        ..ProviderConfig::default()
    }
}

pub(crate) fn remote_provider(
    server: &MockServer,
    batch_size: usize,
    max_retries: u32,
) -> RemoteOpenAiProvider {
    let cfg = remote_cfg(server, batch_size, max_retries);
    RemoteOpenAiProvider::with_options(&cfg, remote_opts(batch_size, max_retries))
        .expect("构造远端 Provider")
}

pub(crate) async fn next_event(events: &mut UnboundedReceiver<AiEvent>) -> AiEvent {
    tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("等待 AI 事件超时")
        .expect("AI 事件通道关闭")
}

// ---------------------------------------------------------------------------
// 远端 OpenAI Provider：请求 / 解析 / 分批 / 重试 / 超时 / 错误分类
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remote_translate_sends_request_headers_model_and_parses() {
    let server = MockServer::scripted(vec![TestResponse::ok_json(openai_response(
        "0|你好\n1|世界",
    ))])
    .await;
    let provider = remote_provider(&server, 20, 0);
    let cues = vec![cue(1_000, 3_000, "Hello"), cue(4_000, 6_000, "World")];
    let opts = TranslateOptions::new("en", "zh");
    let out = provider
        .translate_cues(&cues, &opts)
        .await
        .expect("翻译应成功");
    assert_eq!(out, vec!["你好".to_string(), "世界".to_string()]);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(request.header("authorization"), Some("Bearer sk-test"));

    let body = request.json();
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["role"], "user");
    let user = body["messages"][1]["content"].as_str().unwrap_or_default();
    assert!(
        user.contains("0|00:00:01,000 --> 00:00:03,000|Hello"),
        "prompt 应保留编号+时间轴：{user}"
    );
    assert!(
        user.contains("1|00:00:04,000 --> 00:00:06,000|World"),
        "prompt 应包含第二句：{user}"
    );
    assert!(user.contains("目标语言：简体中文"), "prompt 应含目标语言");
}

#[tokio::test]
async fn remote_translate_batches_and_concatenates() {
    let server = MockServer::scripted(vec![
        TestResponse::ok_json(openai_response("0|甲0\n1|甲1")),
        TestResponse::ok_json(openai_response("2|甲2\n3|甲3")),
        TestResponse::ok_json(openai_response("4|甲4")),
    ])
    .await;
    let provider = remote_provider(&server, 2, 0);
    let cues = make_cues(5);
    let out = provider
        .translate_cues(&cues, &TranslateOptions::default())
        .await
        .expect("分批翻译应成功");
    assert_eq!(out, vec!["甲0", "甲1", "甲2", "甲3", "甲4"]);

    let requests = server.requests();
    assert_eq!(requests.len(), 3, "5 条/每批 2 条 = 3 次请求");
    let user_batch2 = requests[1].json()["messages"][1]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        user_batch2.contains("2|00:00:02,000"),
        "第二批应从全局编号 2 开始"
    );
    let user_batch3 = requests[2].json()["messages"][1]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        user_batch3.contains("4|00:00:04,000"),
        "第三批应只有全局编号 4"
    );
}

#[tokio::test]
async fn remote_retries_500_then_succeeds() {
    let server = MockServer::scripted(vec![
        TestResponse::status(500, "{\"error\":{\"message\":\"overloaded\"}}"),
        TestResponse::ok_json(openai_response("0|重试成功")),
    ])
    .await;
    let provider = remote_provider(&server, 20, 2);
    let out = provider
        .translate_cues(&[cue(0, 1000, "Hi")], &TranslateOptions::default())
        .await
        .expect("500 后重试应成功");
    assert_eq!(out, vec!["重试成功".to_string()]);
    assert_eq!(server.requests().len(), 2, "应恰好重试一次");
}

#[tokio::test]
async fn remote_500_final_failure_is_server_error_and_transient() {
    let server = MockServer::scripted(vec![
        TestResponse::status(500, "{\"error\":\"boom-1\"}"),
        TestResponse::status(500, "{\"error\":\"boom-2\"}"),
        TestResponse::status(500, "{\"error\":\"boom-3\"}"),
    ])
    .await;
    let provider = remote_provider(&server, 20, 2);
    let err = provider
        .translate_cues(&[cue(0, 1000, "Hi")], &TranslateOptions::default())
        .await
        .expect_err("持续 500 应最终失败");
    assert_eq!(err.kind(), ErrorKind::Server);
    assert_eq!(server.requests().len(), 3, "max_retries=2 → 共 3 次尝试");
    assert!(err.to_string().contains("500"), "错误应含状态码：{err}");
    assert!(crate::remote::is_transient_error(&err), "500 应可重试");
}

#[tokio::test]
async fn remote_timeout_maps_to_network_error() {
    let server = MockServer::scripted(vec![
        TestResponse::ok_json(openai_response("0|太慢了")).with_delay(Duration::from_secs(5))
    ])
    .await;
    let cfg = remote_cfg(&server, 20, 0);
    let mut opts = remote_opts(20, 0);
    opts.timeout = Duration::from_millis(150);
    let provider = RemoteOpenAiProvider::with_options(&cfg, opts).expect("构造远端 Provider");
    let err = provider
        .translate_cues(&[cue(0, 1000, "Hi")], &TranslateOptions::default())
        .await
        .expect_err("超时应失败");
    assert_eq!(err.kind(), ErrorKind::Network);
    assert!(err.to_string().contains("超时"), "错误应为超时：{err}");
}

#[tokio::test]
async fn remote_401_is_auth_error_without_retry() {
    let server = MockServer::scripted(vec![TestResponse::status(
        401,
        "{\"error\":{\"message\":\"invalid api key\"}}",
    )])
    .await;
    let provider = remote_provider(&server, 20, 2);
    let err = provider
        .translate_cues(&[cue(0, 1000, "Hi")], &TranslateOptions::default())
        .await
        .expect_err("401 应失败");
    assert_eq!(err.kind(), ErrorKind::Auth);
    assert_eq!(server.requests().len(), 1, "鉴权错误不应重试");
    assert!(
        !err.to_string().contains("sk-test"),
        "错误信息不得包含 api_key"
    );
}

#[tokio::test]
async fn remote_bad_json_is_parse_error() {
    let server = MockServer::scripted(vec![TestResponse::ok_json("这不是 JSON 响应")]).await;
    let provider = remote_provider(&server, 20, 0);
    let err = provider
        .translate_cues(&[cue(0, 1000, "Hi")], &TranslateOptions::default())
        .await
        .expect_err("非法 JSON 应失败");
    assert_eq!(err.kind(), ErrorKind::Other);
    assert!(
        err.to_string().contains("JSON"),
        "错误应为 JSON 解析：{err}"
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn remote_translate_fills_missing_lines_with_original() {
    let server = MockServer::scripted(vec![TestResponse::ok_json(openai_response(
        "0|只译了第一句",
    ))])
    .await;
    let provider = remote_provider(&server, 20, 0);
    let cues = make_cues(2);
    let out = provider
        .translate_cues(&cues, &TranslateOptions::default())
        .await
        .expect("部分解析应容错成功");
    assert_eq!(out[0], "只译了第一句");
    assert_eq!(out[1], cues[1].text, "缺失行应回退原文");
}

#[test]
fn parse_translation_reply_is_tolerant() {
    let reply = "好的，以下是翻译：\n```\n0 | 你好\n1、世界\n[2] 第三句\n3. 第四句\n第4条：第五句\n0|重复忽略\n99|越界忽略\n00:00:00 --> 00:00:01|回声忽略\n```";
    let parsed = parse_translation_reply(reply, &[0, 1, 2, 3, 4]);
    assert_eq!(
        parsed,
        vec![
            (0usize, "你好".to_string()),
            (1, "世界".to_string()),
            (2, "第三句".to_string()),
            (3, "第四句".to_string()),
            (4, "第五句".to_string()),
        ]
    );
}

#[test]
fn urls_are_normalized_for_openai_compatible_endpoints() {
    assert_eq!(
        chat_completions_url("https://api.openai.com/v1"),
        "https://api.openai.com/v1/chat/completions"
    );
    assert_eq!(
        chat_completions_url("https://api.deepseek.com"),
        "https://api.deepseek.com/v1/chat/completions"
    );
    assert_eq!(
        chat_completions_url("http://127.0.0.1:8080/v1/"),
        "http://127.0.0.1:8080/v1/chat/completions"
    );
    assert_eq!(
        models_url("https://api.openai.com/v1"),
        "https://api.openai.com/v1/models"
    );
}

#[tokio::test]
async fn local_health_check_false_when_not_ready() {
    // 本地 LLM：指向必定拒绝连接的端口 → 健康检查 false（不依赖真实服务）。
    let llm = ProviderConfig::local(
        "llm",
        ProviderKind::LocalLlm,
        "http://127.0.0.1:1/v1",
        "qwen.gguf",
    );
    assert!(!crate::health_check(&llm).await);
    // 本地 Whisper：CLI 不存在 → false（任何 feature 下都成立）。
    let mut whisper = ProviderConfig::local("whisper", ProviderKind::LocalWhisper, "", "model.bin");
    whisper.cli_path = Some("/definitely/not/a/whisper-cli".to_string());
    assert!(!crate::health_check(&whisper).await);
}

// ---------------------------------------------------------------------------
// 选择 / 优先级 / 回退 / offline_only
// ---------------------------------------------------------------------------

#[test]
fn select_provider_filters_disabled_and_sorts_by_priority() {
    let cfg = ai_config(
        vec![
            provider_cfg("a-disabled", ProviderKind::RemoteOpenAi, false, 1),
            provider_cfg("b-remote", ProviderKind::RemoteOpenAi, true, 30),
            provider_cfg("c-remote", ProviderKind::RemoteOpenAi, true, 10),
            provider_cfg("d-local", ProviderKind::LocalLlm, true, 1),
        ],
        false,
        false,
    );
    assert_eq!(
        select_provider(&cfg, ProviderKind::RemoteOpenAi).map(|p| p.id.as_str()),
        Some("c-remote")
    );
    assert_eq!(
        select_provider(&cfg, ProviderKind::LocalLlm).map(|p| p.id.as_str()),
        Some("d-local")
    );
    assert!(select_provider(&cfg, ProviderKind::LocalWhisper).is_none());
}

#[tokio::test]
async fn fallback_uses_next_provider_when_priority_one_fails() {
    let remote = Arc::new(
        MockProvider::new("remote")
            .with_kind(ProviderKind::RemoteOpenAi)
            .always_fail(),
    );
    let local = Arc::new(MockProvider::new("local").with_translations(vec!["备用译文".into()]));
    let cfg = ai_config(
        vec![
            provider_cfg("remote", ProviderKind::RemoteOpenAi, true, 1),
            provider_cfg("local", ProviderKind::LocalLlm, true, 50),
        ],
        false,
        false,
    );
    let mut registry = ProviderRegistry::from_config(&cfg);
    registry.register(remote.clone());
    registry.register(local.clone());
    let (translations, provider_id) = translate_with_fallback(
        &registry,
        AiTaskKind::TranslateSubtitles,
        &make_cues(1),
        &TranslateOptions::default(),
    )
    .await
    .expect("第二优先级应兜底成功");
    assert_eq!(provider_id, "local");
    assert_eq!(translations, vec!["备用译文".to_string()]);
    assert_eq!(remote.translate_calls(), 1);
    assert_eq!(local.translate_calls(), 1);
}

#[tokio::test]
async fn offline_only_skips_remote_even_if_enabled() {
    let remote = Arc::new(
        MockProvider::new("remote")
            .with_kind(ProviderKind::RemoteOpenAi)
            .with_translations(vec!["远端译文".into()]),
    );
    let local = Arc::new(MockProvider::new("local").with_translations(vec!["本地译文".into()]));
    let cfg = ai_config(
        vec![
            provider_cfg("remote", ProviderKind::RemoteOpenAi, true, 1),
            provider_cfg("local", ProviderKind::LocalLlm, true, 50),
        ],
        true,
        false,
    );
    let mut registry = ProviderRegistry::from_config(&cfg);
    registry.register(remote.clone());
    registry.register(local.clone());
    assert!(select_provider(&cfg, ProviderKind::RemoteOpenAi).is_none());
    let (translations, provider_id) = translate_with_fallback(
        &registry,
        AiTaskKind::TranslateSubtitles,
        &make_cues(1),
        &TranslateOptions::default(),
    )
    .await
    .expect("本地应可用");
    assert_eq!(provider_id, "local");
    assert_eq!(translations, vec!["本地译文".to_string()]);
    assert_eq!(remote.translate_calls(), 0, "offline_only 不得调用远端");
}

// ---------------------------------------------------------------------------
// 缓存
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pipeline_cache_hit_and_invalidation() {
    let temp = TempDir::new("cache-hit");
    let cache = AiCache::new(temp.path()).with_max_entries(10);
    let mock = Arc::new(
        MockProvider::new("mock").with_translations(vec!["译A".to_string(), "译B".to_string()]),
    );
    let provider: Arc<dyn AiProvider> = mock.clone();
    let (_cfg, registry) = cfg_and_registry(provider, 10);
    let doc = make_doc(2);
    let opts = TranslateOptions::default();

    let first = translate_subtitle_document(&registry, &cache, "file-fp-1", &doc, &opts).await;
    assert!(!first.cached && !first.degraded);
    assert_eq!(
        first.translations,
        vec!["译A".to_string(), "译B".to_string()]
    );
    assert_eq!(mock.translate_calls(), 1);
    assert_eq!(cache.entry_count(), 1);

    let second = translate_subtitle_document(&registry, &cache, "file-fp-1", &doc, &opts).await;
    assert!(second.cached, "同指纹+语言+模型应命中缓存");
    assert_eq!(second.provider_id, None);
    assert_eq!(mock.translate_calls(), 1, "命中缓存不应调用 Provider");

    // 指纹变化 → 缓存失效。
    let third = translate_subtitle_document(&registry, &cache, "file-fp-2", &doc, &opts).await;
    assert!(!third.cached);
    assert_eq!(mock.translate_calls(), 2);

    // 目标语言变化 → 缓存失效。
    let opts_en = TranslateOptions::new("auto", "en");
    let fourth = translate_subtitle_document(&registry, &cache, "file-fp-2", &doc, &opts_en).await;
    assert!(!fourth.cached);
    assert_eq!(mock.translate_calls(), 3);
}

#[test]
fn cache_lru_evicts_oldest() {
    let temp = TempDir::new("cache-lru");
    let cache = AiCache::new(temp.path()).with_max_entries(2);
    let kind = AiTaskKind::TranslateSubtitles;
    for (i, fingerprint) in ["fp-1", "fp-2", "fp-3"].iter().enumerate() {
        let key = CacheKey::new(*fingerprint, "m", "zh", kind);
        cache
            .put_translations(&key, &[format!("v{i}")])
            .expect("写缓存");
        std::thread::sleep(Duration::from_millis(20)); // 保证 mtime 可排序
    }
    assert!(cache.entry_count() <= 2, "条目数应受上限约束");
    let oldest = CacheKey::new("fp-1", "m", "zh", kind);
    assert!(
        cache.get_translations(&oldest).is_none(),
        "最旧条目应被淘汰"
    );
    let newest = CacheKey::new("fp-3", "m", "zh", kind);
    assert_eq!(
        cache.get_translations(&newest),
        Some(vec!["v2".to_string()])
    );
}

#[test]
fn cache_file_is_atomic_json_and_corruption_is_miss() {
    let temp = TempDir::new("cache-json");
    let cache = AiCache::new(temp.path()).with_max_entries(10);
    let key = CacheKey::new(
        "fp-atomic",
        "deepseek-chat",
        "zh",
        AiTaskKind::TranslateSubtitles,
    );
    cache
        .put_translations(&key, &["你好".to_string()])
        .expect("写缓存");
    let path = cache.entry_path(&key);
    assert!(path.exists(), "缓存文件应存在");
    assert_eq!(
        path.parent().and_then(|p| p.to_str()),
        Some(temp.path().to_str().unwrap())
    );
    let raw = std::fs::read_to_string(&path).expect("读缓存文件");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("缓存文件应为合法 JSON");
    assert_eq!(value["key"]["file_fingerprint"], "fp-atomic");
    assert_eq!(value["key"]["model"], "deepseek-chat");
    assert_eq!(value["key"]["target_lang"], "zh");
    assert!(value["value"].is_array());
    assert!(
        !std::fs::read_dir(temp.path())
            .expect("列目录")
            .any(|entry| entry
                .map(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
                .unwrap_or(false)),
        "原子写不应遗留临时文件"
    );

    // 损坏条目 → 视为未命中（不阻塞翻译）。
    std::fs::write(&path, "not a json").expect("写坏缓存");
    assert!(cache.get_translations(&key).is_none());
}

#[test]
fn fingerprint_is_stable_and_deterministic() {
    let a = fingerprint_file_metadata(1024, 1_700_000_000_000);
    let b = fingerprint_file_metadata(1024, 1_700_000_000_000);
    let c = fingerprint_file_metadata(1025, 1_700_000_000_000);
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_eq!(a.len(), 64, "sha256 hex 长度");
}

// ---------------------------------------------------------------------------
// 翻译管线：失败降级 / 译文 Cue
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pipeline_degrades_to_original_when_all_providers_fail() {
    let temp = TempDir::new("pipeline-degrade");
    let cache = AiCache::new(temp.path());
    let failing = Arc::new(MockProvider::new("fail").always_fail());
    let provider: Arc<dyn AiProvider> = failing.clone();
    let (_cfg, registry) = cfg_and_registry(provider, 10);
    let doc = make_doc(2);
    let outcome = translate_subtitle_document(
        &registry,
        &cache,
        "fp-degrade",
        &doc,
        &TranslateOptions::default(),
    )
    .await;
    assert!(outcome.degraded, "Provider 全失败应降级而非报错");
    assert!(outcome.has_warning());
    assert_eq!(
        outcome.translations,
        vec!["第0句".to_string(), "第1句".to_string()],
        "降级应返回原文"
    );
    assert!(
        outcome.errors[0].contains("AI 翻译失败"),
        "errors 应标记失败原因：{:?}",
        outcome.errors
    );
    assert_eq!(failing.translate_calls(), 1, "单 Provider 只失败一次");
}

#[test]
fn build_translated_doc_keeps_timeline_and_raw_style() {
    let mut doc = make_doc(2);
    doc.cues[0].raw_style = Some("{\\an8}".to_string());
    let translations = vec!["你好".to_string(), String::new()];
    let translated = build_translated_doc(&doc, &translations);
    assert_eq!(translated.cues.len(), 2);
    assert_eq!(translated.cues[0].text, "你好");
    assert_eq!(translated.cues[0].start_ms, doc.cues[0].start_ms);
    assert_eq!(translated.cues[0].end_ms, doc.cues[0].end_ms);
    assert_eq!(
        translated.cues[0].raw_style.as_deref(),
        Some("{\\an8}"),
        "原样式应保留"
    );
    assert_eq!(translated.cues[1].text, doc.cues[1].text, "空译文回退原文");
    assert_eq!(translated.format, doc.format);
}

// ---------------------------------------------------------------------------
// 任务队列：成功 / 取消 / 超时 / 优先级 / 播放期策略 / 文本生成
// ---------------------------------------------------------------------------

#[tokio::test]
async fn service_translate_success_then_cache_hit() {
    let temp = TempDir::new("svc-cache");
    let mock = Arc::new(
        MockProvider::new("mock")
            .with_translations(vec!["服务译0".to_string(), "服务译1".to_string()]),
    );
    let provider: Arc<dyn AiProvider> = mock.clone();
    let (ai, registry) = cfg_and_registry(provider, 10);
    let config = AiServiceConfig::new(ai)
        .with_cache_dir(temp.path())
        .with_task_timeout(Duration::from_secs(5));
    let (service, mut events) = AiService::spawn(config, registry).expect("启动 AI 服务");

    let id = service
        .submit_subtitle_translation(&make_doc(2), TranslateOptions::default(), "svc-fp")
        .expect("提交翻译任务");
    let event = next_event(&mut events).await;
    assert_eq!(event.task_id, id);
    match event.outcome {
        AiOutcome::Done(AiResult::Translations {
            translations,
            provider_id,
            cached,
        }) => {
            assert!(!cached);
            assert_eq!(
                translations,
                vec!["服务译0".to_string(), "服务译1".to_string()]
            );
            assert_eq!(provider_id.as_deref(), Some("mock"));
        }
        other => panic!("期望翻译成功，实际：{other:?}"),
    }
    assert_eq!(mock.translate_calls(), 1);
    assert_eq!(service.cache().entry_count(), 1);

    let id2 = service
        .submit_subtitle_translation(&make_doc(2), TranslateOptions::default(), "svc-fp")
        .expect("提交第二个翻译任务");
    let event2 = next_event(&mut events).await;
    assert_eq!(event2.task_id, id2);
    match event2.outcome {
        AiOutcome::Done(AiResult::Translations {
            cached,
            provider_id,
            translations,
        }) => {
            assert!(cached, "二次请求应命中缓存");
            assert_eq!(provider_id, None);
            assert_eq!(translations.len(), 2);
        }
        other => panic!("期望缓存命中，实际：{other:?}"),
    }
    assert_eq!(mock.translate_calls(), 1, "缓存命中不应再调用 Provider");
}

#[tokio::test]
async fn service_cancel_inflight_task() {
    let temp = TempDir::new("svc-cancel");
    let mock = Arc::new(MockProvider::new("mock").with_delay(Duration::from_secs(3)));
    let provider: Arc<dyn AiProvider> = mock.clone();
    let (ai, registry) = cfg_and_registry(provider, 10);
    let config = AiServiceConfig::new(ai)
        .with_cache_dir(temp.path())
        .with_task_timeout(Duration::from_secs(30));
    let (service, mut events) = AiService::spawn(config, registry).expect("启动 AI 服务");
    let id = service
        .submit(TaskSpec::translate_subtitles(
            make_cues(1),
            TranslateOptions::default(),
            None,
        ))
        .expect("提交任务");
    tokio::time::sleep(Duration::from_millis(50)).await;
    service.cancel(id).expect("取消任务");
    let event = next_event(&mut events).await;
    assert_eq!(event.task_id, id);
    assert!(
        matches!(event.outcome, AiOutcome::Canceled),
        "应回传取消：{:?}",
        event.outcome
    );
}

#[tokio::test]
async fn service_cancel_queued_task_while_another_runs() {
    let temp = TempDir::new("svc-cancel-queued");
    let mock = Arc::new(
        MockProvider::new("mock")
            .with_delay(Duration::from_millis(150))
            .with_translations(vec!["译文".into()]),
    );
    let provider: Arc<dyn AiProvider> = mock.clone();
    let (ai, registry) = cfg_and_registry(provider, 10);
    let config = AiServiceConfig::new(ai)
        .with_cache_dir(temp.path())
        .with_max_concurrent(1)
        .with_task_timeout(Duration::from_secs(10));
    let (service, mut events) = AiService::spawn(config, registry).expect("启动 AI 服务");
    let running = service
        .submit(TaskSpec::translate_subtitles(
            make_cues(1),
            TranslateOptions::default(),
            None,
        ))
        .expect("提交执行中任务");
    tokio::time::sleep(Duration::from_millis(20)).await;
    let queued = service
        .submit(TaskSpec::translate_subtitles(
            make_cues(1),
            TranslateOptions::default(),
            None,
        ))
        .expect("提交排队任务");
    service.cancel(queued).expect("取消排队任务");

    let mut seen = Vec::new();
    while seen.len() < 2 {
        seen.push(next_event(&mut events).await);
    }
    let done = seen
        .iter()
        .find(|e| e.task_id == running)
        .expect("应有执行中任务的事件");
    assert!(
        done.outcome.is_done(),
        "先提交的任务应完成：{:?}",
        done.outcome
    );
    let canceled = seen
        .iter()
        .find(|e| e.task_id == queued)
        .expect("应有排队任务的事件");
    assert!(
        matches!(canceled.outcome, AiOutcome::Canceled),
        "排队任务应被取消：{:?}",
        canceled.outcome
    );
}

#[tokio::test]
async fn service_task_timeout_reports_failure() {
    let temp = TempDir::new("svc-timeout");
    let mock = Arc::new(MockProvider::new("mock").with_delay(Duration::from_secs(5)));
    let provider: Arc<dyn AiProvider> = mock.clone();
    let (ai, registry) = cfg_and_registry(provider, 10);
    let config = AiServiceConfig::new(ai)
        .with_cache_dir(temp.path())
        .with_task_timeout(Duration::from_millis(150));
    let (service, mut events) = AiService::spawn(config, registry).expect("启动 AI 服务");
    let id = service
        .submit(TaskSpec::translate_subtitles(
            make_cues(1),
            TranslateOptions::default(),
            None,
        ))
        .expect("提交任务");
    let event = next_event(&mut events).await;
    assert_eq!(event.task_id, id);
    match event.outcome {
        AiOutcome::Failed { message, retryable } => {
            assert!(message.contains("超时"), "应报告超时：{message}");
            assert!(retryable, "超时应标记可重试");
        }
        other => panic!("期望超时失败，实际：{other:?}"),
    }
}

#[tokio::test]
async fn service_priority_order_low_number_first() {
    let temp = TempDir::new("svc-priority");
    let mock = Arc::new(
        MockProvider::new("mock")
            .with_delay(Duration::from_millis(120))
            .with_translations(vec!["译文".into()]),
    );
    let provider: Arc<dyn AiProvider> = mock.clone();
    let (ai, registry) = cfg_and_registry(provider, 10);
    let config = AiServiceConfig::new(ai)
        .with_cache_dir(temp.path())
        .with_max_concurrent(1)
        .with_task_timeout(Duration::from_secs(10));
    let (service, mut events) = AiService::spawn(config, registry).expect("启动 AI 服务");
    let first = service
        .submit(
            TaskSpec::translate_subtitles(make_cues(1), TranslateOptions::default(), None)
                .with_priority(100),
        )
        .expect("提交任务 A");
    tokio::time::sleep(Duration::from_millis(20)).await;
    let low = service
        .submit(
            TaskSpec::translate_subtitles(make_cues(1), TranslateOptions::default(), None)
                .with_priority(200),
        )
        .expect("提交低优先级任务");
    let high = service
        .submit(
            TaskSpec::translate_subtitles(make_cues(1), TranslateOptions::default(), None)
                .with_priority(1),
        )
        .expect("提交高优先级任务");
    let order: Vec<u64> = vec![
        next_event(&mut events).await.task_id,
        next_event(&mut events).await.task_id,
        next_event(&mut events).await.task_id,
    ];
    assert_eq!(
        order,
        vec![first, high, low],
        "优先级数值小者应先执行（同优先级 FIFO）"
    );
}

#[tokio::test]
async fn service_rejects_non_urgent_task_during_playback() {
    let temp = TempDir::new("svc-playback");
    let mock = Arc::new(MockProvider::new("mock").with_translations(vec!["译文".into()]));
    let provider: Arc<dyn AiProvider> = mock.clone();
    let (ai, registry) = cfg_and_registry(provider, 10);
    let config = AiServiceConfig::new(ai)
        .with_cache_dir(temp.path())
        .with_task_timeout(Duration::from_secs(5));
    let (service, mut events) = AiService::spawn(config, registry).expect("启动 AI 服务");

    service.set_playing(true);
    assert!(service.is_playing());
    let err = service
        .submit(TaskSpec::translate_subtitles(
            make_cues(1),
            TranslateOptions::default(),
            None,
        ))
        .expect_err("播放期默认应拒绝");
    assert!(
        is_playback_rejected(&err),
        "错误应可识别为播放期拒绝：{err}"
    );
    assert!(err.to_string().contains("播放期间"), "错误应明确：{err}");

    // 紧急任务（实时字幕前瞻）放行。
    let urgent = service
        .submit(
            TaskSpec::translate_subtitles(make_cues(1), TranslateOptions::default(), None)
                .with_urgent(true),
        )
        .expect("紧急任务应放行");
    let event = next_event(&mut events).await;
    assert_eq!(event.task_id, urgent);
    assert!(event.outcome.is_done());

    // 暂停后恢复正常提交。
    service.set_playing(false);
    service
        .submit(TaskSpec::translate_subtitles(
            make_cues(1),
            TranslateOptions::default(),
            None,
        ))
        .expect("暂停后应允许提交");
}

#[tokio::test]
async fn service_allow_during_playback_config_bypasses_rejection() {
    let temp = TempDir::new("svc-allow-playback");
    let mock = Arc::new(MockProvider::new("mock").with_translations(vec!["译文".into()]));
    let provider: Arc<dyn AiProvider> = mock.clone();
    let (mut ai, registry) = cfg_and_registry(provider, 10);
    ai.allow_during_playback = true;
    let config = AiServiceConfig::new(ai)
        .with_cache_dir(temp.path())
        .with_task_timeout(Duration::from_secs(5));
    let (service, mut events) = AiService::spawn(config, registry).expect("启动 AI 服务");
    service.set_playing(true);
    let id = service
        .submit(TaskSpec::translate_subtitles(
            make_cues(1),
            TranslateOptions::default(),
            None,
        ))
        .expect("开启 allow_during_playback 后播放期应允许");
    let event = next_event(&mut events).await;
    assert_eq!(event.task_id, id);
    assert!(event.outcome.is_done());
}

#[tokio::test]
async fn service_complete_task_returns_text() {
    let temp = TempDir::new("svc-complete");
    let mock = Arc::new(MockProvider::new("mock"));
    let provider: Arc<dyn AiProvider> = mock.clone();
    let (ai, registry) = cfg_and_registry(provider, 10);
    let config = AiServiceConfig::new(ai)
        .with_cache_dir(temp.path())
        .with_task_timeout(Duration::from_secs(5));
    let (service, mut events) = AiService::spawn(config, registry).expect("启动 AI 服务");
    let id = service
        .submit_complete("总结一下字幕")
        .expect("提交文本任务");
    let event = next_event(&mut events).await;
    assert_eq!(event.task_id, id);
    match event.outcome {
        AiOutcome::Done(AiResult::Text { text, provider_id }) => {
            assert_eq!(text, "Mock 摘要结果");
            assert_eq!(provider_id, "mock");
        }
        other => panic!("期望文本生成成功，实际：{other:?}"),
    }
}

#[test]
fn build_registry_warns_for_unavailable_or_misconfigured_providers() {
    let ai = ai_config(
        vec![
            provider_cfg("local-list", ProviderKind::LocalLlm, true, 10),
            ProviderConfig::remote_openai(
                "remote-empty-model",
                "https://api.example.com/v1",
                "",
                "k",
            ),
        ],
        false,
        false,
    );
    let (registry, warnings) = build_provider_registry(&ai);
    assert!(!warnings.is_empty(), "应有配置/能力警告");
    assert!(
        warnings.iter().any(|w| w.contains("remote-empty-model")),
        "远端缺 model 应告警：{warnings:?}"
    );
    #[cfg(not(feature = "local-llm"))]
    assert!(
        warnings.iter().any(|w| w.contains("local-llm")),
        "未编译本地 feature 应告警：{warnings:?}"
    );
    // 远端 Provider 仍会被构造并注册（后续真实调用再报错/回退）。
    assert!(registry.is_registered("remote-empty-model"));
}
