# 播放器 — 任务计划 v2（完整字幕 / 4K·蓝光 / AI / 弹幕；暂不动工）

> 状态：**仅计划，未写任何实现代码**。本版按新需求（完整字幕 + 4K/ISO 蓝光原盘流畅 + AI + 源码可控 + 后续弹幕）重写，
> 取代 v1 的「libVLC vmem」计划。架构依据：[PLAYER_ARCHITECTURE.md](PLAYER_ARCHITECTURE.md)；
> 调研底稿：[PLAYER_RESEARCH.md](PLAYER_RESEARCH.md)。
> 估算口径：1 名熟悉本项目的 Rust 工程师，含自测与文档；不含等待用户环境的时间。

---

## 0. 开工前前置（必须先解决）

| # | 前置 | 说明 | 责任 |
|---|---|---|---|
| P0-1 | 用户确认《架构》§11 的 8 个开放问题 | Windows 窗口形态（Mode B 原生嵌入是否接受）、蓝光菜单是否必须、AI 本地/云偏好、运行时打包体积接受度、优先级、**弹幕来源/交互深度/层级与 AI 弹幕范围** | 用户 |
| P0-2 | M0 技术验证通过 | 尤其：4K 硬解矩阵、字幕矩阵、ASS 弹幕轨叠加、流缓存吞吐 | 开发 |
| P0-3 | 依赖构建环境就绪 | Linux：meson/ninja/CMake、FFmpeg 构建依赖、Vulkan/EGL/VAAPI 头；Windows：MSYS2/MinGW-w64 工具链 + Rust target | 开发 |
| P0-4 | 测试素材 | 4K HEVC HDR mkv、蓝光 BDMV/ISO（未加密示例）、多音轨/多字幕轨 mkv、PGS 内嵌字幕、ASS 特效字幕、弹幕 XML/JSON 样本若干、无字幕视频（ASR 用） | 用户/开发 |
| P0-5 | 平台测试机 | Linux（X11 + Wayland）、Windows 10/11（含 HDR 显示器可选） | 用户 |

---

## 1. 里程碑总览

| 里程碑 | 目标 | 关键产出 | 估算 |
|---|---|---|---|
| **M0 技术验证** | 消除所有高风险未知项 | 7 份实验报告（E1–E7）+ 结论 | **6–10 人日** |
| **M1 播放内核** | libmpv 集成 + 双渲染模式跑通 4K | `wp-media-mpv`、`wp-player` 骨架、Mode A/B 渲染桥、4K 性能达标 | 8–12 |
| **M2 完整字幕** | 字幕全格式/双字幕/样式/同步/AI 注入接口 | `wp-subtitle` + SubtitlePanel + 字幕矩阵通过 | 6–8 |
| **M3 源接入** | 网盘/本地/ISO 全链路 + 流缓存 | `wp-stream`（缓存/换链/预读）+ 播放入口 + 碟片标题选择 | 8–10 |
| **M4 AI 基础设施 + 字幕翻译** | AI 地基 + P1 双语字幕 | `wp-ai`（provider/worker/缓存）+ 双语 ASS 注入 | 8–12 |
| **M5 弹幕系统** | 弹幕双渲染通道 + 本地弹幕全流程 | `wp-danmaku` + egui 覆盖层 + ASS 弹幕轨 + 面板/屏蔽/密度/本地库 | 8–12 |
| **M6 性能/打包/验收** | 可发行、可回归 | 运行时打包（Linux/Windows）、许可合规、性能基线、文档/验收报告 | 6–8 |
| **M7 AI/弹幕进阶（按需）** | P2/P3/P4 + 弹幕增强 | ASR 字幕、摘要/问答、OCR/超分、在线弹幕源/弹幕翻译/精华 | 10–18 |
| **合计（M0–M6）** | | | **约 50–72 人日**（单人 3 个月+ / 两人 6–8 周） |

---

## 2. M0 技术验证（先做；每项有明确通过门槛）

### E1：vendor media-stack 构建（Linux + Windows）

