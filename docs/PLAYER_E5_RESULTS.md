# 播放器 E5 真机复测结果（M2 字幕 / E3 弹幕 / sub-reload）

> 任务：**task-25**「M2/E5 真机复测」——把 task-22 交付的副字幕正式命令、`MpvConfig::{fonts_dir, sub_auto}`
> 与既有 E3（ASS 弹幕副字幕轨）/ sub-reload 能力串起来，在**本机 headless（Xvfb + 软件 GL）**实测。
>
> 环境：JZ Linux / i7-1195G7（8 线程）/ Iris Xe / 15GB / **无物理 DISPLAY** → `Xvfb :99`（1280x800x24，
> Mesa **llvmpipe 软件 GL**）；运行时用 **M6 自包含 LGPL 运行时**
> （`WP_MPV_LIB=播放器运行时/lib/libmpv.so.2`，见 `docs/PLAYER_PACKAGING.md` §10）。
> 复测期间系统负载 load1 ≈ 9–12（其他任务并行），**所有帧率/CPU 为软件渲染+高负载口径，不代表真机 GPU**。
>
> 工具：`crates/wp-media-mpv/examples/render_probe`（本轮新增 `--secondary-sid` / `--fonts-dir` /
> `--sub-auto` / `--dump-props` / `--sub-reload` / `--reload-at-ms` 参数，见文末 §8）；
> 素材：`crates/wp-subtitle/examples/m2_probe_kit` 生成（`/tmp/wp-m0/e5/kit/assets/`）+ 自建受控 ASS。
> 证据目录：`出/验收截图/媒体内核-E5/`（`png/` 截图、`json/` 原始数据、`scripts/` 复现脚本）。

## 0. TL;DR

| # | 项 | 结论 |
|---|---|---|
| E5-1 | **副字幕正式命令** `PlayerCommand::SelectSecondarySubtitleTrack{id}` → `secondary-sid` | ✅ 生效：`props.secondary-sid="2"`、主+副 cue 同帧上报（prim=3/sec=3）、像素证据主字幕底/副字幕顶同屏；对照（不发命令）`secondary-sid="no"`、sec=0 |
| E5-2 | `MpvConfig{fonts_dir}` → `--sub-fonts-dir` | ✅ 生效：`props.sub-fonts-dir=/tmp/wp-m0/e5/fonts`；自定义目录 + DejaVu 字体 ASS 渲染正常（主字幕区白像素 2829） |
| E5-2b | `MpvConfig{sub_auto="fuzzy"}` 同名自动加载 | ✅ 生效：默认 `no` → tracks=2、无 cue；`fuzzy` → tracks=3（external `.srt` 选中）、cue=2；静态背景像素证明字幕出现（0 → 1788） |
| E5-3 | E3 复测：4K HEVC + 主字幕 + `m0-density-1000pm.ass` 副字幕（弹幕） | ✅ 无 GL/播放错误；12 帧/1.15fps/CPU 3.77 核/副字幕 **95 条 cue**（M0-E3 同口径 10 帧/1.05fps/3.27 核/52 条，趋势一致） |
| E5-4 | sub-reload / AI 重译注入 | ✅ 播放中改双语 ASS 译文 → `ReloadSubtitles` → cue 文本即时更新为「[AI 重译] …v2」（JSON 有前后对比） |
| — | `render_probe` 新参数 + `MpvEngine::mpv_property_string` | ✅ 已交付，`cargo fmt/check/test -p wp-media-mpv` 全绿 0 警告 |

**副字幕不再需要 `set_property("secondary-sid", ...)` 逃生门**：GUI 可直接用正式 `PlayerCommand`。

## 1. E5-1 副字幕正式命令（主 + 副同屏、cue 事件、像素）

**方法**（`render_probe`，GUI 同样调用）：

```rust
// 主字幕（select=true → sid=1）
engine.command(PlayerCommand::AddSubtitleFile { path: primary_ass, select: true });
// 副字幕/弹幕（select=false → 第 2 条字幕轨 id=2）
engine.command(PlayerCommand::AddSubtitleFile { path: secondary_ass, select: false });
// ✅ 正式命令（替代 M0 的 set_property 逃生门）
engine.command(PlayerCommand::SelectSecondarySubtitleTrack { id: 2 });
engine.command(PlayerCommand::SetSecondarySubtitleVisible(true));
```

**复现命令**：

