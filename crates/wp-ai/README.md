# wp-ai —— AI 子系统（P1 字幕翻译 + P2 本地 ASR + P3 摘要原型 + 集成闭环）

播放优先的 AI 地基：**本地 + 远端 Provider 可分别启停/调优先级**，任务队列（优先级/取消/超时/
并发上限/播放期策略），JSON 结果缓存（文件指纹+模型+目标语言+任务类型），字幕翻译管线与 ASR 管线。

| 能力 | Provider | 状态 |
|---|---|---|
| 字幕翻译（P1，双语） | 远端 OpenAI 兼容 + 本地 llama-server/vLLM/Ollama | ✅ |
| 文本生成/摘要问答（P3 基础） | 同上 | ✅ |
| 语音转写生成字幕（P2） | 本地 `whisper-cli`（whisper.cpp 外部进程） | ✅ 管线完整，真模型未在 CI 跑 |
| 集成闭环：翻译 → 双语 ASS 文件（漏译统计/降级）、ffmpeg 抽 16k wav（ASR 前置） | `wp_ai::integration` | ✅ |
| P3 字幕摘要/章节（原型）：分批窗口 → JSON 章节（容错/纯文本回退/失败降级）→ 时间轴合法化 → 缓存 | `wp_ai::summarize` | ✅ |
| 语音转写（P2 远端） | 远端 OpenAI 兼容 `/audio/transcriptions` | ⏳ 未实现（当前返回 Unsupported 并回退） |
| OCR/超分（P4） | ONNX | ⏳ 未实现 |

## 配置

`AiConfig` 直接序列化进应用配置（`配置.json` 的 `ai` 字段）：

```json
{
  "providers": [
    {
      "id": "deepseek", "kind": "RemoteOpenAi", "enabled": true, "priority": 20,
      "base_url": "https://api.deepseek.com/v1", "model": "deepseek-chat", "api_key": "sk-…",
      "timeout_secs": 60, "max_retries": 2, "batch_size": 20
    },
    {
      "id": "local-llama", "kind": "LocalLlm", "enabled": false, "priority": 10,
      "base_url": "http://127.0.0.1:8080/v1", "model": "qwen2.5-7b-instruct", "timeout_secs": 300
    },
    {
      "id": "local-whisper", "kind": "LocalWhisper", "enabled": false, "priority": 30,
      "model": "/opt/whisper.cpp/models/ggml-medium.bin",
      "cli_path": "/opt/whisper.cpp/build/bin/whisper-cli",
      "threads": 8, "timeout_secs": 600
    }
  ],
  "offline_only": false,
  "allow_during_playback": false
}
```

- `priority` 数值**越小越优先**；失败按优先级回退下一个能力匹配的 Provider。
- `offline_only=true`：只用本地 Provider，字幕/音频不出本机。
- `allow_during_playback=false`（默认）：播放期间拒绝非紧急任务；单任务 `.with_urgent(true)` 放行。
- `base_url`：OpenAI 兼容根地址（可写 `…/v1`，只写主机会自动补 `/v1`）。
- 本地 ASR 额外字段：`cli_path`（whisper-cli 路径，空=环境变量 `WP_WHISPER_CLI` 或 `PATH` 查找）、
  `threads`（`-t`）、`model`（ggml 模型路径）。

## 本地 LLM（llama-server / vLLM / Ollama）

```bash
# llama.cpp（推荐）
git clone https://github.com/ggml-org/llama.cpp && cd llama.cpp
cmake -B build && cmake --build build -j
./build/bin/llama-server -m qwen2.5-7b-instruct-q4_k_m.gguf --port 8080 -c 8192 -ngl 99
# → base_url http://127.0.0.1:8080/v1

# Ollama
ollama serve && ollama pull qwen2.5:7b
# → base_url http://127.0.0.1:11434/v1，model qwen2.5:7b

# vLLM
python -m vllm.entrypoints.openai.api_server --model Qwen/Qwen2.5-7B-Instruct --port 8000
# → base_url http://127.0.0.1:8000/v1，model Qwen/Qwen2.5-7B-Instruct
```

健康检查 `GET {base_url}/models`（绕过系统代理）；不存在 API key 概念，配置里的 `api_key` 会被清空。

## 本地 ASR（whisper.cpp 外部进程）

```bash
# 1) 构建 whisper.cpp
git clone https://github.com/ggml-org/whisper.cpp && cd whisper.cpp
cmake -B build && cmake --build build -j        # 产物 build/bin/whisper-cli
# 2) 下载模型（中文推荐 medium / large-v3）
bash ./models/download-ggml-model.sh medium     # 产物 models/ggml-medium.bin
# 3) 视频抽音轨（whisper 需要音频文件；本 Provider 不内置 ffmpeg）
ffmpeg -i video.mkv -vn -ac 1 -ar 16000 -c:a pcm_s16le audio.wav
# 4) 手工验证（Provider 内部就是拼这条命令）
whisper-cli -m models/ggml-medium.bin -f audio.wav -l zh -t 8 -osrt -of /tmp/out  # → /tmp/out.srt
```

超时/取消：`timeout_secs` 到点会 kill 子进程；任务取消（`AiService::cancel`）通过
`kill_on_drop` 终止。错误可读（找不到 CLI/模型未配置/非零退出含 stderr/输出为空）。
**限制**：单测用伪造 shell 脚本覆盖命令行拼装/SRT 解析/超时/失败路径；真实模型精度与速度需本机联调。

## 集成闭环（GUI 一个调用拿到成品双语 ASS）