- 目的：证明「源码可控」可落地——从固定版本源码构建 **LGPL 构建的 libmpv + 依赖**。
- 方法：拉取 pinned 源码（mpv v0.41.0；FFmpeg LGPL 配置；libass；libplacebo；libbluray；可选 libaacs/libbdplus），用 `meson -Dgpl=false -Dlibmpv=true` 构建产物 `libmpv.so` / `libmpv-2.dll`；Windows 走 MSYS2/MinGW。
- 产出：`scripts/build-media-stack-linux.sh`、`-windows.ps1`（脚本骨架即可）+ 构建日志/产物清单。
- **门槛**：干净环境一次脚本构建成功；`ldd/objdump` 依赖完整；LGPL 声明文件齐全。

### E2：libmpv render API + eframe/glow 收帧（Mode A）

- 目的：确认 `egui_glow::CallbackFn` + render API FBO 贴图链路；验证 Linux X11/Wayland 两种上下文。
- 方法：最小 PoC：libloading 加载 libmpv → `mpv_render_context_create(OPENGL)` → render 到 FBO → egui 绘制纹理；播放 1080p/4K 文件。
- 产出：收帧帧率、首帧延迟、渲染路径（EGL/GLX）、`report_swap` 是否工作。
- **门槛**：4K HEVC 稳定 60/24fps 输出，无撕裂；egui UI 可与视频同帧合成。

### E3：Windows Mode B 原生窗口嵌入（`--wid`）

- 目的：验证 Windows 主选路径：子窗口 + d3d11va + 4K/HDR。
- 方法：Win32 创建 child HWND → `--wid=<hwnd>`；`vo=gpu-next`、`hwdec=auto-safe`、`gpu-context=d3d11`、`target-colorspace-hint=yes`；播放 4K HEVC HDR。
- 产出：硬解器实际选择（d3d11va？）、4K60 丢帧/CPU/GPU、HDR 是否直通、窗口缩放/全屏行为。
- **门槛**：4K60 HEVC 10bit 丢帧 < 0.5%、CPU 单核 < 40%；HDR 直通或明确降级方案。

### E4：Linux 硬解矩阵（Mode A × X11/Wayland）

- 目的：确认 VAAPI/NVDEC/Vulkan Video 在 render API 下的零拷贝工作条件。
- 方法：分别用 VAAPI（Intel/AMD）、NVDEC、Vulkan Video 播放 4K HEVC；记录 `hwdec-current`、是否零拷贝、丢帧/CPU。
- **门槛**：至少一种硬解在 X11 与 Wayland 都达标；失败时明确「降级 copy/软解」策略与提示文案。

### E5：字幕矩阵（完整字幕的可行性）

- 目的：验证 mpv 字幕栈覆盖所有必需格式。
- 方法：内嵌 ASS/SRT/PGS、外挂 SRT/ASS/VTT、双字幕（主+副）、内嵌字体、HDR 字幕、`sub-ass-override` 等逐项测试；记录样式/位置/字体回退。
- **门槛**：矩阵全通过；记录已知限制（如特殊字体缺失表现）。

### E6：蓝光 ISO/BDMV 主标题 + 菜单探测

- 目的：验证 libbluray 播放路径与标题选择；确认菜单支持边界。
- 方法：v0.41.0 播放未加密 BDMV/ISO 主标题；用 libbluray 列 titles；再试 master（0.42-dev）的 `bd://menu`/overlay/`disc-menu-active`。
- 产出：标题选择 UI 数据接口（titles/chapters/角/音轨/字幕）、菜单可用性结论、加密盘的报错形态。
- **门槛**：未加密原盘主标题可播；标题切换/章节 seek 正常；菜单能力给出「0.41 不可用 / master 可用（实验）」结论。

### E7：ASS 弹幕轨 + 副字幕轨叠加 + 流缓存吞吐

- 目的：为弹幕与网盘流畅打提前量。
- 方法：
  - 生成 5000 条 `\move` 弹幕 ASS（4K 字号），作为副字幕轨加载，与主字幕同时显示，测 4K 播放丢帧/CPU；验证主/副字幕层级与位置。
  - 流代理原型：对三盘直链做「预读 200–500MB + Range seek + 403 换链」压测，4K 码率（60–100Mbps）下模拟带宽抖动。