```bash
export DISPLAY=:99 WP_MPV_LIB="$PWD/播放器运行时/lib/libmpv.so.2"
R=/home/xgl/共享基座/重活/构建/cargo-target/debug/examples/render_probe
$R /tmp/wp-m0/media/h264_1080p_30s.mp4 --seconds 6 --size 1280x720 --hwdec no \
   --sub /tmp/wp-m0/e5/assets/primary.ass --sub /tmp/wp-m0/e5/assets/secondary.ass \
   --secondary-sid 2 \
   --dump-props secondary-sid,secondary-sub-visibility,sub-fonts-dir,sub-auto \
   --screenshot-window /tmp/wp-m0/e5/shots/e5-1-secondary.ppm --json
# 对照：去掉 --secondary-sid（不发正式命令）
```

**数据**：

| 用例 | 状态 | 视频 | tracks | 渲染帧 | 出帧 | CPU(核) | 首帧 | 主字幕 cue | 副字幕 cue | 丢帧 | GL 错误 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| e5-1-secondary（发命令） | Playing | 1920x1080 | 4 | 39 | 5.90 fps | 4.20 | 358 ms | **3** | **3** | 137 | 0 |
| e5-1-control（不发命令） | Playing | 1920x1080 | 4 | 30 | 6.63 fps | 4.49 | 141 ms | 2 | **0** | 85 | 0 |

**属性诊断（`--dump-props`，引擎新增 `mpv_property_string`）**：

```text
e5-1-secondary: secondary-sid="2", secondary-sub-visibility="yes", sub-auto="no", sub-fonts-dir=""
e5-1-control  : secondary-sid="no", secondary-sub-visibility="yes"
```

**cue 事件样例（同一帧前后上报，从 `json.subtitle_cues`）**：

```text
[1.27s] SubtitleCue { start_ms: 500, text: "主字幕 PRIMARY-ONE 第一句", secondary: false }
[1.27s] SubtitleCue { start_ms: 500, text: "副字幕 SECONDARY-ALPHA 甲", secondary: true }
[2.84s] SubtitleCue { start_ms: 2600, text: "主字幕 PRIMARY-TWO 第二句", secondary: false }
[2.84s] SubtitleCue { start_ms: 2600, text: "副字幕 SECONDARY-BETA 乙", secondary: true }
```

**像素证据**（静态背景对照 `v-static-*`，同视频同窗口，隔离「只加副字幕轨」变量）：

| 截图 | 副字幕区白像素(x400-880,y6-50) | 主字幕区白像素(x400-900,y600-700) |
|---|---|---|
| `v-static-primary.png`（只有主字幕） | **0** | 2842 |
| `v-static-both.png`（主+副，正式命令） | **1415** | 2842（完全一致） |

→ 副字幕区从 0 变 1415、主字幕区不变 ⇒ **两轨在同一帧内由 libass 分别渲染**（副字幕顶部 `\pos(960,120)`，
主字幕底部居中），无覆盖冲突。

![主+副字幕同屏（静态背景对照）](../出/验收截图/媒体内核-E5/png/v-static-both.png)

另外 1080p 实测截图（不同视频时刻，视觉参考）：`e5-1-secondary.png`（副字幕区 1663 白像素）vs
`e5-1-control.png`（副字幕区 0）。

## 2. E5-2 `MpvConfig{fonts_dir}` 自定义字体目录

```rust
let config = MpvConfig { fonts_dir: Some("/tmp/wp-m0/e5/fonts".into()), ..Default::default() };
// → props::build_options() 组装 sub-fonts-dir（open/initialize 前设置）
```

```bash
$R /tmp/wp-m0/e5/kit/assets/static-10s.mp4 --seconds 3 --size 1280x720 --hwdec no \
   --sub /tmp/wp-m0/e5/assets/primary.ass --fonts-dir /tmp/wp-m0/e5/fonts \
   --dump-props sub-fonts-dir,sub-auto --screenshot-window /tmp/wp-m0/e5/shots/e5-2-fonts.ppm --json
```

| 用例 | 状态 | tracks | 渲染帧 | 出帧 | CPU | 首帧 | 属性诊断 | 主字幕区白像素 |
|---|---|---|---|---|---|---|---|---|
| e5-2-fonts | Playing | 3 | 48 | 14.65 fps | 3.95 | 94 ms | `sub-fonts-dir=/tmp/wp-m0/e5/fonts` | **2829**（字幕正常渲染） |

