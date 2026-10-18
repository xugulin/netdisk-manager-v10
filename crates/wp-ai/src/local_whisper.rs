//! 本地 ASR Provider（feature `local-whisper`）：调用 **whisper.cpp 的 `whisper-cli` 外部进程**。
//!
//! 设计：进程隔离调用（符合架构 §7「播放优先」—— ASR 跑在子进程，超时可 kill，不阻塞 UI；
//! 播放期间由 [`crate::AiConfig::allow_during_playback`] / 任务 `urgent` 策略控制）。
//! 本模块不链接任何 whisper 库、不需要模型参与编译，只依赖外部可执行文件 + ggml 模型文件。
//!
//! # 接入真 whisper.cpp 的步骤
//!
//! 1. 构建（Linux/macOS；需要 cmake + C++ 工具链）：
//!    ```text
//!    git clone https://github.com/ggml-org/whisper.cpp && cd whisper.cpp
//!    cmake -B build && cmake --build build -j
//!    # 可执行文件：build/bin/whisper-cli（旧版本叫 build/bin/main）
//!    ```
//! 2. 下载模型（中文推荐 `medium` 或 `large-v3`；`base/small` 更快但准确率低）：
//!    ```text
//!    bash ./models/download-ggml-model.sh medium
//!    # 产物：models/ggml-medium.bin
//!    ```
//! 3. 手动验证命令（本 Provider 内部就是拼这条命令）：
//!    ```text
//!    whisper-cli -m models/ggml-medium.bin -f audio.wav -l zh -t 4 -osrt -of /tmp/out
//!    # 产物：/tmp/out.srt
//!    ```
//! 4. 应用配置（`AiConfig.providers` 里加一条）：
//!    ```json
//!    { "id": "local-whisper", "kind": "LocalWhisper", "enabled": true, "priority": 10,
//!      "model": "/opt/whisper.cpp/models/ggml-medium.bin",
//!      "cli_path": "/opt/whisper.cpp/build/bin/whisper-cli",
//!      "threads": 8, "timeout_secs": 600 }
//!    ```
//! - `cli_path` 空 → 环境变量 `WP_WHISPER_CLI`，再退到 `PATH` 中的 `whisper-cli`；
//! - `model` 必须填 ggml 模型文件路径；
//! - `threads` 对应 `-t`（0=whisper 默认）；
//! - `timeout_secs`：**长音频务必调大**（默认 60s 只适合短音频；整片建议 600+），到点 kill 子进程。
//! 5. 音频来源：whisper.cpp 需要 wav/mp3/flac 等音频文件。视频音轨需先用 ffmpeg 抽取：
//!    ```text
//!    ffmpeg -i video.mp4 -vn -ac 1 -ar 16000 -c:a pcm_s16le audio.wav
//!    ```
//!    （后续可接 `wp-stream` 代理缓存 + 内置 ffmpeg 抽轨，见 [`crate::pipeline::transcribe_file_to_subtitle`]。）
//!
//! # 限制
//! - 当前只解析 `-osrt` 产出的 SRT（以及无时间轴时退化解析 txt/stdout，生成单条无时间轴 cue）；
//! - 未做 VAD 分段/长音频分片（whisper-cli 自带 VAD 可选：`--vad`，可通过
//!   [`WhisperOptions::with_extra_args`] 透传）；
//! - 真模型未在 CI/单测中运行：单测用**伪造的 whisper-cli shell 脚本**验证命令行拼装、SRT 解析、
//!   超时 kill、非零退出、错误可读性。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use wp_core::{Error, Result};
use wp_subtitle::{Cue, SubtitleDoc, SubtitleFormat};

use crate::{AiProvider, ProviderConfig, ProviderKind, TranslateOptions};

/// 默认可执行文件名（`PATH` 中查找）。
pub const DEFAULT_CLI: &str = "whisper-cli";

/// 指定可执行文件路径的环境变量（`cli_path` 未配置时使用）。
pub const ENV_CLI: &str = "WP_WHISPER_CLI";

/// 本地 Whisper Provider 是否已实现真实外部进程链路。
pub const IMPLEMENTED: bool = true;