- **门槛**：ASS 弹幕 4K 播放丢帧 < 0.5%（或明确降级方案：egui 覆盖层/分片 reload/密度上限）；代理在带宽 150Mbps 下稳定供流、seek 回看 < 300ms。

### E8：AI 挂钩 spike

- 目的：确认「读字幕 → 翻译 → 注入」与「提取音频 → ASR」两条链路的接口可行。
- 方法：mpv 观察 `sub-start`/`sub-text`/`track-list`；用临时脚本翻译 20 句 → 生成双语 ASS → `sub-add`+`sub-reload`；另一路 ffmpeg/symphonia 从文件提取音频 → whisper.cpp 识别 30 秒。
- **门槛**：双语字幕注入即时生效、时间轴对齐；ASR 中文/英文可出可用文本。

> M0 结束输出《技术验证报告》并据此修订 M1+ 估算；未通过项先解决再进入 M1（不允许带病开工）。

---

## 3. 任务分解

### M1 播放内核（8–12 人日）

| ID | 任务 | 交付物 | 依赖 | 估算 | 写作用域 | 验收 |
|---|---|---|---|---|---|---|
| T1 | `wp-media-mpv`：预生成 bindings + 安全封装 | libmpv 加载/初始化/属性/命令/事件封装；版本检测与错误提示 | E1,E2 | 2 | `crates/wp-media-mpv/**`、`vendor/` | 无 mpv 环境可读报错；有 mpv 可创建 instance/player |
| T2 | RenderBridge（Mode A） | GL FBO + `egui_glow::CallbackFn` 合成；`report_swap` 帧同步；窗口缩放/上下文丢失处理 | T1,E2 | 2 | `wp-media-mpv` + `wp-gui` | 4K 视频贴在 egui 页面内，UI 可覆盖；缩放不崩 |
| T3 | Mode B 后端（`--wid` + 子窗口管理） | Win32 child window（Windows）/ X11（可选）；全屏/DPI/焦点/尺寸同步；键盘转发 | T1,E3 | 2 | `wp-gui`、`wp-media-mpv` | Windows 4K/HDR 原生播放；控制条在视频区外可用 |
| T4 | `wp-player` 状态机 + 事件总线 + 会话 | PlayState 状态机、PlayerSnapshot、命令队列、错误分类与恢复钩子 | T1 | 1.5 | `crates/wp-player/**` | 单测：状态迁移/错误分类；播放/暂停/seek/停止正确 |
| T5 | Player 页 UI（基础） | 视频区域、播放/暂停/进度/时间/音量/倍速/全屏、加载与错误态、性能诊断面板（硬解器/丢帧） | T2,T4 | 2 | `wp-gui/src/pages/player.rs`、`app.rs` | 截图回归；4K 下 UI 不卡 |
| T6 | 4K 性能调优（M1 阶段） | 自动渲染模式选择（平台/上下文探测）、hwdec 策略、帧同步参数、repaint 节流 | T2,T3,T5 | 1.5 | `wp-player`/`wp-media-mpv` | 达到 E3/E4 指标；写性能报告 |

### M2 完整字幕（6–8 人日）

| ID | 任务 | 交付物 | 依赖 | 估算 | 验收 |
|---|---|---|---|---|---|
| T7 | `wp-subtitle` 基础：模型/解析/ASS 生成 | Cue 模型、SRT/VTT/ASS/SSA 解析、ASS 生成（样式模板、双语布局） | M1 | 2 | 单测覆盖解析/生成；往返转换不丢时间轴 |
| T8 | 字幕控制：轨道/双字幕/样式/同步 | Track 枚举与切换；主+副字幕；字号/位置/延迟/速度/样式覆盖；持久化（每文件/全局） | T7 | 1.5 | 多字幕轨 mkv/外挂/双字幕操作正确；重启后恢复 |
| T9 | 外挂自动加载/字体/导出/搜索 | `--sub-auto=fuzzy`、`--sub-file-paths` 同名字幕；字体目录/内嵌字体；字幕导出/搜索 | T7 | 1 | 同名 srt/ass 命中；字体回退正确 |
| T10 | SubtitlePanel UI + 字幕矩阵回归 | 字幕面板、快捷键、样式预览；字幕矩阵测试表 | T8 | 1.5 | E5 矩阵全过并入库 |
| T11 | AI 字幕注入接口（先通管道） | `AddSubtitleFile`/`sub-reload` 封装、双语 ASS 样式、A/B 轨切换 | T7 | 1 | 手工生成双语 ASS 可即时注入并切换 |

