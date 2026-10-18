//! T7 新增测试：本地 LLM（OpenAI 兼容本地服务）/ 本地 ASR（伪 whisper-cli 脚本）/ ASR 管线。
//!
//! - 默认 features：ASR 管线与队列测试（用进程内 `AsrMock` Provider，不联网）；
//! - `--all-features`：额外的 `local-llm`（本地 mock HTTP）与 `local-whisper`
//!   （伪造 `whisper-cli` shell 脚本）测试；仍不依赖真实模型/外网。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use wp_core::{Error, Result};
use wp_subtitle::{SubtitleDoc, SubtitleFormat};

use crate::cache::AiCache;
use crate::pipeline::{transcribe_file_to_subtitle, transcribe_to_subtitle};
use crate::service::{AiOutcome, AiResult, AiService, AiServiceConfig, TaskSpec};
use crate::tests::{cfg_and_registry, make_cues, next_event, TempDir};
use crate::{AiProvider, ProviderKind};

// ---------------------------------------------------------------------------
// 通用：ASR Mock Provider / 音频文件 helper
// ---------------------------------------------------------------------------

fn asr_doc(n: usize) -> SubtitleDoc {
    SubtitleDoc {
        format: Some(SubtitleFormat::Srt),
        cues: make_cues(n),
        ass_header: None,
    }
}

fn write_audio(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("写测试音频文件");
    path
}

/// 进程内 ASR Mock：可控延迟/失败，分别统计 `transcribe` 与 `transcribe_file` 调用次数。
struct AsrMock {
    id: String,
    doc: SubtitleDoc,
    fail: bool,
    delay: Duration,
    transcribe_count: Arc<AtomicUsize>,
    file_count: Arc<AtomicUsize>,
}

impl AsrMock {
    fn new(doc: SubtitleDoc) -> Arc<Self> {
        Arc::new(Self {
            id: "asr-mock".to_string(),
            doc,
            fail: false,
            delay: Duration::ZERO,
            transcribe_count: Arc::new(AtomicUsize::new(0)),
            file_count: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            id: "asr-mock".to_string(),
            doc: SubtitleDoc::default(),
            fail: true,
            delay: Duration::ZERO,
            transcribe_count: Arc::new(AtomicUsize::new(0)),
            file_count: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn transcribe_calls(&self) -> usize {
        self.transcribe_count.load(Ordering::SeqCst)
    }

    fn file_calls(&self) -> usize {
        self.file_count.load(Ordering::SeqCst)
    }

    async fn respond(&self) -> Result<SubtitleDoc> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        if self.fail {
            return Err(Error::network("Mock ASR 转写失败"));
        }
        Ok(self.doc.clone())
    }
}

#[async_trait]
impl AiProvider for AsrMock {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::LocalWhisper
    }

    async fn transcribe(&self, _audio: &[u8], _lang: &str) -> Result<SubtitleDoc> {
        self.transcribe_count.fetch_add(1, Ordering::SeqCst);
        self.respond().await
    }

    async fn transcribe_file(&self, _path: &Path, _lang: &str) -> Result<SubtitleDoc> {
        self.file_count.fetch_add(1, Ordering::SeqCst);
        self.respond().await
    }
}

// ---------------------------------------------------------------------------
// ASR 管线（默认 features）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn transcribe_pipeline_caches_and_hits() {
    let temp = TempDir::new("asr-cache");
    let cache = AiCache::new(temp.path()).with_max_entries(10);
    let mock = AsrMock::new(asr_doc(2));
    let provider: Arc<dyn AiProvider> = mock.clone();
    let (_cfg, registry) = cfg_and_registry(provider, 10);
    let audio = b"RIFF....WAVEfmt ";

    let first = transcribe_to_subtitle(&registry, &cache, "asr-fp-1", audio, "zh").await;
    assert!(!first.cached && !first.degraded && first.is_complete());
    assert_eq!(first.cue_count(), 2);
    assert_eq!(first.provider_id.as_deref(), Some("asr-mock"));
    assert_eq!(mock.transcribe_calls(), 1);
    assert_eq!(cache.entry_count(), 1);