```rust
use wp_ai::{AiCache, TranslateOptions};
use wp_ai::integration::translate_doc_and_write_ass;
use wp_subtitle::AssOptions;

let result = translate_doc_and_write_ass(
    &registry, &cache, &fingerprint, &doc,
    &TranslateOptions::default(),      // 源/目标语言 + 术语表
    &AssOptions::default(),            // 字体/字号/双语布局
    std::path::Path::new("/tmp/bilingual.ass"),
).await?;
// result: { path, total, matched, missing, provider_id, cached, degraded, errors, coverage() }
if result.degraded { /* 翻译失败已降级为原文字幕 */ }
```

ASR 前置抽音轨（视频 → 16kHz 单声道 wav）：

```rust
use wp_ai::integration::{extract_audio_for_asr, cleanup_temp_audio};
let wav = extract_audio_for_asr(video_path).await?;   // 外部 ffmpeg，超时 kill、缺 ffmpeg → NotFound
// pipeline::transcribe_file_to_subtitle(&registry, &cache, &fp, &wav, "zh").await …
cleanup_temp_audio(&wav);
```

详细集成说明（GUI 伪代码、mpv `sub-add`/`sub-reload`、真机评估方案）见
[docs/PLAYER_AI.md](../../docs/PLAYER_AI.md)。

## 代码用法

### 字幕翻译（带缓存/降级）

```rust
use wp_ai::cache::AiCache;
use wp_ai::pipeline::translate_subtitle_document;
use wp_ai::provider::build_provider_registry;
use wp_ai::TranslateOptions;

let (registry, warnings) = build_provider_registry(&ai_cfg); // warnings 可直接展示
let cache = AiCache::new(cache_dir);
let outcome = translate_subtitle_document(&registry, &cache, &fingerprint, &doc, &TranslateOptions::default()).await;
if outcome.degraded { /* 提示失败，译文=原文 */ }
let translated = wp_ai::pipeline::build_translated_doc(&doc, &outcome.translations);
// 交给 wp_subtitle::build_bilingual_ass(original, translated, opts) → 双语 ASS
```

### ASR 转写为字幕（文件路径推荐）

```rust
use wp_ai::pipeline::transcribe_file_to_subtitle;

let outcome = transcribe_file_to_subtitle(&registry, &cache, &fingerprint, audio_path, "zh").await;
if outcome.is_complete() {
    let doc = outcome.doc;           // SubtitleDoc：时间轴 + 文本，可直接交给 wp-subtitle 导出/注入
} else {
    eprintln!("ASR 失败：{:?}", outcome.errors);
}
```

### 任务队列 / AI 面板

```rust
let config = AiServiceConfig::new(ai_cfg)
    .with_cache_dir(cache_dir)
    .with_max_concurrent(1)
    .with_task_timeout(std::time::Duration::from_secs(600));
let (service, mut events) = AiService::spawn(config, registry)?;
service.set_playing(true);                                     // 播放期策略
let statuses = service.provider_statuses();                    // 面板列表（不含密钥）
let id = service.submit_subtitle_translation(&doc, opts, &fp)?;
let id2 = service.submit_transcribe_file(audio_path, "zh", &fp)?;
service.cancel(id)?;
while let Some(event) = events.recv().await { /* Done/Failed/Canceled/Rejected */ }
```

## P3 字幕摘要 / 章节（原型）

```rust
use wp_ai::summarize::{summarize_subtitles, summary_to_markdown, SummarizeOptions};

let outcome = summarize_subtitles(&registry, &cache, &fingerprint, &doc, &SummarizeOptions::default()).await;
println!("{}", outcome.summary.to_json_pretty()?);     // 章节 JSON
println!("{}", summary_to_markdown(&outcome.summary)); // 可读 Markdown
// AiSummary { chapters[{start_ms,end_ms,title,summary}], overview, keywords[] }
// 窗口默认 5 分钟（可配 1–10 分钟）；JSON 容错/纯文本回退；Provider 失败→标题=首句；缓存命中零调用
```

示例（mock provider，无需 key）：`cargo run -p wp-ai --example summarize_srt`
详细说明（数据结构/示例输出/GUI 章节目录+点击 seek 建议）见 [docs/PLAYER_AI.md](../../docs/PLAYER_AI.md) §5。

## 测试

```bash
cargo test -p wp-ai                 # 65 单测 + 5 doc test（本地 mock HTTP/伪造进程，不联网）
cargo test -p wp-ai --all-features  # 81 单测（+ 本地 LLM mock HTTP / 伪 whisper-cli 脚本）
cargo run -p wp-ai --example summarize_srt  # P3 摘要示例（mock）
cargo clippy -p wp-ai --all-features --all-targets  # 0 warning
```

## 路线与限制

- 重活最终要跑独立 worker 进程（架构 §7）；当前 `service` 是进程内 tokio 队列，语义已对齐
  （id/优先级/取消/超时/事件），后续把 `execute_task` 换成 IPC 即可，GUI 接口不变。
- 远端 ASR（`/audio/transcriptions`）、P3 问答/台词搜索、P4 OCR/超分待做；P3 摘要原型已可用（每窗口独立生成后合并，超长片连贯性后续可做二次汇总）。
- **真机翻译质量/时延待真机**（本机无 API key、无本地权重）：样本与方法见
  [docs/PLAYER_AI.md](../../docs/PLAYER_AI.md) §8（P3 摘要质量同样待真机，见 §5）。
- 真实大模型/whisper 模型未纳入 CI（无密钥、无权重）；单测全部使用本地 mock HTTP 与伪造 CLI 脚本。