/// whisper-cli 调用参数（可完全自定义，便于测试/进阶部署）。
#[derive(Debug, Clone)]
pub struct WhisperOptions {
    /// whisper-cli 可执行文件路径或命令名。
    pub cli_path: String,
    /// ggml 模型文件路径（`models/ggml-*.bin`）。
    pub model_path: String,
    /// `-t` 线程数（0=不传，用 whisper 默认）。
    pub threads: u32,
    /// 单次转写超时（到点 kill 子进程）。
    pub timeout: Duration,
    /// 追加到命令行的额外参数（如 `--vad`、`--no-prints`）。
    pub extra_args: Vec<String>,
}

impl Default for WhisperOptions {
    fn default() -> Self {
        Self {
            cli_path: default_cli_path(),
            model_path: String::new(),
            threads: 0,
            timeout: Duration::from_secs(600),
            extra_args: Vec::new(),
        }
    }
}

impl WhisperOptions {
    /// 从 [`ProviderConfig`] 构造：`cli_path`/`model`/`threads`/`timeout_secs` 均可在配置中改。
    pub fn from_config(cfg: &ProviderConfig) -> Self {
        Self {
            cli_path: cfg
                .cli_path
                .clone()
                .filter(|p| !p.trim().is_empty())
                .unwrap_or_else(default_cli_path),
            model_path: cfg.model.trim().to_string(),
            threads: cfg.threads,
            timeout: cfg.effective_timeout(),
            extra_args: Vec::new(),
        }
    }

    /// 追加额外 CLI 参数并返回自身。
    pub fn with_extra_args(mut self, args: Vec<String>) -> Self {
        self.extra_args = args;
        self
    }
}

/// 默认 whisper-cli 路径：环境变量 `WP_WHISPER_CLI` → `whisper-cli`（交给 `PATH` 查找）。
pub fn default_cli_path() -> String {
    std::env::var(ENV_CLI)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_CLI.to_string())
}

/// 本地 Whisper Provider（外部 `whisper-cli` 子进程）。
pub struct LocalWhisperProvider {
    cfg: ProviderConfig,
    opts: WhisperOptions,
}

impl LocalWhisperProvider {
    /// 用配置构造（参数映射见 [`WhisperOptions::from_config`]）。
    pub fn new(cfg: ProviderConfig) -> Result<Self> {
        let opts = WhisperOptions::from_config(&cfg);
        Self::with_options(cfg, opts)
    }

    /// 用自定义参数构造（测试/进阶部署）。
    pub fn with_options(cfg: ProviderConfig, opts: WhisperOptions) -> Result<Self> {
        Ok(Self { cfg, opts })
    }

    /// 配置（`Debug` 已脱敏）。
    pub fn config(&self) -> &ProviderConfig {
        &self.cfg
    }

    /// 实际调用参数。
    pub fn options(&self) -> &WhisperOptions {
        &self.opts
    }

    /// 当前 CLI + 模型是否就绪（文件存在且可解析）。
    pub fn is_available(&self) -> bool {
        resolve_executable(&self.opts.cli_path).is_some()
            && !self.opts.model_path.trim().is_empty()
            && Path::new(&self.opts.model_path).is_file()
    }