    let second = transcribe_to_subtitle(&registry, &cache, "asr-fp-1", audio, "zh").await;
    assert!(second.cached, "同指纹+语言+模型应命中缓存");
    assert_eq!(second.provider_id, None);
    assert_eq!(second.cue_count(), 2);
    assert_eq!(mock.transcribe_calls(), 1, "缓存命中不应再调 Provider");

    let third = transcribe_to_subtitle(&registry, &cache, "asr-fp-2", audio, "zh").await;
    assert!(!third.cached, "指纹变化应失效");
    assert_eq!(mock.transcribe_calls(), 2);
}

#[tokio::test]
async fn transcribe_pipeline_degrades_on_failure() {
    let temp = TempDir::new("asr-degrade");
    let cache = AiCache::new(temp.path());
    let mock = AsrMock::failing();
    let provider: Arc<dyn AiProvider> = mock.clone();
    let (_cfg, registry) = cfg_and_registry(provider, 10);
    let outcome = transcribe_to_subtitle(&registry, &cache, "asr-fail", b"RIFF....", "zh").await;
    assert!(outcome.degraded, "Provider 失败应降级");
    assert!(outcome.has_warning());
    assert_eq!(outcome.cue_count(), 0, "降级为空字幕文档");
    assert!(
        outcome.errors[0].contains("AI 转写失败"),
        "errors 应标记失败原因：{:?}",
        outcome.errors
    );
    assert_eq!(mock.transcribe_calls(), 1);
}

#[tokio::test]
async fn transcribe_file_pipeline_routes_to_transcribe_file() {
    let temp = TempDir::new("asr-file-route");
    let audio = write_audio(temp.path(), "a.wav", b"RIFF....WAVEfmt ");
    let mock = AsrMock::new(asr_doc(2));
    let provider: Arc<dyn AiProvider> = mock.clone();
    let (_cfg, registry) = cfg_and_registry(provider, 10);
    let cache = AiCache::new(temp.path());
    let outcome = transcribe_file_to_subtitle(&registry, &cache, "", &audio, "zh").await;
    assert!(!outcome.degraded && outcome.is_complete());
    assert_eq!(outcome.cue_count(), 2);
    assert_eq!(outcome.provider_id.as_deref(), Some("asr-mock"));
    assert_eq!(mock.file_calls(), 1, "文件路径应走 transcribe_file");
    assert_eq!(mock.transcribe_calls(), 0, "不应退化为读字节再 transcribe");
}

#[tokio::test]
async fn transcribe_file_pipeline_missing_file_degrades() {
    let temp = TempDir::new("asr-file-missing");
    let cache = AiCache::new(temp.path());
    let mock = AsrMock::new(asr_doc(1));
    let provider: Arc<dyn AiProvider> = mock.clone();
    let (_cfg, registry) = cfg_and_registry(provider, 10);
    let outcome = transcribe_file_to_subtitle(
        &registry,
        &cache,
        "",
        Path::new("/definitely/not/exists/audio.wav"),
        "zh",
    )
    .await;
    assert!(outcome.degraded);
    assert!(outcome.errors[0].contains("不存在"), "{:?}", outcome.errors);
    assert_eq!(mock.file_calls(), 0, "文件不存在不应调用 Provider");
}