### M3 源接入 + 流缓存（8–10 人日）

| ID | 任务 | 交付物 | 依赖 | 估算 | 验收 |
|---|---|---|---|---|---|
| T12 | `wp-stream`：本地 HTTP + Range 透传 + 头注入 | 代理服务、token、Range/HEAD/Content-Range、headers 注入、STATS | — | 2 | 单测（mock 上游）；VLC/curl/mpv 均可播代理 URL |
| T13 | 磁盘预读缓存 | 分块预取（多连接）、LRU、缓存目录、容量策略、seek 命中、与播放器 read-ahead 交互 | T12 | 1.5 | 4K 码率下缓冲稳定；缓存命中 seek < 300ms |
| T14 | 链接过期热换 | `expire_at`/401/403/410 → `direct_link` 刷新 → Range 续传；重试上限与错误提示 | T12 | 1 | 人为过期/断网后自动恢复；无感换链 |
| T15 | 网盘/本地/ISO 播放入口 | 浏览器双击/右键「播放」、本地文件选择/拖放、网盘直链→代理、最近播放 | T12,T13,T14,M1 | 2 | 三盘各 1 个视频可播；本地 4K 可播 |
| T16 | 蓝光碟片 | libbluray 标题/章节/角度列举；`DiscTitleDialog`；`bd://` 播放；主标题默认选最长；菜单 feature 探测 | E6 | 2 | 未加密 BDMV/ISO 可播；标题选择正确；限制文案准确 |
| T17 | 播放列表/断点续播/设置持久化 | 目录顺序/循环播放；每文件进度；最近播放；播放器设置页 | T15 | 1 | 重启后恢复进度/设置；播放列表操作正确 |

### M4 AI 基础设施 + P1 字幕翻译（8–12 人日）

| ID | 任务 | 交付物 | 依赖 | 估算 | 验收 |
|---|---|---|---|---|---|
| T18 | `wp-ai` 骨架：Provider trait + 任务队列 + 缓存 + worker 进程 | provider 接口、优先级/取消/超时/重试、结果缓存（文件指纹+语言+模型）、worker IPC | M2 | 3 | 单测：队列/缓存/取消；播放期间 AI 任务不影响帧率（隔离验证） |
| T19 | Provider：远端 OpenAI 兼容 + 本地 LLM 之一 | HTTP provider（可配 base_url/key/model）、术语表/系统提示；本地 provider（llama-cpp-2 或 candle）可后置 | T18 | 2.5 | 20 句翻译质量/时延基线；失败降级 |
| T20 | 字幕翻译管线（整片 + 实时前瞻） | 源字幕提取（外挂/内嵌文本）→ 分批翻译 → 双语 ASS → `sub-add`/`sub-reload`；缓存复用 | T11,T18,T19 | 2.5 | 整片双语字幕可播；二次播放命中缓存零耗时 |
| T21 | AiPanel UI + 设置 | AI 开关、provider 配置、翻译语言/双语布局、进度/取消、隐私提示 | T20 | 2 | 全流程 UI 可用；错误可读 |

### M5 弹幕系统（8–12 人日）

