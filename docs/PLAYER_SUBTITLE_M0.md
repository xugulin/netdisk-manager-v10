# PLAYER_SUBTITLE_M0 —— M2 字幕真机联调（外挂 / 内嵌 / 主副 / AI 双语）

> 对应任务：**task-17（M2 字幕真机联调）**；上游：task-3（wp-subtitle 内核）、task-8（T11 双语注入辅助）、M0-E3（media-core render_probe）
> 验证方式：**media-core `render_probe`（Mode A / libmpv render API）+ Xvfb + mpv v0.41.0** 真渲染截图，不是 mock
> 日期：2026-10-19 · 环境：Linux / Xvfb `:99` 1280x800x24 / llvmpipe 软件 GL / `--hwdec no`
> 素材生成器：`cargo run -p wp-subtitle --example m2_probe_kit`（一条命令重建全部素材 + 矩阵脚本）
> 证据目录：`出/验收截图/字幕弹幕/字幕联调/`（`assets/` 素材、`raw/` JSON+日志+PPM、`screens/` PNG、`raw/case-metrics.txt` 像素指标）
> 代码：本任务只新增示例/文档与证据，未改 `wp-subtitle`/`wp-danmaku` 既有 API 语义
>
> **更新（task-22 已补齐本报告 §5 的两个接口缺口）**：`PlayerCommand::SelectSecondarySubtitleTrack { id }`（mpv `secondary-sid`，id=-1 关闭）+ `MpvConfig::{fonts_dir, sub_auto}`（由 `props::build_options()` 组装 `--sub-fonts-dir` / `--sub-auto`）已进 `wp-media`/`wp-media-mpv`，含 Mock/纯映射/真机 libmpv 属性测试；GUI 字幕面板已接「副字幕轨下拉 + 字体目录入口」。

---

## 0. 结论先行

| # | 能力 | 结论 | 关键证据（像素 AE = 与同帧无字幕控制组的不同像素数） |
|---|---|---|---|
| 1 | 外挂 **SRT**（中/日/emoji） | ✅ 显示正确 | `screens/01-srt.png`，底部字幕带 AE=1821 |
| 2 | 外挂 **VTT**（NOTE/cue id/settings） | ✅ 显示正确 | `screens/02-vtt.png`，AE=2241 |
| 3 | 外挂 **ASS**（大字号 72px / 黄色 / 黑描边） | ✅ 样式生效 | `screens/03-ass-styled.png`，AE=10575 |
| 4 | ASS **`\pos` 定位**（顶部 (640,120)） | ✅ 位置生效 | `screens/04-ass-pos.png`，顶带 AE=3308、底带 0 |
| 5 | **主 + 副字幕叠加**（同屏两条） | ✅ 同帧渲染 | `screens/show-05-main-secondary.png`、`05-main-secondary.png`（底 2507 / 顶 2981） |
| 6 | **内嵌 SRT mkv**（default 轨） | ✅ mpv 自动选中并显示 | `screens/11-embedded-default.png`，AE=1821 |
| 7 | **内嵌 SRT mkv**（非 default 轨） | ✅ 需显式选轨 `sid=1`（mpv 默认 `sid=no`，实测） | `screens/15-embedded-explicit-sid.png`，AE=1821 |
| 8 | **AI 双语注入**（无真 key，给定译文 → wp-ai integration） | ✅ 原文/译文双行、时间轴严格一致 | `screens/10-ai-bilingual.png`、`show-10-ai-bilingual.png`，AE=1928；回读 6 events、3/3 配对 |
| 9 | 样式覆盖 `sub-ass-override=force` + `sub-font-size=90` + `sub-color=#FF00FF` | ✅ | `screens/06-style-override.png`，AE=14310（默认样式 AE=10575） |
| 10 | 字体覆盖 `sub-font="Noto Serif CJK SC"` | ✅ serif 生效 | `screens/09-font-serif.png`，与默认样式差异 AE=12371 |
| 11 | 位置 `sub-pos=80`（默认 100=底部） | ✅ 字幕上移到 y≈520 | `screens/13-sub-pos-up.png`，mid 带 AE=1827 |
| 12 | 延迟 `sub-delay=2.0s` | ✅ 同 seek 下第一句被推迟消失；另一处第二句出现 | `screens/07-delay-baseline.png`(有) / `07b-delay-2s-same.png`(空) / `08-delay-2s-second.png`(第二句) |
| 13 | 字幕开关 `sub-visibility=no` | ✅ 整条隐去（AE=0） | `screens/12-sub-visibility-off.png` |
| 14 | 副字幕开关 `secondary-sub-visibility=no` | ✅ 只关副字幕，主字幕保留 | `screens/14-secondary-off.png`（底 2507 / 顶 0） |
| 15 | egui UI 与视频/字幕**同帧合成** | ✅（Mode A 叠加层可用） | `screens/show-10-window.png`（红色 `UI OVERLAY 同帧合成` 角标 + 视频 + 双语字幕） |
| — | **PGS/VobSub 位图字幕** | ⚠ 本机无样本，**未测**（需蓝光样本，见 §6） | — |
| — | ASS 内嵌字体（`[Fonts]` 段） | ⚠ 未做样本（样式覆盖已用 `sub-font` 验证字体路径）；`--sub-fonts-dir` 引擎入口已由 task-22 补齐（`MpvConfig.fonts_dir`） | — |