#[tokio::test]
async fn service_transcribe_file_returns_subtitle_and_caches() {
    let temp = TempDir::new("asr-service");
    let audio = write_audio(temp.path(), "audio.wav", b"RIFF....WAVEfmt ");
    let mock = AsrMock::new(asr_doc(2));
    let provider: Arc<dyn AiProvider> = mock.clone();
    let (ai, registry) = cfg_and_registry(provider, 10);
    let config = AiServiceConfig::new(ai)
        .with_cache_dir(temp.path())
        .with_task_timeout(Duration::from_secs(5));
    let (service, mut events) = AiService::spawn(config, registry).expect("启动 AI 服务");

    let id = service
        .submit_transcribe_file(&audio, "zh", "asr-svc-fp")
        .expect("提交文件转写任务");
    let event = next_event(&mut events).await;
    assert_eq!(event.task_id, id);
    match event.outcome {
        AiOutcome::Done(AiResult::Subtitle {
            doc,
            provider_id,
            cached,
        }) => {
            assert!(!cached);
            assert_eq!(doc.cues.len(), 2);
            assert_eq!(provider_id.as_deref(), Some("asr-mock"));
        }
        other => panic!("期望转写成功，实际：{other:?}"),
    }
    assert_eq!(mock.file_calls(), 1);
    assert_eq!(service.cache().entry_count(), 1);

    let id2 = service
        .submit_transcribe_file(&audio, "zh", "asr-svc-fp")
        .expect("提交第二个文件转写任务");
    let event2 = next_event(&mut events).await;
    assert_eq!(event2.task_id, id2);
    match event2.outcome {
        AiOutcome::Done(AiResult::Subtitle {
            cached,
            provider_id,
            doc,
        }) => {
            assert!(cached, "二次转写应命中缓存");
            assert_eq!(provider_id, None);
            assert_eq!(doc.cues.len(), 2);
        }
        other => panic!("期望缓存命中，实际：{other:?}"),
    }
    assert_eq!(mock.file_calls(), 1, "缓存命中不应再调 Provider");
}