| ID | 任务 | 交付物 | 依赖 | 估算 | 验收 |
|---|---|---|---|---|---|
| T22 | `wp-danmaku` 模型/解析/导出 | Comment 模型；B 站 XML、JSON、ASS 导入/导出；排序/去重/时间轴归一 | M2 | 2 | 解析样本正确；导入导出往返一致 |
| T23 | 布局引擎（车道/密度/坐标） | 滚动车道碰撞、顶部/底部固定车道、密度上限、4K/1080p 相对坐标、性能 O(n log n) | T22 | 2 | 单测：碰撞/追尾/溢出/密度；10 万条布局 < 1s |
| T24 | 渲染后端 A：egui 覆盖层 | 批量绘制、文本图集/缓存、活跃窗口（二分）、视频矩形映射、交互命中（悬停暂停/复制） | T23,M1 | 2.5 | 4K + 1000 条/分钟 丢帧 < 0.5%；交互可用 |
| T25 | 渲染后端 B：ASS 弹幕轨 | 生成 `\move`/`\pos` 动画 ASS、4K 字号、颜色/透明度；副字幕轨加载/分层；分片 `sub-reload` | T23,M1 | 2 | Windows Mode B 可显示弹幕；主/副字幕层级正确；4K 不丢帧 |
| T26 | 弹幕管理/面板/本地库 | 弹幕开关、样式/密度/速度/屏蔽词、时间轴密度热力图、搜索跳转、本地弹幕库（指纹映射）、同名自动加载 | T24,T25 | 2.5 | 本地弹幕自动加载命中 100%；设置持久化；切换 < 300ms |
| T27 | 双模式验证 + 性能调优 | Mode A（egui）/ Mode B（ASS）一致性、4K 弹幕压测、降级策略（密度/上限/关闭交互） | T26 | 1 | 弹幕矩阵（滚动/顶部/底部/颜色/屏蔽/双模式）通过 |

### M6 性能/打包/验收（6–8 人日）

| ID | 任务 | 交付物 | 依赖 | 估算 | 验收 |
|---|---|---|---|---|---|
| T28 | 运行时打包（Linux/Windows） | `libmpv` 运行时包、依赖 DLL、许可文件、安装/加载逻辑（找不到→提示/外部播放器兜底） | M1–M5 | 2 | 干净机器安装即用；无 mpv 时行为可读 |
| T29 | 性能与稳定性基线 | 4K 播放、弹幕、AI 并发、1h 长播、内存/显存；报告入库 | M5 | 2 | 达到架构 §1 指标 |
| T30 | 许可合规与文档 | LGPL 声明/源码获取方式/补丁发布流程、README、播放器使用说明（限制：HDR/AACS/菜单/字幕特例）、验收报告 | T28 | 1.5 | 合规材料齐全；文档可复现 |
| T31 | 全量回归 | `cargo fmt/check/test --workspace`、smoke 截图（新增播放/字幕/弹幕页）、真机矩阵、提交 | T28,T29,T30 | 2 | 全绿；证据入库 |

### M7 进阶（按需，10–18 人日）

| ID | 任务 | 说明 | 估算 |
|---|---|---|---|
| T32 | P2 ASR 字幕 | 音频提取 → VAD → whisper 本地/远端 → 生成 SRT/ASS 注入 | 3–5 |
| T33 | P3 摘要/章节/问答 | 字幕全文 + 时间轴 → LLM 摘要/章节；播放页跳转 | 3–5 |
| T34 | P4 OCR/超分/画面理解 | PGS/位图字幕 OCR 翻译、GLSL/ONNX 画质增强、场景识别 | 4–8 |
| T35 | 在线弹幕源 / 弹幕翻译 / 弹幕精华 | Provider 可选接入（合规）、AI 弹幕翻译、密度曲线→高能片段 | 4–8 |

---

## 4. 测试矩阵（验收依据）

| 维度 | 用例 |
|---|---|
| 容器/编码 | mp4(H.264/HEVC)、mkv(HEVC 10bit HDR)、蓝光 BDMV/ISO、flv、ts、mp3/flac（音频） |
| 字幕 | 内嵌 ASS/SRT/PGS；外挂 SRT/ASS/VTT；双字幕；内嵌字体；HDR 字幕；特殊字体缺失 |
| 弹幕 | 滚动/顶部/底部、彩色/字号、1000 条/分钟、屏蔽词/正则、时间偏移、Mode A/B 双渲染、导入导出 |
| 源 | 本地、百度/夸克/光鸭直链（代理）、链接过期、带宽抖动、seek 回看 |
| 平台/模式 | Linux X11 Mode A（+B 如 GPL 构建）、Linux Wayland Mode A、Windows Mode B（4K/HDR） |
| AI | 双语翻译（整片/缓存命中）、ASR 30s 基线、AI 并发时播放帧率 |
| 性能 | 4K60 丢帧/CPU/GPU/内存 1h；seek 时延；弹幕密度；起播时延 |