**一句话结论**：libmpv/libass 在本机 Xvfb + 软件 GL 下对 SRT/VTT/ASS **外挂、内嵌、主副叠加、样式/字体/位置/延迟/开关、以及 wp-ai 生成的双语 ASS 全部显示正确**；M2 字幕面板所需的操作路径除「副字幕轨选择」外都已有正式 `PlayerCommand`（见 §5），可进入 GUI 集成。

---

## 1. 环境与复现

| 项 | 值 |
|---|---|
| 播放器工具 | `crates/wp-media-mpv/examples/render_probe.rs`（Mode A：libmpv render API → egui/GL FBO） |
| mpv | `mpv v0.41.0（client API 2.5）`，系统 `libmpv.so.2.5.0` |
| 显示 | `Xvfb :99 -screen 0 1280x800x24`，llvmpipe 软件 GL；单次运行窗口 1280×720 |
| 解码 | `--hwdec no`（软件解码）：本报告只验证「字幕显示正确」，性能不代表真机 GPU 路径 |
| 视频 | ffmpeg 生成 10s 1280×720 30fps + 440Hz 音频：`test-10s.mp4`（testsrc2 彩条，展示用）、`static-10s.mp4`（静态背景，像素差分用） |
| 字幕 | 由 `m2_probe_kit` 生成（中文/日文/emoji、ASS 大字号样式、主+副、延迟、AI 双语） |

### 1.1 一键重建素材（可复现套件）

```bash
# 在 网盘管理_V10/ 下；默认写到 出/验收截图/字幕弹幕/字幕联调/
cargo run -p wp-subtitle --example m2_probe_kit            # 生成 assets/ + run_matrix.sh + README
cargo run -p wp-subtitle --example m2_probe_kit -- --run   # 生成后直接跑矩阵（需 Xvfb :99）
```

前置：`cargo build -p wp-media-mpv --example render_probe`；`Xvfb :99 -screen 0 1280x800x24 &`；ffmpeg + ImageMagick。

### 1.2 跑矩阵

```bash
cd 出/验收截图/字幕弹幕/字幕联调
DISPLAY=:99 bash run_matrix.sh          # 19 个用例，约 5 分钟（同步输出 + raw/case-metrics.txt）
```

### 1.3 单条用例命令模板

```bash
DISPLAY=:99 cargo run -p wp-media-mpv --example render_probe -- \
  assets/static-10s.mp4 --size 1280x720 --hwdec no --seconds 5.5 --seek 1.2 \
  --sub assets/zh-ja-emoji.srt \
  --screenshot raw/01-srt.ppm --json > raw/01-srt.json
magick raw/01-srt.ppm screens/01-srt.png
```

说明：
- `--screenshot` 抓的是 **mpv 视频 FBO**（libass 字幕已烧进视频帧，因此截图能证明字幕真的渲染出来了）；`--screenshot-window` 抓 egui 窗口（含 UI 叠加层）。
- `--seconds 5.5` 是为绕过「seek 生效前就退出」的竞态（1.2s 时首帧还没来）；脚本另有黑帧/字幕未出现的自动重试。
- `--sub` 可多次注入（第一条 `select=true`）；`--prop k=v` 是 mpv 属性逃生门（引擎 `MediaEngine::set_property`）。

---

## 2. 结果矩阵

### 2.1 像素差分矩阵（静态背景 ⇒ 差值即字幕像素）