#[tokio::test]
async fn service_transcribe_failure_reports_failed_event() {
    let temp = TempDir::new("asr-service-fail");
    let mock = AsrMock::failing();
    let provider: Arc<dyn AiProvider> = mock.clone();
    let (ai, registry) = cfg_and_registry(provider, 10);
    let config = AiServiceConfig::new(ai)
        .with_cache_dir(temp.path())
        .with_task_timeout(Duration::from_secs(5));
    let (service, mut events) = AiService::spawn(config, registry).expect("启动 AI 服务");
    let id = service
        .submit(TaskSpec::transcribe(b"RIFF....".to_vec(), "zh"))
        .expect("提交转写任务");
    let event = next_event(&mut events).await;
    assert_eq!(event.task_id, id);
    match event.outcome {
        AiOutcome::Failed { message, .. } => {
            assert!(message.contains("转写失败"), "错误应可读：{message}");
        }
        other => panic!("期望失败事件，实际：{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 本地 LLM（OpenAI 兼容本地服务）—— 需要 feature `local-llm`
// ---------------------------------------------------------------------------

#[cfg(feature = "local-llm")]
mod local_llm_tests {
    use super::*;
    use crate::local_llm::{LocalLlmProvider, DEFAULT_LOCAL_BASE_URL, DEFAULT_LOCAL_MODEL};
    use crate::mock::MockProvider;
    use crate::provider::{
        build_provider, build_provider_registry, provider_statuses, translate_with_fallback,
    };
    use crate::tests::{ai_config, cue, openai_response, provider_cfg, MockServer, TestResponse};
    use crate::{AiTaskKind, ProviderConfig, TranslateOptions};

    fn local_cfg(base_url: &str) -> ProviderConfig {
        ProviderConfig {
            id: "local-llm-test".into(),
            kind: ProviderKind::LocalLlm,
            enabled: true,
            priority: 10,
            base_url: base_url.to_string(),
            model: "qwen2.5-7b-instruct".into(),
            ..ProviderConfig::default()
        }
    }

    #[test]
    fn defaults_are_filled_and_api_key_is_cleared() {
        let mut cfg = ProviderConfig {
            id: "local-llm-test".into(),
            kind: ProviderKind::LocalLlm,
            enabled: true,
            priority: 10,
            base_url: String::new(),
            model: String::new(),
            api_key: "should-be-dropped".into(),
            ..ProviderConfig::default()
        };
        cfg.api_key = "should-be-dropped".into();
        let provider = LocalLlmProvider::new(cfg).expect("构造本地 LLM Provider");
        assert_eq!(
            provider.resolved_base_url(),
            DEFAULT_LOCAL_BASE_URL.trim_end_matches('/')
        );
        assert_eq!(
            provider.chat_completions_url(),
            format!("{DEFAULT_LOCAL_BASE_URL}/chat/completions")
        );
        assert_eq!(provider.config().model, DEFAULT_LOCAL_MODEL);
        assert!(
            provider.config().api_key.is_empty(),
            "本地服务不应保留/发送 api_key"
        );
        assert!(provider.is_available());
    }

    #[tokio::test]
    async fn translate_uses_local_openai_compatible_endpoint_without_auth() {
        let server =
            MockServer::scripted(vec![TestResponse::ok_json(openai_response("0|本地译文"))]).await;
        let provider =
            LocalLlmProvider::new(local_cfg(&server.url())).expect("构造本地 LLM Provider");
        let cues = vec![cue(0, 1000, "Hello")];
        let out = provider
            .translate(&cues, &TranslateOptions::new("en", "zh"))
            .await
            .expect("本地 LLM 翻译应成功");
        assert_eq!(out, vec!["本地译文".to_string()]);

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/v1/chat/completions");
        assert!(
            requests[0].header("authorization").is_none(),
            "本地服务不应发送 Authorization"
        );
        assert_eq!(requests[0].json()["model"], "qwen2.5-7b-instruct");
    }

    #[tokio::test]
    async fn health_check_gets_models_endpoint() {
        let server = MockServer::scripted(vec![TestResponse::ok_json(
            "{\"data\":[{\"id\":\"qwen\"}]}",
        )])
        .await;
        let cfg = local_cfg(&server.url());
        assert!(crate::health_check(&cfg).await, "2xx 应视为可用");
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/v1/models");
        assert!(requests[0].header("authorization").is_none());
    }

    #[tokio::test]
    async fn health_check_false_on_http_error() {
        let server =
            MockServer::scripted(vec![TestResponse::status(500, "{\"error\":\"down\"}")]).await;
        let cfg = local_cfg(&server.url());
        assert!(!crate::health_check(&cfg).await);
    }

    #[test]
    fn build_registry_registers_local_llm_as_usable() {
        let ai = ai_config(
            vec![provider_cfg(
                "local-llm-test",
                ProviderKind::LocalLlm,
                true,
                10,
            )],
            true,
            false,
        );
        let (registry, warnings) = build_provider_registry(&ai);
        assert!(warnings.is_empty(), "本地 LLM 应可无警告构造：{warnings:?}");
        assert!(registry.is_registered("local-llm-test"));
        let statuses = provider_statuses(&registry);
        assert_eq!(statuses.len(), 1);
        assert!(statuses[0].usable && statuses[0].is_local);
        let provider = build_provider(&ai.providers[0]).expect("构造 Provider");
        assert_eq!(provider.kind(), ProviderKind::LocalLlm);
    }

    #[tokio::test]
    async fn fallback_uses_local_llm_when_remote_fails() {
        let server =
            MockServer::scripted(vec![TestResponse::ok_json(openai_response("0|本地兜底"))]).await;
        let ai = ai_config(
            vec![
                provider_cfg("remote-down", ProviderKind::RemoteOpenAi, true, 1),
                provider_cfg("local-llm-test", ProviderKind::LocalLlm, true, 10),
            ],
            false,
            false,
        );
        let mut registry = crate::provider::ProviderRegistry::from_config(&ai);
        let remote = Arc::new(
            MockProvider::new("remote-down")
                .with_kind(ProviderKind::RemoteOpenAi)
                .always_fail(),
        );
        registry.register(remote.clone());
        let mut local_cfg_value = ai.providers[1].clone();
        local_cfg_value.base_url = server.url();
        local_cfg_value.model = "qwen2.5-7b-instruct".into();
        registry.register(Arc::new(
            LocalLlmProvider::new(local_cfg_value).expect("构造本地 LLM Provider"),
        ));

        let (translations, provider_id) = translate_with_fallback(
            &registry,
            AiTaskKind::TranslateSubtitles,
            &[cue(0, 1000, "Hi")],
            &TranslateOptions::default(),
        )
        .await
        .expect("远端失败后本地应兜底");
        assert_eq!(provider_id, "local-llm-test");
        assert_eq!(translations, vec!["本地兜底".to_string()]);
        assert_eq!(remote.translate_calls(), 1);
    }
}

// ---------------------------------------------------------------------------
// 本地 Whisper（伪 whisper-cli 脚本）—— 需要 feature `local-whisper` + Unix
// ---------------------------------------------------------------------------

#[cfg(all(feature = "local-whisper", unix))]
mod whisper_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    use crate::local_whisper::{
        parse_srt_cues, parse_srt_timestamp, LocalWhisperProvider, WhisperOptions,
    };
    use crate::provider::ProviderRegistry;
    use crate::tests::{ai_config, TempDir};
    use crate::ProviderConfig;

    const SAMPLE_SRT: &str =
        "1\n00:00:00,000 --> 00:00:02,500\n你好世界\n\n2\n00:00:02,500 --> 00:00:05,000\n第二句\n";

    fn forge_cli(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-whisper-cli");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("写假 whisper-cli 脚本");
        let mut perms = std::fs::metadata(&path)
            .expect("读假 CLI 元数据")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod 假 CLI");
        path
    }

    fn fake_model(dir: &Path) -> PathBuf {
        write_audio(dir, "ggml-test.bin", b"fake-model")
    }

    /// 生成「记录参数 + 写 SRT」的假 CLI。
    fn srt_cli(dir: &Path) -> (PathBuf, PathBuf) {
        let args_file = dir.join("args.txt");
        let body = format!(
            "printf '%s\\n' \"$@\" > '{}'\nout=\"\"\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    -of) out=\"$2\"; shift 2;;\n    *) shift;;\n  esac\ndone\ncat > \"$out.srt\" <<'SRT'\n{SAMPLE_SRT}SRT",
            args_file.display()
        );
        (forge_cli(dir, &body), args_file)
    }

    fn whisper_provider(cli: &Path, model: &Path, timeout: Duration) -> LocalWhisperProvider {
        let cfg = ProviderConfig {
            id: "local-whisper-test".into(),
            kind: ProviderKind::LocalWhisper,
            enabled: true,
            priority: 10,
            model: model.display().to_string(),
            cli_path: Some(cli.display().to_string()),
            threads: 4,
            ..ProviderConfig::default()
        };
        LocalWhisperProvider::with_options(
            cfg,
            WhisperOptions {
                cli_path: cli.display().to_string(),
                model_path: model.display().to_string(),
                threads: 4,
                timeout,
                extra_args: vec![],
            },
        )
        .expect("构造本地 Whisper Provider")
    }

    #[test]
    fn parse_srt_helpers_are_tolerant() {
        assert_eq!(parse_srt_timestamp("01:02:03,456"), Some(3_723_456));
        assert_eq!(parse_srt_timestamp("00:00:02.5"), Some(2_500));
        assert_eq!(parse_srt_timestamp("00:02,250"), Some(2_250));
        assert_eq!(parse_srt_timestamp("bad"), None);
        let cues = parse_srt_cues("1\r\n00:00:01,000 --> 00:00:02,000\r\n第一行\r\n第二行\r\n\r\n00:00:03,000 --> 00:00:04,000\r\nSecond\r\n");
        assert_eq!(cues.len(), 2, "缺编号的块也应解析");
        assert_eq!(cues[0].start_ms, 1000);
        assert_eq!(cues[0].end_ms, 2000);
        assert_eq!(cues[0].text, "第一行\n第二行");
        assert_eq!(cues[1].text, "Second");
    }

    #[tokio::test]
    async fn transcribe_file_via_forged_cli() {
        let temp = TempDir::new("whisper-cli-ok");
        let (cli, args_file) = srt_cli(temp.path());
        let model = fake_model(temp.path());
        let audio = write_audio(temp.path(), "audio.wav", b"RIFF....WAVEfmt ");
        let provider = whisper_provider(&cli, &model, Duration::from_secs(10));

        let doc = provider
            .transcribe_path(&audio, "zh")
            .await
            .expect("假 CLI 转写应成功");
        assert_eq!(doc.cues.len(), 2);
        assert_eq!(doc.cues[0].start_ms, 0);
        assert_eq!(doc.cues[0].end_ms, 2500);
        assert_eq!(doc.cues[0].text, "你好世界");
        assert_eq!(doc.cues[1].text, "第二句");

        let args = std::fs::read_to_string(&args_file).expect("读取 CLI 参数记录");
        assert!(args.contains("-m"), "应传 -m：{args}");
        assert!(
            args.contains(&model.display().to_string()),
            "应传模型路径：{args}"
        );
        assert!(args.contains("-f"), "应传 -f：{args}");
        assert!(
            args.contains(&audio.display().to_string()),
            "应传音频路径：{args}"
        );
        assert!(args.lines().any(|line| line == "zh"), "应传 -l zh：{args}");
        assert!(args.contains("-t"), "应传 -t：{args}");
        assert!(args.lines().any(|line| line == "4"), "应传线程数 4：{args}");
        assert!(args.contains("-osrt"), "应要求 SRT 输出：{args}");
        assert!(args.contains("-of"), "应传 -of：{args}");
    }

    #[tokio::test]
    async fn transcribe_bytes_writes_temporary_wav() {
        let temp = TempDir::new("whisper-bytes");
        let (cli, args_file) = srt_cli(temp.path());
        let model = fake_model(temp.path());
        let provider = whisper_provider(&cli, &model, Duration::from_secs(10));
        let doc = provider
            .transcribe_bytes(b"RIFF....WAVEfmt ", "en")
            .await
            .expect("字节转写应成功");
        assert_eq!(doc.cues.len(), 2);
        let args = std::fs::read_to_string(&args_file).expect("读取 CLI 参数记录");
        assert!(
            args.contains(".wav"),
            "字节输入应按 RIFF 头写成 .wav 临时文件：{args}"
        );
        assert!(args.lines().any(|line| line == "en"));
    }

    #[tokio::test]
    async fn timeout_kills_process_and_reports() {
        let temp = TempDir::new("whisper-timeout");
        let cli = forge_cli(temp.path(), "sleep 5\nexit 0");
        let model = fake_model(temp.path());
        let audio = write_audio(temp.path(), "audio.wav", b"RIFF....");
        let provider = whisper_provider(&cli, &model, Duration::from_millis(200));
        let started = std::time::Instant::now();
        let err = provider
            .transcribe_path(&audio, "zh")
            .await
            .expect_err("应超时失败");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "超时应及时返回并 kill，实际耗时 {:?}",
            started.elapsed()
        );
        assert_eq!(err.kind(), wp_core::ErrorKind::Network);
        assert!(err.to_string().contains("超时"), "错误应可读：{err}");
    }

    #[tokio::test]
    async fn nonzero_exit_reports_stderr() {
        let temp = TempDir::new("whisper-exit");
        let cli = forge_cli(
            temp.path(),
            "echo 'model load failed: bad magic' >&2\nexit 3",
        );
        let model = fake_model(temp.path());
        let audio = write_audio(temp.path(), "audio.wav", b"RIFF....");
        let provider = whisper_provider(&cli, &model, Duration::from_secs(5));
        let err = provider
            .transcribe_path(&audio, "zh")
            .await
            .expect_err("非零退出应失败");
        let message = err.to_string();
        assert!(message.contains("退出码 3"), "应含退出码：{message}");
        assert!(
            message.contains("model load failed"),
            "应含 stderr：{message}"
        );
    }

    #[tokio::test]
    async fn missing_cli_reports_not_found() {
        let temp = TempDir::new("whisper-missing-cli");
        let model = fake_model(temp.path());
        let audio = write_audio(temp.path(), "audio.wav", b"RIFF....");
        let cfg = ProviderConfig::local("local-whisper-test", ProviderKind::LocalWhisper, "", "");
        let provider = LocalWhisperProvider::with_options(
            cfg,
            WhisperOptions {
                cli_path: "/definitely/not/whisper-cli".to_string(),
                model_path: model.display().to_string(),
                threads: 0,
                timeout: Duration::from_secs(5),
                extra_args: vec![],
            },
        )
        .expect("构造 Provider");
        let err = provider
            .transcribe_path(&audio, "zh")
            .await
            .expect_err("CLI 不存在应失败");
        assert_eq!(err.kind(), wp_core::ErrorKind::NotFound);
        assert!(
            err.to_string().contains("找不到 whisper-cli"),
            "错误应可读：{err}"
        );
    }

    #[tokio::test]
    async fn empty_output_is_readable_error() {
        let temp = TempDir::new("whisper-empty");
        let body = "out=\"\"\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    -of) out=\"$2\"; shift 2;;\n    *) shift;;\n  esac\ndone\n: > \"$out.srt\"";
        let cli = forge_cli(temp.path(), body);
        let model = fake_model(temp.path());
        let audio = write_audio(temp.path(), "audio.wav", b"RIFF....");
        let provider = whisper_provider(&cli, &model, Duration::from_secs(5));
        let err = provider
            .transcribe_path(&audio, "zh")
            .await
            .expect_err("空输出应报错");
        assert!(
            err.to_string().contains("没有字幕条目"),
            "错误应可读：{err}"
        );
    }

    #[tokio::test]
    async fn txt_fallback_returns_unsynced_cue() {
        let temp = TempDir::new("whisper-txt");
        let body = "out=\"\"\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    -of) out=\"$2\"; shift 2;;\n    *) shift;;\n  esac\ndone\nprintf '纯文本内容\\n' > \"$out.txt\"";
        let cli = forge_cli(temp.path(), body);
        let model = fake_model(temp.path());
        let audio = write_audio(temp.path(), "audio.wav", b"RIFF....");
        let provider = whisper_provider(&cli, &model, Duration::from_secs(5));
        let doc = provider
            .transcribe_path(&audio, "zh")
            .await
            .expect("txt 退化输出应可解析");
        assert_eq!(doc.cues.len(), 1);
        assert_eq!(doc.cues[0].text, "纯文本内容");
        assert_eq!(doc.cues[0].start_ms, 0);
    }

    #[tokio::test]
    async fn health_check_requires_cli_and_model() {
        let temp = TempDir::new("whisper-health");
        let (cli, _args) = srt_cli(temp.path());
        let model = fake_model(temp.path());
        let cfg = ProviderConfig {
            id: "local-whisper-test".into(),
            kind: ProviderKind::LocalWhisper,
            enabled: true,
            priority: 10,
            model: model.display().to_string(),
            cli_path: Some(cli.display().to_string()),
            ..ProviderConfig::default()
        };
        assert!(crate::health_check(&cfg).await);

        let mut missing_model = cfg.clone();
        missing_model.model = "/definitely/not/model.bin".to_string();
        assert!(!crate::health_check(&missing_model).await);

        let mut missing_cli = cfg;
        missing_cli.cli_path = Some("/definitely/not/whisper-cli".to_string());
        assert!(!crate::health_check(&missing_cli).await);
    }

    #[tokio::test]
    async fn service_transcribe_file_via_whisper_cli() {
        let temp = TempDir::new("whisper-service");
        let (cli, _args) = srt_cli(temp.path());
        let model = fake_model(temp.path());
        let audio = write_audio(temp.path(), "audio.wav", b"RIFF....WAVEfmt ");
        let provider = whisper_provider(&cli, &model, Duration::from_secs(10));
        let provider_cfg = provider.config().clone();
        let ai = ai_config(vec![provider_cfg], true, false);
        let mut registry = ProviderRegistry::from_config(&ai);
        registry.register(Arc::new(provider));
        let config = AiServiceConfig::new(ai)
            .with_cache_dir(temp.path())
            .with_task_timeout(Duration::from_secs(10));
        let (service, mut events) = AiService::spawn(config, registry).expect("启动 AI 服务");
        let id = service
            .submit_transcribe_file(&audio, "zh", "whisper-e2e-fp")
            .expect("提交文件转写任务");
        let event = next_event(&mut events).await;
        assert_eq!(event.task_id, id);
        match event.outcome {
            AiOutcome::Done(AiResult::Subtitle {
                doc,
                provider_id,
                cached,
            }) => {
                assert!(!cached);
                assert_eq!(doc.cues.len(), 2);
                assert_eq!(doc.cues[0].text, "你好世界");
                assert_eq!(provider_id.as_deref(), Some("local-whisper-test"));
            }
            other => panic!("期望 whisper 转写成功，实际：{other:?}"),
        }
    }
}