字体目录内放 `DejaVuSans.ttf` + `DejaVuSans-Bold.ttf`（`primary.ass` 的 `Fontname=DejaVu Sans`），
截图 `e5-2-fonts.png` 确认 ASS 用该字体正常渲染。

> 说明：本用例证明「配置 → mpv 选项生效 → ASS 渲染正常」链路；**尚未做「同名字体族仅存在于自定义目录」的
> 强行为断言**（需要改字体 name 表的工具，如 fontTools，本机未装）——列入下一轮（§7）。

![自定义字体目录渲染](../出/验收截图/媒体内核-E5/png/e5-2-fonts.png)

## 3. E5-2b `MpvConfig{sub_auto="fuzzy"}` 同名外挂字幕自动加载

素材：`/tmp/wp-m0/e5/subauto/movie.mp4` + 同目录 `movie.srt`（内容 `AUTO-SUB-ONE/TWO/THREE`）。

```bash
# 默认 no（MpvConfig::default）
$R /tmp/wp-m0/e5/subauto/movie.mp4 --seconds 5 --hwdec no --dump-props sub-auto --json
# fuzzy
$R /tmp/wp-m0/e5/subauto/movie.mp4 --seconds 5 --hwdec no --sub-auto fuzzy --dump-props sub-auto --json
```

| 用例 | `sub-auto` | tracks | 外挂 srt | 字幕 cue | 渲染帧 | 备注 |
|---|---|---|---|---|---|---|
| e5-2-subauto-no | `no` | **2** | 无 | **0** | 39 | 默认不自动加载 ✅ |
| e5-2-subauto-fuzzy | `fuzzy` | **3** | `external=true, codec=subrip, title=srt` | **2** | 32 | 自动加载并选中 ✅ |

**静态背景像素对照**（同视频同窗口，`v-subauto-*`）：

| 截图 | 主字幕区白像素 |
|---|---|
| `v-subauto-no.png`（默认 no） | **0** |
| `v-subauto-fuzzy.png`（fuzzy） | **1788** |

→ 默认 `no` 时画面完全没有字幕；`fuzzy` 时 srt 文本出现 ⇒ 配置生效、同名自动加载可用（T9 可直接接）。

![同名自动加载（fuzzy）](../出/验收截图/媒体内核-E5/png/v-subauto-fuzzy.png)

## 4. E5-3 E3 复测：4K + 主字幕 + ASS 弹幕副字幕轨

**夹具**：`出/验收截图/弹幕/m0-density-1000pm.ass`（1000 条/分钟、峰值同屏 129 条、122,948 B）
作为**副字幕轨**（第 2 条），`primary.ass` 为主字幕；视频 4K HEVC 10bit（3840x2160）。

```bash
$R /tmp/wp-m0/media/h265_4k_10s_10bit.mp4 --seconds 8 --size 640x360 --hwdec no \
   --sub /tmp/wp-m0/e5/assets/primary.ass --sub "$PWD/出/验收截图/弹幕/m0-density-1000pm.ass" \
   --secondary-sid 2 --dump-props secondary-sid,secondary-sub-visibility \
   --screenshot-window /tmp/wp-m0/e5/shots/e5-3-4k-danmaku.ppm --json
# 对照：不带弹幕轨
```

| 用例 | 状态 | 视频 | tracks | 渲染帧 | 出帧 | CPU(核) | 首帧 | 主 cue | 副 cue（弹幕） | 丢帧 | GL 错误 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| e5-3-4k-control（仅主字幕） | Playing | 3840x2160 | 2 | 11 | 1.19 fps | 4.10 | 494 ms | 4 | 0 | 147 | 0 |
| e5-3-4k-danmaku（主+弹幕） | Playing | 3840x2160 | 3 | 12 | 1.15 fps | 3.77 | 320 ms | 4 | **95** | 157 | 0 |

**与 M0-E3 对比**（同 640x360 窗口、Xvfb 软件 GL、`hwdec=no`）：

| 来源 | 用例 | 渲染帧 | 出帧 | CPU(核) | 副字幕 cue | 结论 |
|---|---|---|---|---|---|---|
| M0-E3（set_property 逃生门） | E3-1 1000pm | 10 | 1.05 fps | 3.27 | 52 | 主+副双轨可行，CPU 软渲染瓶颈 |
| **E5-3（正式命令）** | 1000pm | **12** | **1.15 fps** | **3.77** | **95** | 趋势一致；正式命令下同样工作，无 GL 错误 |