控制组 = 同 seek、无字幕、同视频；AE 为 `ImageMagick compare -metric AE` 在不同带内的差异像素数。
`bottom` = 主字幕带 y500–720；`top` = 顶部带 y0–180（x>340 避开时间戳）；`mid` = 上移带 y440–600（sub-pos 用例）。
完整数据：`出/验收截图/字幕弹幕/字幕联调/raw/case-metrics.txt`。

| # | 用例 | seek(s) | 关键参数 | bottom AE | top AE | mid AE | 结论 |
|---|---|---|---|---|---|---|---|
| 1 | `01-srt` | 1.2 | `--sub zh-ja-emoji.srt` | **1821** | 0 | 0 | ✅ |
| 2 | `02-vtt` | 3.8 | `--sub zh-ja-emoji.vtt` | **2241** | 0 | 0 | ✅ |
| 3 | `03-ass-styled` | 1.3 | `--sub styled.ass`（72px 黄/描边） | **10575** | 0 | 0 | ✅ |
| 4 | `04-ass-pos` | 4.8 | `--sub styled.ass`（`\pos(640,120)`） | 0 | **3308** | 0 | ✅ 位置生效 |
| 5 | `05-main-secondary` | 1.2 | 两次 `--sub` + `secondary-sid=2` + `secondary-sub-visibility=yes` | **2507** | **2981** | 0 | ✅ 双轨同屏 |
| 6 | `06-style-override` | 1.3 | `sub-ass-override=force` + `sub-font-size=90` + `sub-color=#FF00FF` | **14310** | 0 | 0 | ✅ 覆盖生效（默认 10575） |
| 7 | `07-delay-baseline` | 1.2 | `--sub delay.srt`（cue1 1–3s） | **1731** | 0 | 0 | ✅ 第一句可见 |
| 8 | `07b-delay-2s-same` | 1.2 | 同上 + `sub-delay=2.0` | 0 | 0 | 0 | ✅ 同帧第一句被推迟（空白） |
| 9 | `08-delay-2s-second` | 3.1 | `sub-delay=2.0`（effective≈6.1s → cue2 5–7s） | **1731** | 0 | 0 | ✅ 第二句出现 |
| 10 | `09-font-serif` | 1.3 | `sub-font="Noto Serif CJK SC"` + override | **9326** | 0 | 0 | ✅ serif 生效 |
| 11 | `10-ai-bilingual` | 1.2 | `--sub bilingual.ass`（wp-ai 生成） | **1928** | 0 | 0 | ✅ 原文+译文两行 |
| 12 | `11-embedded-default` | 1.2 | 内嵌 mkv（sub 轨 default） | **1821** | 0 | 0 | ✅ 自动选中 |
| 13 | `12-sub-visibility-off` | 1.2 | `--sub` + `sub-visibility=no` | 0 | 0 | 0 | ✅ 整条隐去 |
| 14 | `13-sub-pos-up` | 1.2 | `--sub` + `sub-pos=80` | **1827** | 0 | **1827** | ✅ 上移到 y≈520 |
| 15 | `14-secondary-off` | 1.2 | 主+副 + `secondary-sub-visibility=no` | **2507** | 0 | 0 | ✅ 只关副字幕 |
| 16 | `15-embedded-explicit-sid` | 1.2 | 内嵌 mkv（sub 轨非 default）+ `sid=1` | **1821** | 0 | 0 | ✅ 显式选轨 |

对照（不同内容/样式之间的差异，佐证内容真的不同）：

| 对照 | AE |
|---|---|
| 延迟 0s vs 2s（同 seek） | 2059 |
| 延迟后第一句 vs 第二句 | 551 |
| ASS 默认样式 vs serif 字体 | 12371 |
| ASS 默认样式 vs 强制覆盖样式 | 21483 |
| 主+副字幕 vs 关闭副字幕 | 3286 |
| SRT 默认位置 vs `sub-pos=80` | 4279 |
| 外挂 SRT vs 内嵌 default（同内容不同轨道） | 451 |
| 内嵌 default vs 非 default 显式选轨（同内容） | 568 |

### 2.2 展示用例（testsrc2 彩条真实画面，人工可读）