    /// 转写音频文件（whisper-cli 直接读取路径，不复制音频内容）。
    pub async fn transcribe_path(&self, path: &Path, lang: &str) -> Result<SubtitleDoc> {
        if self.opts.model_path.trim().is_empty() {
            return Err(Error::other(format!(
                "Provider {}：本地 Whisper 未配置模型路径（ProviderConfig.model 应填 ggml-*.bin 路径）",
                self.cfg.id
            )));
        }
        if !path.is_file() {
            return Err(Error::not_found(format!(
                "Whisper 音频文件不存在：{}（视频请先用 ffmpeg 抽取音轨为 wav/mp3/flac）",
                path.display()
            )));
        }
        let workspace = TempWorkspace::new()?;
        let out_prefix = workspace.path.join("out");

        let mut command = tokio::process::Command::new(&self.opts.cli_path);
        command
            .arg("-m")
            .arg(&self.opts.model_path)
            .arg("-f")
            .arg(path)
            .arg("-l")
            .arg(normalize_lang(lang))
            .arg("-osrt")
            .arg("-of")
            .arg(&out_prefix)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if self.opts.threads > 0 {
            command.arg("-t").arg(self.opts.threads.to_string());
        }
        for extra in &self.opts.extra_args {
            command.arg(extra);
        }

        let child = command
            .spawn()
            .map_err(|e| map_spawn_error(&self.opts.cli_path, &e))?;
        let output = match tokio::time::timeout(self.opts.timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                return Err(Error::other(format!(
                    "whisper-cli 运行失败（{}）：{e}",
                    self.opts.cli_path
                )))
            }
            Err(_) => {
                // 超时后 future 被丢弃 → Child 被 drop → kill_on_drop 终止子进程。
                return Err(Error::network(format!(
                    "whisper-cli 转写超时（{}s），已终止子进程：{}",
                    self.opts.timeout.as_secs().max(1),
                    path.display()
                )));
            }
        };
        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            let stderr = truncate_chars(&String::from_utf8_lossy(&output.stderr), 300);
            return Err(Error::other(format!(
                "whisper-cli 退出码 {code}{}",
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!("：{stderr}")
                }
            )));
        }

        let srt_path = out_prefix.with_extension("srt");
        let txt_path = out_prefix.with_extension("txt");
        let mut cues = Vec::new();
        if srt_path.is_file() {
            let srt = tokio::fs::read_to_string(&srt_path)
                .await
                .map_err(Error::Io)?;
            cues = parse_srt_cues(&srt);
        }
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if cues.is_empty() && stdout.contains("-->") {
            cues = parse_srt_cues(&stdout);
        }
        if !cues.is_empty() {
            return Ok(SubtitleDoc {
                format: Some(SubtitleFormat::Srt),
                cues,
                ass_header: None,
            });
        }
        // 退化路径：只有纯文本（`-otxt` 或 stdout）→ 单条无时间轴 cue。
        let unsynced = if txt_path.is_file() {
            tokio::fs::read_to_string(&txt_path)
                .await
                .map_err(Error::Io)?
        } else {
            stdout
        };
        let unsynced = unsynced.trim();
        if unsynced.is_empty() {
            return Err(Error::other(format!(
                "whisper-cli 输出没有字幕条目（可能音频无语音或输出格式异常；命令：{} -m {} -f {} -l {} -osrt）",
                self.opts.cli_path,
                self.opts.model_path,
                path.display(),
                normalize_lang(lang)
            )));
        }
        Ok(SubtitleDoc {
            format: Some(SubtitleFormat::Srt),
            cues: vec![Cue {
                start_ms: 0,
                end_ms: 0,
                text: unsynced.to_string(),
                raw_style: None,
            }],
            ass_header: None,
        })
    }

    /// 转写内存中的音频字节（写临时文件后调用 CLI；按文件头猜扩展名，默认 wav）。
    pub async fn transcribe_bytes(&self, audio: &[u8], lang: &str) -> Result<SubtitleDoc> {
        if audio.is_empty() {
            return Err(Error::other("音频数据为空，无法转写"));
        }
        let workspace = TempWorkspace::new()?;
        let extension = guess_audio_extension(audio);
        let audio_path = workspace.path.join(format!("audio.{extension}"));
        tokio::fs::write(&audio_path, audio)
            .await
            .map_err(Error::Io)?;
        self.transcribe_path(&audio_path, lang).await
    }
}

#[async_trait]
impl AiProvider for LocalWhisperProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::LocalWhisper
    }

    async fn translate(&self, _cues: &[Cue], _opts: &TranslateOptions) -> Result<Vec<String>> {
        Err(Error::unsupported(
            "本地 Whisper 只做语音转写，不支持字幕翻译（请在 AI 设置里改选翻译 Provider）",
        ))
    }

    async fn transcribe(&self, audio: &[u8], lang: &str) -> Result<SubtitleDoc> {
        self.transcribe_bytes(audio, lang).await
    }

    async fn transcribe_file(&self, path: &Path, lang: &str) -> Result<SubtitleDoc> {
        self.transcribe_path(path, lang).await
    }

    async fn complete(&self, _prompt: &str) -> Result<String> {
        Err(Error::unsupported("本地 Whisper 不支持文本生成"))
    }
}

/// 本地 Whisper 健康检查：CLI 可解析 + 模型文件存在。
pub async fn health_check(cfg: &ProviderConfig) -> bool {
    let opts = WhisperOptions::from_config(cfg);
    resolve_executable(&opts.cli_path).is_some()
        && !opts.model_path.trim().is_empty()
        && Path::new(&opts.model_path).is_file()
}

