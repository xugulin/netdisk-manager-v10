# 播放器 AI 集成说明（PLAYER_AI）

> 日期：2026-10-19 · 状态：**wp-ai 集成层已实现（P1 字幕翻译 / P2 本地 ASR）；真机翻译评估待真机**
> 范围：`crates/wp-ai`（Provider / 队列 / 缓存 / 管线 / `integration` 集成层）+ 播放器/GUI 接入约定。
> 相关：[PLAYER_ARCHITECTURE.md](PLAYER_ARCHITECTURE.md) §7、[PLAYER_TASKS.md](PLAYER_TASKS.md) T18–T21 / T11。

---

## 1. 结论速览

| 能力 | 实现 | 入口 |
|---|---|---|
| 字幕翻译（P1，双语） | ✅ 远端 OpenAI 兼容（DeepSeek/OpenAI 等）+ 本地 llama-server/vLLM/Ollama | `wp_ai::pipeline::translate_subtitle_document` / `translate_doc_and_write_ass` |
| 翻译 → 双语 ASS（保留原样式/ASS 头） | ✅ | `wp_ai::integration::translate_doc_and_write_ass` → 产出可 `sub-add` 的文件 |
| 本地 ASR（P2） | ✅ whisper.cpp `whisper-cli` 外部进程 | `wp_ai::integration::extract_audio_for_asr` → `pipeline::transcribe_file_to_subtitle` |
| VIP 级「播放期不抢 CPU」 | ✅ 队列默认并发 1 + 播放期拒绝非紧急任务 + 超时/取消 | `AiService::set_playing` / `submit(..., urgent)` |
| 结果缓存（跨次播放零成本） | ✅ (文件指纹 + 模型 + 目标语言 + 任务类型) | `AiCache` |
| 远端 ASR / P3 摘要问答 / P4 OCR | ⏳ 未实现（返回 Unsupported 并按优先级回退） | — |

一次典型调用链（GUI 一个按钮）：

```text
外挂字幕 SubtitleDoc
   └─(AiService 队列)→ Provider 翻译（本地优先/失败回退/缓存）
        └─→ 双语 ASS 文本（wp-subtitle：保留原 ASS 头/样式，原文+译文双轨）
             └─→ 原子写文件 → mpv: sub-add / sub-reload → 屏幕显示双语
```

---

## 2. 配置（`配置.json` 的 `ai` 字段 = `wp_ai::AiConfig`）

```json
{
  "providers": [
    {
      "id": "deepseek", "kind": "RemoteOpenAi", "enabled": true, "priority": 20,
      "base_url": "https://api.deepseek.com/v1", "model": "deepseek-chat", "api_key": "sk-…",
      "timeout_secs": 60, "max_retries": 2, "batch_size": 20
    },
    {
      "id": "local-llama", "kind": "LocalLlm", "enabled": true, "priority": 10,
      "base_url": "http://127.0.0.1:8080/v1", "model": "qwen2.5-7b-instruct", "timeout_secs": 300
    },
    {
      "id": "local-whisper", "kind": "LocalWhisper", "enabled": true, "priority": 10,
      "model": "/opt/whisper.cpp/models/ggml-medium.bin",
      "cli_path": "/opt/whisper.cpp/build/bin/whisper-cli",
      "threads": 8, "timeout_secs": 600
    }
  ],
  "offline_only": false,
  "allow_during_playback": false
}
```

- `priority`：数值越小越优先；失败按优先级回退到下一个能力匹配 Provider（远端挂了自动走本地，反之亦然）。
- `offline_only=true`：只用本地 Provider，字幕/音频不出本机。
- `allow_during_playback=false`（默认）：播放期间非紧急 AI 任务会被拒绝（返回可识别错误
  `wp_ai::is_playback_rejected`）；实时前瞻类任务可在提交时 `.with_urgent(true)` 放行。
- `api_key` 只出现在 `Authorization: Bearer` 头；`Debug`/日志/错误信息均已脱敏（`***`）。
- 本地 ASR 的 `cli_path` 空时依次用环境变量 `WP_WHISPER_CLI` → `PATH` 里的 `whisper-cli`；
  `threads` 对应 `-t`；长音频务必把 `timeout_secs` 调大（如 600）。

### 2.1 GUI Provider 配置面板约定