> cue 条数差异属**统计口径**（M0 用 `set_property` 逃生门 + 旧属性去重逻辑、窗口时长略短；本轮为主+副双轨
> 逐 cue 上报，完整 10s 段）。两轮共同结论不变：**4K + 千条/分钟弹幕在软件 GL 下 CPU 3–4 核、帧率 1fps 级**，
> 弹幕主路径仍推荐 **egui 覆盖层**（`wp-danmaku`），ASS 轨作为跨引擎/兜底。

![4K + 弹幕副字幕（官方命令）](../出/验收截图/媒体内核-E5/png/e5-3-4k-danmaku.png)

## 5. E5-4 sub-reload / AI 重译注入

**素材**：`wp-subtitle` 的 `m2_probe_kit` 生成的双语 ASS（`assets/bilingual.ass`，同一时间轴「原文 + AI 译文」
双样式；译语来自 `ai_translations.json`）。

**流程**：播放中注入双语 ASS → 在 cue 显示期间**改写文件译文**（`Welcome to this player` →
`[AI 重译] 欢迎使用本播放器 v2`）→ `PlayerCommand::ReloadSubtitles`（mpv `sub-reload`）→ 观察 cue 是否更新。

```bash
cp /tmp/wp-m0/e5/kit/assets/bilingual.ass /tmp/wp-m0/e5/assets/bilingual-reload.ass
$R /tmp/wp-m0/e5/kit/assets/static-10s.mp4 --seconds 6 --hwdec no \
   --sub /tmp/wp-m0/e5/assets/bilingual-reload.ass \
   --sub-reload "/tmp/wp-m0/e5/assets/bilingual-reload.ass|Welcome to this player|[AI 重译] 欢迎使用本播放器 v2" \
   --reload-at-ms 1500 --json
```

**实测（`json.sub_reload`）**：

```text
at_secs = 1.54s，cues_before = 1
cues_after_reload:
  SubtitleCue { start_ms: 1000, text: "欢迎使用本播放器\n[AI 重译] 欢迎使用本播放器 v2", secondary: false }  ← 新译文
  SubtitleCue { start_ms: 3500, text: "字幕支持双语显示\nSubtitles support bilingual display", secondary: false }
```

完整 cue 序列：`[1.08s] 旧译文 Welcome to this player` → `[1.56s] 新译文 [AI 重译] …v2` → `[3.57s] 下一句`
⇒ ✅ **改文件 + `ReloadSubtitles` 后字幕内容即时更新**（AI 重译/在线翻译回填的完整数据流可用）。

![双语 ASS + AI 重译注入（截图时刻为后续 cue）](../出/验收截图/媒体内核-E5/png/e5-4-reload.png)

## 6. 截图索引（`出/验收截图/媒体内核-E5/png/`）

| 文件 | 说明 |
|---|---|
| `e5-1-secondary.png` / `e5-1-control.png` | 1080p 主+副同屏（正式命令）/ 对照（无副字幕） |
| `v-static-primary.png` / `v-static-both.png` | **静态背景**：只有主字幕 / 主+副（像素证据） |
| `e5-2-fonts.png` | `fonts_dir` 自定义字体目录渲染 |
| `e5-2-subauto-no.png` / `e5-2-subauto-fuzzy.png` | 同名自动加载：默认 no / fuzzy |
| `v-subauto-no.png` / `v-subauto-fuzzy.png` | 静态背景同名自动加载像素对照 |
| `e5-3-4k-control.png` / `e5-3-4k-danmaku.png` | 4K 仅主字幕 / 主字幕+ASS 弹幕副字幕 |
| `e5-4-reload.png` | 双语 ASS 播放截图（reload 数据见 JSON） |

原始数据：`出/验收截图/媒体内核-E5/json/*.json`（每个用例的 stderr 也有记录）；
复现脚本：`出/验收截图/媒体内核-E5/scripts/{run-e5.sh,run-visual.sh,make-assets.py}`。

## 7. 结论与下一轮

**结论**：

1. ✅ **副字幕正式命令可用**（`SelectSecondarySubtitleTrack` 写 `secondary-sid`，主+副 cue 分别上报、同帧渲染），
   M0 的 `set_property` 逃生门可移除；