| 截图 | 内容 |
|---|---|
| `screens/show-01-srt.png` | 彩条画面 + 中文 SRT 字幕 |
| `screens/show-03-ass-styled.png` | 彩条画面 + ASS 大字号黄字 |
| `screens/show-05-main-secondary.png` | 彩条画面 + 底部主字幕 + 顶部绿色副字幕 |
| `screens/show-10-ai-bilingual.png` | 彩条画面 + 「欢迎使用本播放器」+「Welcome to this player」双语两行 |
| `screens/show-10-window.png` | **egui 窗口截图**：视频 + 字幕 + 红色「UI OVERLAY 同帧合成」角标（证明 Mode A 覆盖层可叠加） |

---

## 3. 关键截图（点击相对路径查看）

| 主+副字幕叠加 | AI 双语注入 |
|---|---|
| ![主副字幕](../出/验收截图/字幕弹幕/字幕联调/screens/show-05-main-secondary.png) | ![AI 双语](../出/验收截图/字幕弹幕/字幕联调/screens/show-10-ai-bilingual.png) |

| 内嵌字幕（非 default 显式选轨） | ASS 样式覆盖 |
|---|---|
| ![内嵌字幕](../出/验收截图/字幕弹幕/字幕联调/screens/15-embedded-explicit-sid.png) | ![样式覆盖](../出/验收截图/字幕弹幕/字幕联调/screens/06-style-override.png) |

| egui 同帧合成 | 延迟（第二句出现） |
|---|---|
| ![同帧合成](../出/验收截图/字幕弹幕/字幕联调/screens/show-10-window.png) | ![延迟](../出/验收截图/字幕弹幕/字幕联调/screens/08-delay-2s-second.png) |

全部 26 张：`出/验收截图/字幕弹幕/字幕联调/screens/*.png`。

---

## 4. 关键结论详解

### 4.1 外挂 SRT / VTT / ASS（含 `\pos`、样式）

- 注入路径：`PlayerCommand::AddSubtitleFile { path, select: true }` → mpv `sub-add <path> select`（render_probe 用同一命令，`--sub` 第一次 select，后续 auto）。
- SRT（含多行/HTML/emoji）与 VTT（NOTE/cue id/cue settings）均按默认字幕样式显示；文本与时间轴正确。
- ASS 自带样式全部保留：**72px 字号、黄色 primary、黑色描边、阴影、`\N` 换行、`\pos(640,120)` 定位、`\fad` 淡入淡出**。
- VTT 的 cue settings（`line:90% align:center` 等）本次未做逐项位置断言（只验证显示/文本/时间）；下轮可做 VTT settings 矩阵。

### 4.2 主 + 副字幕叠加（实测操作序列）

```text
1) 注入主字幕：AddSubtitleFile(primary.srt, select=true)        → sid=1（选中）
2) 注入副字幕：AddSubtitleFile(secondary.ass, select=false)      → sid=2（不选中）
3) set_property("secondary-sid", "2")                            → 副字幕轨 = 第 2 条
4) set_property("secondary-sub-visibility", "yes")              → 显示副字幕
   结果：底部主字幕（SRT 默认样式）+ 顶部绿色副字幕（ASS 样式）同帧渲染
```
- 截图 `05-main-secondary.png`：底带 AE=2507、顶带 AE=2981，两带同时出现字幕像素。
- 关闭副字幕（`secondary-sub-visibility=no`，`14-secondary-off.png`）：底带仍 2507、顶带 0 → 只影响副字幕，不误伤主字幕。
- ✅ **task-22 已补正式命令**：`PlayerCommand::SelectSecondarySubtitleTrack { id }`（wp-media-mpv 侧写 mpv `secondary-sid`，`id=-1` → `"no"` 关闭，并回写快照 `secondary_subtitle_track`）；GUI 字幕面板已接「副字幕轨下拉」。本报告早期验证时用的是 `set_property("secondary-sid", ...)` 逃生门，两者等价。

### 4.3 样式覆盖 / 字体 / 位置 / 延迟 / 开关

| 操作 | mpv 属性（`--prop`） | 实测结果 |
|---|---|---|
| 忽略 ASS 内样式、强制覆盖 | `sub-ass-override=force` | `06-style-override.png`：字号 90、洋红色生效 |
| 字号 / 颜色 / 字体 | `sub-font-size=90`、`sub-color=#FF00FF`、`sub-font=Noto Serif CJK SC` | 均即时生效（无需重载字幕） |
| 垂直位置 | `sub-pos=80`（默认 100=底部） | 字幕上移到 y≈520（`13-sub-pos-up.png`） |
| 延迟 | `sub-delay=2.0` | 同 seek 下 0s 显示第一句、2s 延迟后空白；另一位置 2s 延迟后显示第二句（`07/07b/08`） |
| 字幕开关 | `sub-visibility=no` | 整条隐去（AE=0） |
| 副字幕开关 | `secondary-sub-visibility=no` | 只关副字幕 |