面板只需编辑 `AiConfig` 并持久化；启停/调优先级不需要重建其它状态，**重建 `AiService`** 即可生效：

```rust
// AI 面板：列出 provider（不含密钥）
let statuses = service.provider_statuses();
// ProviderStatus { id, kind, enabled, priority, model, is_local, registered, usable }

// 用户改完后（enabled/priority/base_url/model/api_key…）
let (registry, warnings) = wp_ai::provider::build_provider_registry(&new_ai_cfg);
let (service, events) = AiService::from_config(AiServiceConfig::new(new_ai_cfg))?; // 或 spawn(config, registry)
```

`warnings` 是中文可读提示（本地 feature 未编译、远端缺 model/base_url、本地 CLI/模型不存在等），直接展示。

---

## 3. 本地模型部署（可选，隐私优先）

### 3.1 本地 LLM（OpenAI 兼容服务，三选一）

```bash
# llama.cpp（推荐：无 Python 依赖、CPU/GPU 都可）
git clone https://github.com/ggml-org/llama.cpp && cd llama.cpp
cmake -B build && cmake --build build -j
./build/bin/llama-server -m qwen2.5-7b-instruct-q4_k_m.gguf --port 8080 -c 8192 -ngl 99
# → ai.providers[]: kind LocalLlm, base_url http://127.0.0.1:8080/v1, model 与 -m 对应

# Ollama（最省事）
ollama serve && ollama pull qwen2.5:7b
# → base_url http://127.0.0.1:11434/v1, model "qwen2.5:7b"

# vLLM（NVIDIA GPU 吞吐最好）
python -m vllm.entrypoints.openai.api_server --model Qwen/Qwen2.5-7B-Instruct --port 8000
# → base_url http://127.0.0.1:8000/v1, model "Qwen/Qwen2.5-7B-Instruct"
```

健康检查：`GET {base_url}/models`（本地 Provider **绕过系统代理**）。连接失败 → `Network` 错误 → 自动回退。

### 3.2 本地 ASR（whisper.cpp）

```bash
# 1) 构建
git clone https://github.com/ggml-org/whisper.cpp && cd whisper.cpp
cmake -B build && cmake --build build -j          # 产物 build/bin/whisper-cli
# 2) 模型（中文推荐 medium / large-v3；base/small 更快但准确率低）
bash ./models/download-ggml-model.sh medium       # 产物 models/ggml-medium.bin
# 3) 手工验证（Provider 内部命令同款）
whisper-cli -m models/ggml-medium.bin -f audio.wav -l zh -t 8 -osrt -of /tmp/out   # → /tmp/out.srt
```

视频需先抽音轨（见 §4.3，`extract_audio_for_asr` 已封装 ffmpeg：`-vn -ac 1 -ar 16000 -c:a pcm_s16le`）。
ffmpeg 不在 PATH 时：配置环境变量 `WP_FFMPEG=/path/to/ffmpeg`，或调用 `FfmpegOptions::with_cli(...)`。

---

## 4. GUI / 播放器接入伪代码

> 以下为 Rust 伪代码（省略 UI 状态/错误弹窗细节），真实签名见 `crates/wp-ai/README.md` 与源码。

### 4.1 初始化（播放器页面创建时）

```rust
// 1) 从配置读取 AiConfig，构造 registry（为 enabled 的 provider 建实例）
let (registry, warnings) = wp_ai::provider::build_provider_registry(&app.config.ai);
// 2) 启动后台队列（默认并发 1，播放优先；缓存目录=数据/缓存/ai）
let svc_cfg = AiServiceConfig::new(app.config.ai.clone())
    .with_cache_dir(app.paths.cache_dir().join("ai"))
    .with_max_concurrent(1)
    .with_task_timeout(Duration::from_secs(600));
let (service, mut events) = AiService::spawn(svc_cfg, registry)?;
app.ai.service = Some(service);
app.ai.events = events;                      // egui 每帧 try_recv 消费
// 3) 面板状态
let statuses = app.ai.service.as_ref().unwrap().provider_statuses();
```

### 4.2 事件循环（egui update 中非阻塞消费）