---

## 5. 风险登记（执行层）

| ID | 风险 | 触发 | 缓解 | 兜底 |
|---|---|---|---|---|
| R1 | Linux 硬解零拷贝失败 | E4 | 换 hwdec API、强制 EGL、降级 copy | 软解/外部播放器 |
| R2 | Windows Mode A 不可行 | E3 | **已定 Mode B 主选** | Mode C 独立窗口 |
| R3 | 蓝光交互菜单需 0.42-dev | E6 | 主标题优先；菜单作实验 feature + 固定 commit | 外部 VLC/mpv 兜底 |
| R4 | UHD AACS2 无法解密 | 确定 | 文档明示；仅未加密/已解密 | 用户自行备份/转换 |
| R5 | LGPL 构建缺 X11 vo → Linux X11 无 Mode B | 确定 | Linux 默认 Mode A；X11 Mode B 需 GPL 构建 profile | Mode A/C |
| R6 | 4K/8K 高码率带宽不足 | E7 | 磁盘预读 + 码率探测 + 提示 | 降低缓存时长/外部播放 |
| R7 | AI 抢占资源影响播放 | 集成期 | 独立进程 + 优先级 + 播放期暂停重任务 | AI 手动暂停 |
| R8 | mpv master 不稳定 | 菜单 feature | 默认 0.41.0；固定 commit；回归矩阵 | 一键切回 0.41 |
| R9 | 位图字幕（PGS）无法文本翻译 | 确定 | P4 OCR 方案；v1 文档说明 | 显示原文 |
| R10 | 运行时体积大（Win 60–100MB） | 打包期 | 运行时包独立下载/按需；系统 mpv 选项 | 外部播放器模式 |
| R11 | Mode B 无 egui 弹幕覆盖层 | 确定 | ASS 弹幕轨兜底；交互弹幕仅 Mode A | 透明覆盖窗口（后置） |
| R12 | ASS 弹幕高密度/4K libass 性能 | E7 | 分片 reload、密度上限、egui 覆盖层替代 | 关闭部分弹幕 |
| R13 | 在线弹幕源合规/稳定性 | 若启用 | 默认关闭；用户配置；不做内置爬取 | 仅本地弹幕 |

---

## 6. 提交与分支策略

- 每个任务一个原子提交（`feat(player): ...` / `feat(danmaku): ...`），`cargo check --workspace` 零警告。
- M0 实验代码放 `examples/` 或临时分支，验证结论入 `docs/`，不合入产品代码。
- 新增 crate 同步更新 workspace members、README 工程结构、ARCHITECTURE、CI（Linux+Windows 构建矩阵）。
- 每里程碑结束：`cargo test --workspace` + `scripts/smoke-headless.sh` + 真机证据（截图/日志/性能数据）写入验收报告。

---

## 7. 待确认清单（✅ 已全部确认，见 §8）

1. **Windows 形态**：接受 Mode B「原生窗口 + 控制条在视频区外」作为主形态吗？（UI 完全覆盖需等 D3D11 render API/ANGLE 路线）
2. **蓝光**：主标题流畅播放即可，还是必须原盘交互菜单（BD-J/HDMV，需 mpv 0.42-dev 实验 feature）？
3. **AI**：优先本地离线（模型大、需 GPU/CPU）还是允许云端 API（需联网/密钥），还是两者都要（默认本地+远端可选）？
4. **运行时**：接受 Windows 发行包附带 60–100MB 播放器运行时（或独立下载）吗？
5. **优先级**：4K+字幕+网盘 → AI 字幕翻译 → ASR → 摘要/问答 → OCR/超分，这个顺序可以吗？
6. **弹幕**：只做本地弹幕（同名 XML/JSON/ASS+本地库），还是需要内置/可配置 **在线弹幕源（B 站等）**？
7. **弹幕交互**：显示+开关+样式+屏蔽即可，还是需要悬停/点赞/发送/搜索跳转（交互仅 Mode A）？
8. 弹幕与字幕**层级**偏好？是否需要 **AI 弹幕（翻译/精华）** 进入后续版本？

---

## 8. v2.1 决策落地（2026-10-19 用户拍板后修订）