> `sub-ass-override` 取值：`no`（保留原样式）/ `yes`（覆盖但保留字体）/ `force`（全部覆盖）/ `scale`；本次用 `force` 做最强调验证。GUI 的「样式覆盖」开关可直接映射这 4 档。

### 4.4 内嵌字幕：为什么需要显式选轨

- `ffmpeg -i video -i sub.srt -map 0 -map 1 -c copy -c:s srt` 把 SRT 封装为 mkv 第 3 条 stream（`subrip`，标题「中文测试字幕」）。
- **实测 mpv 默认 `sid=no`**：内嵌字幕轨在 `track-list` 里可见（引擎快照 `tracks=3`，GUI 可列出 `TrackInfo{kind:Subtitle, title:"中文测试字幕", lang:"chi", codec:"subrip"}`），但若无 `default`/`forced` 标记，mpv 不会自动选中 → 画面无字幕（本报告第一轮就踩到，作为反例保留在 `raw/case-metrics.txt` 的历史笔记里）。
- 解决路径（两条均已实测通过）：
  1. 封装时标记 default：`-disposition:s:0 default` → mpv 自动选中（`11-embedded-default.png`）；
  2. 运行时显式选轨：`SelectTrack { kind: Subtitle, id }` → `sid=<id>`（`15-embedded-explicit-sid.png`，引擎侧 `set_property("sid","1")`）。
- 注意引擎默认 `sub-auto=no`（`crates/wp-media-mpv/src/props.rs`）：它只关闭**外挂同名/同目录自动加载**，不影响内嵌轨；内嵌轨的自动选中由 `default` disposition 决定。
- **GUI 结论**：字幕面板必须提供「内嵌字幕轨列表 + 手动选择」，不能假设自动出字幕；`snapshot.tracks` 已含所需字段（`id/kind/title/lang/codec/default/selected/external`）。

### 4.5 AI 双语注入端到端（无真 key）

链路（本次实际执行）：

```text
ai_source.srt（3 句中文）
  └─ wp_ai::integration::translate_to_bilingual_ass(doc, &translated[3], &AssOptions{layout: TranslatedBelow, margin_v:40})
       ├─ matched=3, missing=0
       └─ bilingual.ass（6 个 Dialogue：每句「原文(Default, MarginV=100) + 译文(AI-Translated, MarginV=40)」）
  └─ render_probe --sub assets/bilingual.ass
       └─ 截图：中文上行 + 英文下行（同一时间轴）
```

- 回读校验（`wp_subtitle::parse`）：事件数 6 == 原文 3 + 译文 3；逐句断言 `原文.start/end == 译文.start/end`（3/3 通过）。
- 真实 `wp-ai` 远端 Provider 只需把 `translated: Vec<String>` 换成 pipeline 返回的等长译文列表，**不需要真 key 即可验证注入端**；本次即用「给定译文数组 + mock 语义」完成。
- 与 `bilingual_from_json` 示例（task-8）一致：该示例是同一 API 的纯字幕版演示。

### 4.6 为什么用「静态背景 + 无字幕控制组」做像素断言

- render_probe 的 `--seek` 有竞态：退出可能发生在 seek 生效前后（截图位置 = seek 或 seek+~1.5s），testsrc2 每帧图案都在变 → 跨进程截图无法逐像素对比。
- 静态背景视频（纯色 + 左上角时间戳）下，任意两帧除了时间戳外完全相同 → 与「同 seek 无字幕」控制组做字幕带 AE 差分，**差值就是字幕像素**；时间戳在 y<80 且 top 带已从 x>340 起裁掉，不干扰。
- 差分用例 seek 都落在字幕窗口内部（如 SRT cue 1–3s 用 seek 1.2），保证两种竞态下都能看到字幕；脚本对「黑帧 / 期望有字幕但 AE=0」自动重试（最多 3 次），因此同一脚本可重复得到稳定证据。

---

## 5. 给 GUI 字幕面板的「真实可用操作清单」