```rust
while let Ok(event) = events.try_recv() {
    match event.outcome {
        AiOutcome::Done(AiResult::Translations { translations, provider_id, cached }) => {
            // 翻译任务完成：写双语 ASS → 交给播放器注入
            let style = app.config.subtitle.ass_options();          // 字体/字号/双轨布局
            let out = app.paths.cache_dir().join("ai/bilingual.ass");
            // 也可直接用 pipeline::translate_doc_and_write_ass 一步完成（见 4.3 简化版）
            let ass = wp_ai::integration::translate_to_bilingual_ass(&doc, &translations, &style)?;
            std::fs::write(&out, ass)?;
            app.player.command(PlayerCommand::SubAdd { path: out, select: true, name: "AI 双语" });
        }
        AiOutcome::Done(AiResult::Subtitle { doc, .. }) => {
            // ASR 完成：转 ASS/或直接注入 SRT；写入后 sub-add
        }
        AiOutcome::Failed { message, retryable } => app.toast(format!("AI 失败：{message}（可重试={retryable}）")),
        AiOutcome::Canceled => app.toast("AI 任务已取消"),
        AiOutcome::Rejected { message } => app.toast(message),      // 播放期策略拒绝
        _ => {}
    }
}
```

### 4.3 典型流程

**A. 字幕翻译 → 双语 ASS（一步到位，推荐）**

```rust
// 触发：字幕面板「AI 翻译」按钮
let doc = current_subtitle_doc;                       // wp-subtitle 解析出的外挂字幕
let fingerprint = wp_ai::cache::fingerprint_file_metadata(video_size, video_mtime_ms);
let out = app.paths.cache_dir().join("ai").join(format!("{stem}.bilingual.ass"));

let result = wp_ai::integration::translate_doc_and_write_ass(
    &registry, &cache, &fingerprint, &doc,
    &TranslateOptions { source_lang, target_lang, glossary },
    &app.config.subtitle.ass_options(),
    &out,
).await?;                                            // 内部走队列之外的直连管线（GUI 可放 spawn_blocking/任务里）

if result.degraded {
    app.toast(format!("翻译失败，已输出原文字幕：{:?}", result.errors));
} else {
    app.toast(format!("双语字幕 {} 条（漏译 {} 条，缓存={}）", result.matched, result.missing, result.cached));
}
// mpv 注入（T11 字幕专家接口；等价 JSON IPC）：
//   sub-add "<out>" select "AI 双语"
//   已存在时：sub-reload <sid>   （避免重复叠加轨道）
app.player.command(PlayerCommand::SubAdd { path: out, select: true, name: "AI 双语" });
```

**B. 无字幕视频 → ASR 生成字幕 → 注入**

```rust
// 1) 抽音轨（外部 ffmpeg；失败返回 Err：NotFound/Network 超时/Other 含 stderr）
let wav = wp_ai::integration::extract_audio_for_asr(&video_path).await?;

// 2) 转写（whisper.cpp；缓存键 = 文件指纹+模型+语言+TranscribeAudio）
let outcome = wp_ai::pipeline::transcribe_file_to_subtitle(
    &registry, &cache, &fingerprint, &wav, &target_lang,
).await;

// 3) 生成 ASS（单语）并注入；或写 SRT
let ass = wp_subtitle::build_ass(&outcome.doc, &AssOptions::default());
let out = app.paths.cache_dir().join("ai").join("asr.ass");
std::fs::write(&out, ass)?;
app.player.command(PlayerCommand::SubAdd { path: out, select: true, name: "AI 转写" });

// 4) 清理临时音频
wp_ai::integration::cleanup_temp_audio(&wav);

if !outcome.is_complete() {
    app.toast(format!("ASR 失败：{:?}", outcome.errors));
}
```

**C. 队列版（可显示进度/可取消；播放期自动拒绝）**

```rust
service.set_playing(player.is_playing());             // 播放/暂停事件里调用
let id = service.submit_subtitle_translation(&doc, opts, &fingerprint)?; // 或 submit_transcribe_file(...)
// 进度 UI：任务事件里按 task_id 匹配；取消：
service.cancel(id)?;
```

**D. 实时前景（可选，P1+）**：播放中只对**即将到来**的 cue 做 `translate_with_fallback(..., urgent=true)`，
结果直接写 `mpv sub-text` 覆盖显示；避免整片翻译抢 CPU（见 §6 限制）。

### 4.4 mpv 关键词命令速查（由播放器控制层封装）