| 决策 | 对任务的调整 |
|---|---|
| **1. 不接受原生窗口嵌入** | 删除 Mode B 主路线相关任务；M0 新增 **E3'：Windows ANGLE 上下文 + libmpv render API 验证**（见 [PLAYER_WINDOWS_ANGLE.md](PLAYER_WINDOWS_ANGLE.md)）；`vendor/glutin-winit` 已打补丁（Windows 优先 ANGLE，回退 WGL） |
| **2. 蓝光只要主标题** | T16 去掉交互菜单/BD-J overlay 任务；只做 ISO/BDMV 主标题 + 标题/章节/角度选择；mpv 固定 v0.41.0（不引入 0.42-dev 依赖） |
| **3. AI 本地+远端都要** | T18/T19 扩展：Provider 必须支持 enabled/priority 配置与回退；UI 提供分别启停/优先级（T21 面板）；本地 provider（llama.cpp/whisper.cpp）与远端 OpenAI 兼容都列入 |
| **4. 接受 Windows 运行时 60–100MB** | T28 打包方案确定为随包 `播放器运行时/`（libmpv + 依赖 + ANGLE + LICENSES）；不再考虑纯系统依赖兜底作为默认 |
| **5. 优先级顺序** | 保持 M1→M6 顺序：4K+字幕+网盘 → AI 字幕翻译（M4）→ ASR/摘要/OCR（M7） |
| **6. 弹幕在线+本地** | T26/T27 增加「在线弹幕 Provider（可配置、缓存、合规提示）」与本地库并存；M5 估算 8–12 → **10–14 人日** |
| **7. 弹幕完整交互** | T24/T26 增加悬停暂停/发送（本地库、在线可选）/搜索跳转/时间轴热力图；ASS 弹幕轨仅作兜底（交互只能在 egui 覆盖层） |
| **8. 层级可配 + AI 弹幕后续** | 播放器设置增加「弹幕/字幕层级」；AI 弹幕（翻译/精华）列入 M7（T35） |

### 新增/调整任务

| ID | 任务 | 估算 |
|---|---|---|
| T0-Win | Windows ANGLE 上下文验证：eframe+ANGLE 启动、libmpv render API、d3d11va 硬解、4K/HDR、无 ANGLE 回退（需 Windows 测试机） | 2–3 人日 |
| T24b | 弹幕在线 Provider + 本地库合并/缓存/合规配置 | 2–3 人日 |
| T24c | 弹幕交互：悬停暂停、发送（本地/在线可选）、搜索跳转、时间轴热力图 | 2–3 人日 |
| T35 | AI 弹幕（翻译/精华：密度曲线→高能片段） | 4–6 人日 |

## 9. 当前状态（2026-10-19 更新）

| 里程碑 | 状态 | 关键证据 |
|---|---|---|
| M0 技术验证 | ✅ 完成 | `docs/PLAYER_M0_RESULTS.md`（E1–E5 全部有数据） |
| M1 播放内核 | ✅ 基本完成 | GUI 真实 libmpv 渲染截图 `出/验收截图/25-真实播放.png`；workspace 543 tests 全绿；headless 冒烟 24/24 |
| M2 完整字幕 | 🟡 内核完成（59+5 测试）、播放器面板已接 | 待真实内嵌字幕/PGS 真机矩阵与 AI 注入联调 |
| M3 源接入 | 🟡 真机三盘 Range/HLS 完成；GUI 网盘播放待接线 | `wp_stream::playback::resolve_playback` 已就绪 |
| M4 AI 翻译 | 🟡 地基+闭环完成 | 待真实 key/本地 llama-server 真机评估与 GUI 按钮接线 |
| M5 弹幕 | 🟡 内核完成（80+6）、GUI overlay 已接 | 待在线弹幕源/发送/搜索跳转接线与真机密度压测 |
| M6 打包验收 | ⏳ 未开始 | 运行时打包脚本/Windows ANGLE 实测/许可合规清单 |

## 10. 当前状态

- 本计划为**提案**：未写实现代码、未改依赖、未动构建脚本。
- 用户确认 §7 的 8 个问题 + M0 通过后，按 M1→M6 顺序执行；M7 按需插入。