> 依据：本次真机实测 + 现有 `wp_media::PlayerCommand` / `MediaEngine::set_property` 代码路径。
> ✅ = 本报告截图像素验证；🟡 = 代码路径存在但本报告未直接截图验证；⚠ = 当前无正式命令，需逃生门/后续补。

| GUI 功能 | 建议实现（正式 API 优先） | 状态 |
|---|---|---|
| 打开外挂字幕 SRT/VTT/ASS | `PlayerCommand::AddSubtitleFile { path, select }`（mpv `sub-add`） | ✅ |
| AI 双语/翻译后字幕注入 | `wp_ai::integration::translate_to_bilingual_ass` → 写临时文件 → `AddSubtitleFile` | ✅（本报告 §4.5） |
| 更新已注入字幕内容（AI 重译） | `PlayerCommand::ReloadSubtitles`（mpv `sub-reload`） | 🟡 代码路径存在，本报告用重启进程代替验证 |
| 字幕开关 | `set_property("sub-visibility", "yes/no")`；或 `SelectTrack{kind:Subtitle, id:-1}`（`sid=no`） | ✅（`sub-visibility`） |
| 内嵌字幕轨列表 | `snapshot.tracks` 过滤 `kind == TrackKind::Subtitle`，展示 `title/lang/codec/default/selected/external` | ✅（快照 `tracks=3`，含 `subrip`/`chi`/标题） |
| 选择内嵌字幕轨 | `PlayerCommand::SelectTrack { kind: Subtitle, id }`（mpv `sid=<id>`） | ✅（`sid=1` 显式选轨截图） |
| 主字幕延迟 ±s | `PlayerCommand::SetSubtitleDelay(f64)`（mpv `sub-delay`） | ✅（本报告 `sub-delay=2.0`） |
| 副字幕开关 | `PlayerCommand::SetSecondarySubtitleVisible(bool)`（`secondary-sub-visibility`） | ✅ |
| **选择副字幕轨** | `PlayerCommand::SelectSecondarySubtitleTrack { id }`（`id=-1` 关闭；mpv `secondary-sid`） | ✅（task-22 正式命令；逃生门等价实测 + Mock/真机属性测试） |
| 字体目录（`--sub-fonts-dir`） | `MpvConfig.fonts_dir: Option<PathBuf>`（open 前组装；重建引擎生效） | ✅（task-22 新增，`props::build_options` 单测覆盖） |
| 外挂字幕自动加载 | `MpvConfig.sub_auto: String`（默认 `"no"`；可 `fuzzy`/`exact`/`all`） | ✅（task-22 新增，默认行为不变） |
| 字号 | ⚠ `set_property("sub-font-size", px)` | ✅ |
| 颜色（前景/描边） | ⚠ `set_property("sub-color", ...)` / `sub-border-color` | ✅（`sub-color`） |
| 字体 | ⚠ `set_property("sub-font", name)`（需系统有该字体） | ✅（serif/默认对比） |
| 垂直位置 | ⚠ `set_property("sub-pos", 0..100)`（100=底部默认） | ✅ |
| 样式覆盖档位（ASS） | ⚠ `set_property("sub-ass-override", no|yes|force|scale)` | ✅（`force`） |
| 字幕速度/缩放/边距 | ⚠ `sub-speed` / `sub-scale` / `sub-margin-y` 等 | 🟡 未逐项实测（下轮补） |
| 截图（含字幕） | `PlayerCommand::Screenshot { path }`（mpv `screenshot-to-file`） | 🟡 未直接验证该命令（render_probe 用 FBO 直读截图，字幕已烧入） |
| 轨道变化刷新 | 引擎 `track-list` 属性观察 → `snapshot.tracks` 刷新 | 🟡 由 media-core 已有逻辑提供 |

**两个缺口已在 task-22 补齐（现为正式 API，不再需要 escape hatch）**：
1. `PlayerCommand::SelectSecondarySubtitleTrack { id }`：wp-media 枚举 + Mock 快照、wp-media-mpv `secondary-sid` 实现 + 快照回写；测试：Mock 命令→快照 / 纯映射（-1→"no"）/ **真机 libmpv 属性→快照**；
2. `MpvConfig::{fonts_dir: Option<PathBuf>, sub_auto: String}`：`props::build_options()` 真正组装 `sub-fonts-dir`（空路径忽略）与 `sub-auto`（默认 `"no"`，行为不变；T9 同名自动加载可设 `fuzzy`），单测覆盖默认/可配/空路径；GUI 字幕面板已加字体目录入口（提示重建引擎生效）。