| 目的 | mpv 命令 | 备注 |
|---|---|---|
| 添加外挂字幕（双语 ASS） | `sub-add "<path>" select "AI 双语"` | 返回新字幕轨道 id |
| 重载已注入字幕 | `sub-reload <sid>` | 文件被重写后刷新（同路径） |
| 移除字幕后重加 | `sub-remove <sid>` | 换语言/样式时用 |
| 主/副字幕（第二轨） | `secondary-sub-add` / `secondary-sub-*` | mpv ≥0.38；与 wp-media 轨选择对接 |
| 字幕延迟/字号 | `sub-delay` / 字幕样式在 ASS 内 | AI 译文轨样式名 `AI-Translated` |

> 具体封装以 `wp-media` / `wp-gui` 播放器控制层为准（T11/T15）；AI 层只负责产出合法 ASS 文件。

---

## 5. 集成 API 速查（`wp_ai::integration`）

| API | 说明 |
|---|---|
| `translate_to_bilingual_ass(doc, &[String], &AssOptions) -> Result<String>` | 译文列表 → 双语 ASS 文本（保留原 ASS 头/样式；漏译只输出原文） |
| `translate_to_bilingual_ass_report(doc, &[String], &AssOptions) -> BilingualAss` | 同上，附带 `content/matched/missing` |
| `translate_doc_and_write_ass(registry, cache, fingerprint, doc, opts, style, out_path) -> Result<BilingualResult>` | 翻译→双语 ASS→**原子写文件**，返回 `{path,total,matched,missing,provider_id,cached,degraded,errors,coverage()}` |
| `extract_audio_ffmpeg(input, out_wav) -> Result<()>` | 外部 ffmpeg 抽 16kHz 单声道 wav（默认 600s 超时，到点 kill） |
| `extract_audio_ffmpeg_with(&FfmpegOptions, …)` | 自定义 ffmpeg 路径/超时/输入参数（`WP_FFMPEG` 环境变量可覆盖默认） |
| `extract_audio_for_asr(input) -> Result<PathBuf>` | 抽轨到唯一临时 wav 路径（ASR 前置一步完成） |
| `cleanup_temp_audio(path)` | 删除 ASR 临时音频（best-effort） |

失败语义：**翻译失败 = 降级不报错**（`degraded=true`、`matched=0`、`errors` 可读）；
**写文件/ffmpeg 失败 = 返回 `Err`**（`NotFound` 可定位缺 ffmpeg/输入，`Network` 表示超时，`Other` 含 stderr 摘要）。

---

## 6. 缓存 / 性能 / 隐私

- 缓存键 =（文件指纹 + 模型 + 目标语言 + 任务类型），JSON 文件、原子写、LRU（默认 200 条）；
  二次播放翻译 **0 次 Provider 调用**（GUI 显示「命中缓存」）。
- 文件指纹建议 `指纹(大小, mtime)`（`fingerprint_file_metadata`），视频不要整文件哈希。
- 播放优先：队列默认并发 1；播放期间 `allow_during_playback=false` 拒绝非紧急任务；
  任务的 Provider 请求超时/取消会中止（本地 whisper/ffmpeg 子进程 `kill_on_drop`）。
- 隐私：`offline_only=true` 时纯本地；远端模式会把字幕文本（仅文本，不含视频/音频）发给
  `base_url`；`api_key` 不进日志。本地 ASR 时音频只在本机临时目录（用完删除）。

## 7. 真机翻译评估：**待真机**

本机现状（2026-10-19）：**无 OpenAI 兼容 API Key**（环境变量/配置均未提供），且
`llama-server` 未启动、未下载 whisper/LLM 权重 → **无法做真机 20 句翻译质量/时延评估**，
本节记录方法与样本，待 Lead 提供 key 或本机拉起 llama-server 后补测。

### 7.1 评估方法（待执行）

1. 准备 20 句字幕样本（下表中英各半，含专有名词/口语/长句）；
2. 用 `RemoteOpenAiProvider`（DeepSeek `deepseek-chat` 或本地 llama-server）批量翻译；
3. 记录：总耗时、每批（20 条）请求数、token 用量、失败/漏译数、抽样人工质量评分（1–5）；
4. 对照：`MockProvider` 仅验证管线正确性，不代表质量。

### 7.2 样本（20 句，中→英 / 英→中的评估输入）