2. ✅ `MpvConfig{fonts_dir}` / `sub_auto` 已真正作用于 mpv（属性诊断 + 渲染/自动加载行为双证据）；
3. ✅ E3「主字幕 + ASS 弹幕副字幕」在正式命令下复测通过，性能与 M0 一致（软件 GL 瓶颈结论不变）；
4. ✅ **sub-reload 可用**：AI 重译回填「改文件 → ReloadSubtitles → cue 更新」链路实测通过；
5. ⚠️ 全部数据为 **Xvfb + llvmpipe 软件 GL + 高负载**口径，真机 GPU 指标待任务 3/GUI 侧复测。

**下一轮（不阻塞本任务）**：

| # | 项 | 说明 |
|---|---|---|
| 1 | 字体「强行为」断言 | 用 fontTools 造一个 name 表唯一的字体（如 `WP-E5-Font`）只放 `fonts_dir`，断言 `fontselect`/像素与 fallback 不同；当前仅证明选项生效+渲染正常 |
| 2 | 真机 GPU 复测 | X11/Wayland + Iris Xe EGL 下重跑 E5-1/E5-3，记录帧率/CPU/GPU；VAAPI 零拷贝 |
| 3 | `sub-reload` 截图时刻 | 当前截图在退出时刻；可加 `--screenshot-at-ms` 在 reload 生效瞬间取证（JSON 已足够） |
| 4 | 弹幕密度阈值 | 结合 `wp-danmaku` 覆盖层做 1000/5000/百万级真机压测，定 ASS 轨 vs egui 覆盖层切换阈值 |
| 5 | PGS 位图字幕 | 需要 PGS 样本（subtitle-danmaku 可提供）；本轮未覆盖 |

## 8. 本轮工具改动（`crates/wp-media-mpv`）

`examples/render_probe.rs` 新增参数（向后兼容）：

```text
--secondary-sid <N>   用正式 PlayerCommand::SelectSecondarySubtitleTrack 选副字幕轨
--fonts-dir <dir>     MpvConfig.fonts_dir（自定义字幕字体目录）
--sub-auto <mode>     MpvConfig.sub_auto（no/exact/fuzzy/all，默认 no）
--dump-props k1,k2    退出前读取 mpv 属性写入 JSON（诊断；如 sub-fonts-dir,sub-auto,secondary-sid）
--sub-reload <path>|<old>|<new>  播放中改字幕文本 → ReloadSubtitles → 验证 cue 更新
--reload-at-ms <N>    sub-reload 触发时刻（默认 1500ms）
```

JSON 新增字段：`tracks_detail`、`secondary_subtitle_track`、`primary_cues`/`secondary_cues`、
`secondary_command_sent`、`props`、`sub_reload`。
`src/engine.rs` 新增 `MpvEngine::mpv_property_string(&self, name) -> Option<String>`（走引擎线程读属性，
**必须在 `shutdown()` 前调用**——诊断读属性后 `engine.shutdown()` 会停 worker）。

**验证**：`cargo fmt --check` / `cargo check -p wp-media-mpv --all-targets` 0 警告 /
`cargo test -p wp-media-mpv` 全绿（见任务报告）。**不 git commit**。

---

## 附录：用例 → 命令 → 产物

| 用例 | 命令关键参数 | JSON | 截图 |
|---|---|---|---|
| E5-1 | `--sub primary.ass --sub secondary.ass --secondary-sid 2` | `json/e5-1-secondary.json` | `e5-1-secondary.png` |
| E5-1 对照 | 去掉 `--secondary-sid` | `json/e5-1-control.json` | `e5-1-control.png` |
| E5-2 字体 | `--fonts-dir /tmp/wp-m0/e5/fonts --dump-props sub-fonts-dir` | `json/e5-2-fonts.json` | `e5-2-fonts.png` |
| E5-2b 自动加载 | `--sub-auto fuzzy` vs 默认 | `json/e5-2-subauto-*.json` | `e5-2-subauto-*.png` / `v-subauto-*.png` |
| E5-3 弹幕 | `--sub primary.ass --sub m0-density-1000pm.ass --secondary-sid 2` | `json/e5-3-4k-danmaku.json` | `e5-3-4k-danmaku.png` |
| E5-4 reload | `--sub-reload "…|Welcome to this player|[AI 重译] …v2"` | `json/e5-4-reload.json` | `e5-4-reload.png` |