---

## 6. 限制与下一步（不含糊）

| # | 限制 | 说明 / 下一步 |
|---|---|---|
| 1 | **PGS/VobSub/DVB 位图字幕未测** | 本机无蓝光/内嵌位图样本。mpv 走 `sd_lavc` 解码后由 render API 混合，预期可用；**下轮（E5/蓝光任务）**用 ISO/BDMV 实测并补截图。 |
| 2 | ASS 内嵌字体（`[Fonts]` 段）未做样本 | 本次用 `sub-ass-override` + `sub-font` 验证字体覆盖路径。字体嵌入/提取属 T9，下轮加「带 [Fonts] 的 ASS」样本与 `--sub-fonts-dir` 验证。 |
| 3 | `ReloadSubtitles`（sub-reload）未截图验证 | 本次每用例独立进程；AI 重译更新场景需 GUI 集成后验证（改临时文件 → sub-reload → 画面更新）。 |
| 4 | ~~副字幕轨选择无正式命令~~ **已解决（task-22）** | `SelectSecondarySubtitleTrack` 已进 `PlayerCommand`，GUI 字幕面板已接副字幕轨下拉；后续只需真机 GUI 冒烟。 |
| 5 | 软件 GL / 软解，性能数字无效 | 本报告只证明字幕正确性；4K/真机 GPU/HDR 字幕亮度属 E5/M6 性能任务。 |
| 6 | VTT cue settings 位置未逐项断言 | 只验证了显示/文本/时间；下轮加 VTT `line/align/position` 矩阵。 |
| 7 | 双语 ASS 原文/译文颜色相同 | 用样式名 + MarginV 区分上下两行；如需颜色区分可扩展 `AssOptions`（如 translated_primary_color）。 |
| 8 | 本机字体依赖 | ASS 用的 `Noto Sans CJK SC` 命中系统字体；缺字体机器靠 mpv/libass fallback。 |
| 9 | 引擎默认 `sub-auto=no` | 外挂同名自动加载在引擎层是关闭的（设计为上层显式注入）；T9「同名自动加载」需引擎加配置或 GUI 自己扫目录。 |

---

## 附录 A：产物清单

| 路径 | 内容 |
|---|---|
| `出/验收截图/字幕弹幕/字幕联调/assets/`（14 项） | 字幕素材 + 双语 ASS + 2 段 10s 视频 + 2 个内嵌 mkv |
| `出/验收截图/字幕弹幕/字幕联调/screens/`（26 张 PNG） | 15 个差分用例 + 4 个彩条展示 + 控制组 + egui 窗口截图 |
| `出/验收截图/字幕弹幕/字幕联调/raw/`（25 JSON + 26 PPM + 日志） | render_probe `--json` 快照/字幕 cue 事件、stderr、原始 PPM |
| `出/验收截图/字幕弹幕/字幕联调/raw/case-metrics.txt` | 字幕带像素 AE 矩阵（本报告 §2.1 数据源） |
| `出/验收截图/字幕弹幕/字幕联调/run_matrix.sh` | 完整可重跑矩阵脚本（含重试与指标计算） |
| `crates/wp-subtitle/examples/m2_probe_kit.rs` | 素材 + 脚本一键生成器（`--run` 可直接跑矩阵） |
| `crates/wp-subtitle/tests/fixtures/matrix/` | 字幕格式矩阵夹具（task-10，本报告素材的前身） |

## 附录 B：与其它任务的关系

- **M0-E3（media-core）**：已证明 ASS 弹幕轨可作副字幕轨；本报告补充了「主字幕 + 副字幕」的真实字幕叠加、样式/延迟/字体/位置矩阵与内嵌字幕选轨。
- **task-8（T11 注入辅助）**：`write_bilingual_ass_from_doc` 等 API 已就绪；本报告用 wp-ai integration 走通「翻译结果 → 双语 ASS → 播放器显示」闭环。
- **T8/T10（字幕面板/矩阵回归）**：§5 的「真实可用操作清单」可直接作为 SubtitlePanel 的按钮—命令映射表；§6 列出 T8 需要补的命令（副字幕轨选择）与 T9 需要补的能力（自动加载/字体目录）。