| # | 源文 | 目标 | 期望观察点 |
|---|---|---|---|
| 1 | 你昨晚去哪了？ | en | 口语时态 |
| 2 | 这份报告今天必须交。 | en | 情态动词 |
| 3 | 会议推迟到下周三上午十点。 | en | 时间表达 |
| 4 | 他可能是对的。 | en | 不确定语气 |
| 5 | 请不要在走廊里奔跑。 | en | 祈使句 |
| 6 | 这个问题比看起来复杂得多。 | en | 比较结构 |
| 7 | 我们得在天亮前离开这里。 | en | 条件/时间副词 |
| 8 | 她一边做饭一边打电话。 | en | 同时动作 |
| 9 | 打开设置，然后选择「字幕」。 | en | UI 术语（术语表约束） |
| 10 | 这部电影我看了三遍。 | en | 动量表达 |
| 11 | Did you finish the report? | zh | 疑问语气 |
| 12 | I'll be there in five minutes. | zh | 缩略/将来时 |
| 13 | We should probably wait for the rain to stop. | zh | 建议语气 |
| 14 | That's exactly what I meant. | zh | 强调句 |
| 15 | Keep your seat belt fastened. | zh | 标语体简洁 |
| 16 | The server is under maintenance. | zh | 技术短语（术语表） |
| 17 | It's not as bad as it looks. | zh | 比较/否定 |
| 18 | He hasn't decided yet. | zh | 完成时 |
| 19 | Turn left at the next intersection. | zh | 导航短句 |
| 20 | I can't believe we made it on time. | zh | 情绪/口语 |

### 7.3 待填结果（真机后补）

| 模型/端点 | 20 句总耗时 | 漏译 | 平均质量(1–5) | 备注 |
|---|---|---|---|---|
| DeepSeek `deepseek-chat` | —（待真机） | — | — | 无 key |
| 本地 llama-server（Qwen2.5-7B） | —（待真机） | — | — | 服务未启动/无权重 |

> 当前 CI 结论：**管线正确性**由 `cargo test -p wp-ai --all-features`（67 项，其中集成层新增 16 项，
> 含 mock HTTP 与伪造进程脚本）保证；**翻译质量与时延**待真机补充。

---

## 8. 限制与后续

1. **未接真模型**：单测全部 mock（HTTP/伪造 ffmpeg、whisper-cli 脚本）；真机质量/性能未测（§7）。
2. **远端 ASR 未实现**：OpenAI 兼容 `/audio/transcriptions` 待 P2 补齐；当前 ASR 只有本地 whisper.cpp。
3. **视频抽音轨依赖外部 ffmpeg**（已封装）；内嵌字幕轨提取（mkv 内封）尚未接入，需要播放器/媒体层导出。
4. **进程隔离**：目前 ASR/ffmpeg 已是子进程；本地 LLM 推理在 llama-server 进程（本 crate 仅 HTTP）；
   架构 §7 的「独立 worker 进程 + IPC」仍是目标形态，上层 `AiService` 语义已对齐，后续可替换执行层。
5. **实时前瞻字幕**：当前先做整片翻译（P1 验收范围）；播放中按 cue 前瞻翻译需播放器事件钩子（`sub-start/sub-text`）。
6. **P3 摘要问答 / P4 字幕 OCR/超分**：接口占位，未实现。
7. Windows：`extract_audio_for_asr`/whisper 子进程逻辑跨平台可编译，但伪造脚本测试仅 Unix；
   Windows 需在实例上验证 `ffmpeg.exe`/`whisper-cli.exe` 路径与 `kill_on_drop` 行为。

---

## 9. 验收 / 测试

```bash
cargo test -p wp-ai                 # 51 单测 + 4 doc test（本地 mock HTTP/伪造进程，不联网）
cargo test -p wp-ai --all-features  # 67 单测（+ 本地 LLM mock HTTP / 伪造 whisper-cli）
cargo clippy -p wp-ai --all-features --all-targets  # 0 warning
```

集成层新增单测覆盖：双语 ASS 生成（保留原 ASS 头/样式/译文轨）、漏译统计（空/同原文/降级）、
写入文件可被 `wp_subtitle::parse` 回读、缓存命中二次调用、service 事件→ASS 闭环、
伪造 ffmpeg 成功/参数正确/缺 CLI(NotFound)/非零退出含 stderr/超时 kill/无输出报错/缺输入、
真实 ffmpeg 端到端（有 ffmpeg 时跑，没有则跳过）。