/// 解析可执行文件：含路径分隔符按文件判断，否则遍历 `PATH`。
pub fn resolve_executable(name_or_path: &str) -> Option<PathBuf> {
    let name_or_path = name_or_path.trim();
    if name_or_path.is_empty() {
        return None;
    }
    let candidate = Path::new(name_or_path);
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name_or_path))
        .find(|p| p.is_file())
}

/// 语言参数归一化（空/`auto` → `auto`）。
pub fn normalize_lang(lang: &str) -> String {
    let lang = lang.trim();
    if lang.is_empty() {
        "auto".to_string()
    } else {
        lang.to_string()
    }
}

fn map_spawn_error(cli: &str, err: &std::io::Error) -> Error {
    if err.kind() == std::io::ErrorKind::NotFound {
        Error::not_found(format!(
            "找不到 whisper-cli：{cli}（请构建 whisper.cpp 并在配置中设置 cli_path，或设置环境变量 {ENV_CLI}）"
        ))
    } else {
        Error::other(format!("启动 whisper-cli 失败（{cli}）：{err}"))
    }
}

fn guess_audio_extension(audio: &[u8]) -> &'static str {
    if audio.starts_with(b"RIFF") {
        "wav"
    } else if audio.starts_with(b"ID3")
        || (audio.len() > 1 && audio[0] == 0xFF && (audio[1] & 0xE0) == 0xE0)
    {
        "mp3"
    } else if audio.starts_with(b"fLaC") {
        "flac"
    } else if audio.starts_with(b"OggS") {
        "ogg"
    } else if audio.len() > 11 && &audio[4..8] == b"ftyp" {
        "m4a"
    } else {
        "wav"
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

/// 解析 SRT 时间戳（`HH:MM:SS,mmm` / `HH:MM:SS.mmm` / `MM:SS,mmm`）→ 毫秒。
pub fn parse_srt_timestamp(timestamp: &str) -> Option<i64> {
    let timestamp = timestamp.trim();
    let (hms, millis_part) = timestamp.rsplit_once([',', '.'])?;
    let parts: Vec<&str> = hms.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [h, m, s] => (
            h.trim().parse::<i64>().ok()?,
            m.trim().parse::<i64>().ok()?,
            s.trim().parse::<i64>().ok()?,
        ),
        [m, s] => (
            0,
            m.trim().parse::<i64>().ok()?,
            s.trim().parse::<i64>().ok()?,
        ),
        _ => return None,
    };
    let digits: String = millis_part
        .trim()
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    let millis: i64 = match digits.len() {
        0 => 0,
        1 => digits.parse::<i64>().ok()? * 100,
        2 => digits.parse::<i64>().ok()? * 10,
        _ => digits[..3].parse::<i64>().ok()?,
    };
    Some(hours * 3_600_000 + minutes * 60_000 + seconds * 1_000 + millis)
}

/// 解析 SRT 文本 → cues（容错空行/编号缺失/`\r\n`）。
pub fn parse_srt_cues(text: &str) -> Vec<Cue> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut cues = Vec::new();
    let mut block: Vec<&str> = Vec::new();
    for line in normalized.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            if let Some(cue) = parse_srt_block(&block) {
                cues.push(cue);
            }
            block.clear();
        } else {
            block.push(line);
        }
    }
    cues
}

fn parse_srt_block(lines: &[&str]) -> Option<Cue> {
    let mut iter = lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty());
    let first = iter.next()?;
    let time_line = if first.chars().all(|c| c.is_ascii_digit()) {
        iter.next()?
    } else {
        first
    };
    let (start_ms, end_ms) = parse_srt_time_line(time_line)?;
    let text = iter.collect::<Vec<_>>().join("\n");
    if text.trim().is_empty() {
        return None;
    }
    Some(Cue {
        start_ms,
        end_ms,
        text,
        raw_style: None,
    })
}

fn parse_srt_time_line(line: &str) -> Option<(i64, i64)> {
    let (start, rest) = line.split_once("-->")?;
    let end = rest.split_whitespace().next()?;
    Some((parse_srt_timestamp(start)?, parse_srt_timestamp(end)?))
}

static WORKSPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 临时工作目录（Drop 清理）。
struct TempWorkspace {
    path: PathBuf,
}

impl TempWorkspace {
    fn new() -> Result<Self> {
        let count = WORKSPACE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("wp-ai-whisper-{}-{count}", std::process::id()));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
