# 播放器架构设计（v2：完整字幕 + 4K/蓝光原盘 + AI Ready + 源码可控）

> 日期：2026-10-19 · 状态：**设计提案（未实现）**
> 触发：需求从「能播」升级为「**必须完整字幕** + **4K/ISO 蓝光原盘无比流畅** + **后期 AI** + **代码尽量源码可控**」，
> 上一版基于 libVLC vmem 的选型（见 [PLAYER_RESEARCH.md](PLAYER_RESEARCH.md)）已不满足，本文是修订后的目标架构。
> 配套任务计划：[PLAYER_TASKS.md](PLAYER_TASKS.md)（v2 版，已按本架构重写）。

---

## 0. 结论先行

| 决策点 | 结论 |
|---|---|
| **媒体内核** | **libmpv（MPV）**，LGPL 构建（`-Dgpl=false`）动态加载。理由：完整字幕栈（libass + 位图字幕）、硬件解码零拷贝、libplacebo/gpu-next 渲染质量、libbluray 蓝光支持、可 pinned 源码自建、AI 挂钩点多 |
| **播放窗口模型** | 双模式：**Mode A** 进程内 GPU 渲染（render API + OpenGL，Linux 主选，UI/AI 覆盖层可叠在视频上）；**Mode B** 原生子窗口嵌入（`--wid`，Windows 主选，走 d3d11va 硬解 + HDR 直通）；**Mode C** 独立播放窗口/外部播放器兜底 |
| **字幕** | 渲染交给 mpv/libass/PGS 子系统（完整度最高）；**AI 字幕/翻译/生成**由我们的 `wp-subtitle` + `wp-ai` 以「外部字幕注入 + sub-reload」接管，双语 ASS 保留原样式与位置 |
| **4K 流畅** | 硬解矩阵（VAAPI/NVDEC/Vulkan Video/D3D11VA）+ 零拷贝 GPU 纹理 + mpv 帧同步（report_swap/display-resample）+ **磁盘预读缓存流代理**（隔离网盘抖动、支持秒 seek 回看、链接过期热换） |
| **蓝光原盘** | ISO/BDMV 主标题播放用 v0.41 稳定 libbluray 路径；**交互菜单（HDMV/BD-J）** 需 mpv master（0.42-dev）的 overlay 渲染，以「独立 feature + 固定 commit」提供；**加密 UHD（AACS 2.x）无开源解密方案**，仅未加密/已解密备份可播 |
| **源码可控** | 媒体栈 `vendor/` 固定版本源码 + 一键构建脚本（Linux/Windows），Rust 层 100% 自有；LGPL 动态链接 → 应用保持 MIT；提供可替换 `.so/.dll` 与补丁目录；GPL 构建作为可选 profile（换取 X11 窗口 VO/DVD 菜单等） |
| **AI** | `wp-ai` 独立 crate + Provider 抽象（本地 whisper.cpp / llama.cpp / candle / ONNX 与 OpenAI 兼容远端），**本地/远端可分别启停、按 priority 设置优先级与回退**（用户已确认两者都要），重活进程隔离，先做 P1 字幕翻译、P2 ASR，再扩展摘要/问答/画面增强 |
| **弹幕** | 独立 `wp-danmaku` crate + 双渲染后端：**Mode A 用 egui 覆盖层**（完整交互），ASS 弹幕轨作为跨引擎兜底；布局引擎（车道碰撞/密度限制/4K 坐标换算）在 Rust 侧；**数据源=本地+在线（用户已确认）**；交互含悬停/发送/搜索跳转（v2.1）；弹幕/字幕层级可配置、AI 弹幕（翻译/精华）列入后续版本 |

**一句话架构**：`egui 应用（UI/字幕/弹幕/AI）` → 自研 Rust 控制层（`wp-player`）→ **libmpv 媒体内核（pinned LGPL 构建）** → 平台渲染模式（GL render API / 原生窗口）；
网盘/本地/ISO 统一经 **`wp-stream` 磁盘缓存代理** 或 `--http-header-fields` 直连；**字幕/弹幕**通过 mpv 字幕轨（`sub-add`/`sub-reload`/`secondary-sub-*`）+ egui 覆盖层双通道渲染；
AI 通过 **mpv 属性/命令挂钩（sub-start/sub-text/track-list/sub-add/screenshot）** 与独立管线接入。

---

## 1. 需求 → 硬约束 → 可量化指标

| 需求 | 技术硬约束 | 验收指标 |
|---|---|---|
| **完整字幕** | 必须支持：内嵌文本字幕（ASS/SSA/SRT/VTT/mov_text）、**内嵌位图字幕**（PGS 蓝光、VobSub/DVD、DVB）、外挂字幕、同名自动加载、多字幕轨、**主+副双字幕**、完整 ASS 特效（libass）、内嵌字体与字体目录、样式覆盖、延迟/位置/缩放、HDR 下字幕峰值控制 | 字幕矩阵全通过：4 类格式 × 内嵌/外挂 × 双字幕；ASS 特效显示正确；字体回退正确 |
| **4K / 蓝光原盘流畅** | 硬件解码零拷贝；4K60 HEVC 10bit；HDR10/HLG 直通或高质量 tone-mapping；蓝光 ISO/BDMV 导航（主标题至少；菜单可选）；网络流不许卡顿（预读缓存） | 4K60 HEVC 10bit：丢帧 < 0.5%、CPU 单核 < 40%、内存平稳（1h+）；seek 起播 < 1.5s（已缓存区 < 300ms）；4K 蓝光原盘（未加密）主标题可播 |
| **AI 后期加入** | 需要 4 类挂钩：**当前字幕 cue**、**播放事件/时间轴**、**视频帧抓取**、**音轨内容**；AI 不得影响播放流畅 | P1 字幕翻译可实时双语；P2 无字幕视频自动生成字幕；播放帧率不受 AI 任务影响（AI 进程隔离） |
| **源码可控** | 媒体栈源码可获取、可固定版本、可自行编译/打补丁；应用代码不受 GPL 传染（或明确接受 GPL）；许可证合规 | 从零在干净机器上 `scripts/build-media-stack-*.sh/ps1` 可复现构建；`vendor/` 有 pin 版本与补丁；应用 license 保持 MIT |
| **弹幕（后续）** | 弹幕需与视频帧精确同步、支持滚动/顶部/底部/样式/屏蔽/密度控制；覆盖播放器两种渲染模式；可扩展在线源与 AI（翻译/精华） | 1000 条/分钟密度下 4K 播放不丢帧；弹幕同步误差 < 50ms；开关/样式切换 < 300ms 生效；本地弹幕自动加载命中正确 |
| **跨平台** | Windows 10/11 + Linux（X11 与 Wayland） | 两端播放/字幕/AI 基础功能通过矩阵 |

---

## 2. 为什么是 libmpv（而不是 VLC / GStreamer / 纯 FFmpeg）

### 2.1 能力对照（针对新需求）

| 能力 | libmpv | libVLC 3.x | GStreamer | 纯 FFmpeg 自研 |
|---|---|---|---|---|
| 完整字幕 | **libass 全特效 + 位图字幕（PGS/DVD/DVB，`sub/sd_lavc.c`）+ 双字幕 + 字体嵌入 + 样式/位置/延迟/缩放** | vmem 路径**丢字幕**（`vmem.c` 忽略 subpicture）；窗口嵌入才有字幕 | 需自建 libass/overlay 链，PGS 支持零散 | 全部自研（libass 可复用；PGS 位图渲染要自己写） |
| 4K 硬解 | VAAPI/NVDEC/Vulkan Video/D3D11VA，官方推荐 hwdec；gpu-next 默认（0.41） | 支持，但进程内 GPU 渲染回调 VLC4 才有 | 支持（插件齐全时） | 自研（FFmpeg hwaccel + 自己 GPU 互操作，工作量大） |
| 蓝光 ISO/原盘 | libbluray：主标题（0.41 稳定）；**交互菜单/BD-J overlay 在 0.42-dev** | VLC 支持 BD-J 菜单（Java） | 无菜单 | 仅 demux |
| 渲染质量/HDR | libplacebo gpu-next：HDR tone-mapping/passthrough、dither、scaling | 较老 | 依赖 gl 插件 | 自研 |
| AI 挂钩 | **属性/命令极丰富**：`sub-start/sub-text/track-list` 可观察、`sub-add/sub-reload/screenshot-to-file` 可命令、render API 可读帧 | 接口较少 | 可做 appsink/tee，但字幕链要自建 | 全自研 |
| 源码可控 | 纯 C，LGPL 构建可行（`-Dgpl=false`），依赖 LGPL/ISC | 纯 C，libvlc LGPL，但 VLC4 未发布、VLC3 缺嵌入 GPU 路径 | LGPL，源码可控 | FFmpeg LGPL/GPL |
| 移植/维护 | 0.41.0 活跃（2025-12），0.42-dev 持续 | 3.0.24 稳定但 4.0 未发 | 活跃 | 活跃 |

**结论**：mpv 是唯一同时满足「完整字幕 + 4K HDR 硬解质量 + 蓝光 + AI 挂钩 + 可源码自建」的引擎。VLC 仅保留为「BD-J 菜单兜底 / 用户机器已有 VLC 时的外部播放器」参考。

### 2.2 许可证路线（MIT 应用 → LGPL 媒体核）

- mpv 仓库提供 **双许可构建**：默认 GPLv2+；`meson option gpl=false` → **LGPLv2.1+** 构建（`meson.options:2`；`LICENSE.GPL`/`LICENSE.LGPL` 并存）。
- LGPL 构建**会禁用**这些 GPL-only 选项（`meson.build` 中 `get_option('gpl')` 门控）：
  `x11`（mpv 自带 X11 窗口 VO）、`dvdnav`（DVD 菜单）、`dvda`、`cdda`、`dvbin`、`jack`、`oss-audio`、`caca`、`direct3d`（D3D9 VO）。
  **不影响我们需要的**：`gpu-next`/libplacebo、`libmpv` render API、**`d3d11`（Windows 渲染，未门控）**、libass/字幕、`libbluray`（未门控）、FFmpeg 解码、VAAPI/NVDEC/D3D11VA。
- 因此：**MIT 应用 + 动态链接 LGPL libmpv 完全可行**；发行时附 LGPL 文本、源码获取方式、允许用户替换 `libmpv.so/dll`，我们给 mpv 打的补丁按 LGPL 发布。
- 如某些用户要 DVD 菜单/X11 窗口 VO，可提供 **GPL 构建 profile**（文档写明许可影响），默认不启用。

### 2.3 版本与 pin 策略

| 用途 | 版本 | 理由 |
|---|---|---|
| 默认媒体核 | **mpv v0.41.0**（2025-12 发布） | 稳定；gpu-next 默认、Vulkan hwdec 优先、libplacebo 质量、`bd://menu` 主标题基础支持 |
| 蓝光交互菜单（可选 feature） | **mpv master（0.42-dev）固定 commit** | `stream_bluray.c` 的 overlay 渲染（`bd_register_overlay_proc` HDMV + `bd_register_argb_overlay_proc` BD-J）与 `disc-menu-active` 属性仅在 0.42-dev；以 `bluray-menu` feature 单独构建、独立发布说明，避免影响主体验 |
| 依赖 | FFmpeg（LGPL 构建）、libass（ISC）、libplacebo（LGPL-2.1）、libbluray（LGPL-2.1）、可选 libaacs/libbdplus（LGPL-2.1，AACS 1.0/BD+，仅用户自备密钥） | 全 LGPL/ISC/Apache 兼容，可自建 |

---

## 3. 总体架构

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                     GUI 层（wp-gui · eframe 0.37 / egui）                    │
│  PlayerPage（视频表面/控制条/全屏）· SubtitlePanel · DanmakuPanel · AiPanel   │
│  DiscTitleDialog · MediaLibraryPage（网盘/本地/最近播放）                     │
└───────────────▲────────────────────────────────┬────────────────────────────┘
                │ PlayerEvent（状态/位置/轨道/cue/AI 结果）│ 命令（播放/暂停/seek…）
┌───────────────┴────────────────────────────────▼────────────────────────────┐
│                          wp-player（Rust 编排层，可单测）                      │
│  PlaybackController（状态机）· ResumeStore（断点续播）· MediaLibrary          │
│  SubtitleManager（轨道/双字幕/样式/AI 注入）· DanmakuManager（时间轴/弹幕轨）  │
│  AiCoordinator（任务编排）                                                    │
└───────┬──────────────────┬──────────────────┬──────────────────┬───────────┘
        │ 引擎抽象          │ 字幕模型          │ 弹幕模型          │ AI 任务
┌───────▼──────────┐┌──────▼───────────┐┌─────▼───────────┐┌─────▼──────────────┐
│ wp-media-mpv     ││ wp-subtitle      ││ wp-danmaku      ││ wp-ai              │
│ · mpv-sys(预生成)││ · 解析 SRT/VTT/  ││ · 模型/解析      ││ · Provider 本地/远端│
│ · 安全封装/属性/  ││   ASS/SSA        ││   (B站XML/JSON) ││ · 翻译/ASR/摘要/OCR │
│   命令/事件       ││ · 双语 ASS 生成   ││ · 车道布局引擎   ││ · 任务队列/缓存      │
│ · RenderBridge    ││ · cue 追踪       ││ · ASS 弹幕生成   ││ · 进程隔离/限流      │
│   (GL FBO 纹理)   ││ · 字幕轨/双字幕   ││ · egui 覆盖层    ││                     │
│ · 双模式 render   ││                  ││ · 本地弹幕库     ││                     │
│   API / --wid 窗口││                  ││                  ││                     │
└───────┬──────────┘└──────┬───────────┘└─────┬───────────┘└─────┬──────────────┘
        │ libmpv           │ sub-add/reload   │ 副字幕轨/覆盖层   │ AI 结果注入
┌───────▼──────────────────▼──────────────────▼──────────────────▼──────────────┐
│               wp-stream（本地流代理 + 磁盘预读缓存 + 链接过期热换）            │
│  Range/HEAD 透传与头注入（Cookie/Referer/UA）· 多连接分块预取 · LRU 磁盘缓存   │
└───────┬────────────────────────────────────────────────┬──────────────────────┘
        │ DirectLink{url, headers, expire_at}             │ 本地文件路径 / ISO
┌───────▼──────────────────────────┐          ┌──────────▼─────────────────────┐
│ wp-drive 适配器（百度/夸克/光鸭）  │          │ 本地磁盘 / 蓝光 ISO·BDMV        │
│ 已有 direct_link()，无需改动       │          │ libbluray 解析标题/章节/音轨     │
└──────────────────────────────────┘          └────────────────────────────────┘
```

> libmpv 有两种窗口形态：**Mode A** 由我们提供 GL 上下文（进程内渲染，UI 可覆盖视频）；
> **Mode B** 由 mpv 渲染到我们创建的原生子窗口（Windows 主选，硬解/HDR 最稳）。
> 弹幕同样双通道：Mode A 用 egui 覆盖层（可交互），Mode B 用预生成的 ASS 弹幕轨（mpv 渲染）。

### 3.1 线程/进程模型

| 线程/进程 | 职责 | 原则 |
|---|---|---|
| UI 主线程（egui） | 渲染 UI、处理输入、消费 PlayerEvent | 绝不阻塞；视频帧直接贴 GPU 纹理 |
| mpv 内部线程池 | 解复用/解码/音频输出/渲染 | 由 mpv 管理；我们只提供 GL 上下文回调与渲染调用 |
| RenderBridge 线程 | 在 GL 上下文 current 时调用 render API、report_swap | 与 egui 绘制同线程（或由 eframe paint callback 驱动） |
| wp-stream 运行时（tokio） | 代理 HTTP、预读、换链 | 独立线程池，与播放解耦 |
| wp-ai Worker（**独立进程**，推荐） | ASR/LLM 推理、翻译批处理 | 防止 AI 卡顿影响播放；GPU 抢占时自动降级/暂停 |
| 事件总线 | mpsc/Crossbeam channel | 所有跨线程通信走消息，避免锁 |

---

## 4. 渲染与性能架构（4K/蓝光流畅的关键）

> ⚠️ **v2.1 决策更新（2026-10-19）**：用户**不接受原生窗口嵌入**，故下表的 **Mode B 不再采用**；
> Windows 与 Linux 统一走 **Mode A（进程内 GPU 渲染）**；Windows 通过 **ANGLE（EGL/GLES）上下文**
> 让 libmpv render API 获得 D3D11VA 硬解能力。实现见 [PLAYER_WINDOWS_ANGLE.md](PLAYER_WINDOWS_ANGLE.md)
> 与 `vendor/glutin-winit` 补丁。Mode B 仅作为历史方案保留说明。

### 4.1 三种显示模式（按平台自动选，可手动覆盖）

| 模式 | 集成方式 | 硬解/HDR | 适用平台 | 优缺点 |
|---|---|---|---|---|
| **A. 进程内 GPU 渲染** | libmpv render API（`MPV_RENDER_API_TYPE_OPENGL`）渲染到 FBO → `egui_glow::CallbackFn` 贴到 egui 画面；mpv 只负责解码/渲染，窗口归 eframe | 依赖 app 的 GL 上下文：Linux EGL + VAAPI dmabuf 零拷贝；Windows WGL 下 D3D11VA 受限（见下） | **Linux 主选**（X11/Wayland） | UI 可完整覆盖（控制条/字幕面板/AI HUD/全屏）；EGL 下零拷贝 |
| **B. 原生子窗口嵌入** | 创建子窗口（Windows：Win32 child HWND；Linux X11：child XID）→ `--wid`；mpv 用 `vo=gpu-next` + 原生 context（d3d11 / x11） | **最佳**：D3D11VA + D3D11 零拷贝、HDR10 直通（`--target-colorspace-hint`）；Linux X11 VAAPI 直出 | **Windows 主选**；Linux X11 可选 | 性能/兼容性最好；但 egui 不能覆盖视频区域（控制条放视频区外；全屏可隐藏 chrome） |
| **C. 独立窗口 / 外部播放器** | 独立 mpv 窗口（影院模式）或系统播放器 | 最佳（原生） | 任意 | 兜底；UI 分离 |

**平台默认策略**：
- Linux **Wayland** → Mode A（EGL 上下文，VAAPI/NVDEC 零拷贝 dmabuf；egui 可全覆盖）。
- Linux **X11** → 先试 Mode A（EGL，若驱动/上下文支持 hwdec 直通）；失败/性能不达标 → **需 GPL 构建的 Mode B**（LGPL 构建无 x11 vo，见 §2.2）→ 否则 Mode C。
- **Windows（v2.1）** → **Mode A + ANGLE**：`vendor/glutin-winit` 补丁让 eframe 优先用 ANGLE EGL/GLES 上下文，
  libmpv render API 在该上下文下尝试 D3D11VA 硬解；无 ANGLE 时自动回退 WGL（软件解码 + 清晰提示）。
  验证清单见 [PLAYER_WINDOWS_ANGLE.md](PLAYER_WINDOWS_ANGLE.md)。
- 未来：mpv master 已出现 **D3D11 render API**（`MPV_RENDER_API_TYPE_D3D11`、`render_d3d11.h`，0.42-dev），若配合 wgpu/D3D11 合成的 eframe 路线，Windows 可回到 Mode A；列 M4 评估。

> ⚠️ M0 必须实测的矩阵见任务计划 E1~E6：**Mode A×Linux X11/Wayland×4K HEVC 硬解**、**Mode B×Windows×4K/HDR**、两种模式的帧同步与 CPU/GPU 占用。

### 4.2 帧同步与「无比流畅」

- render API：启用 `MPV_RENDER_PARAM_ADVANCED_CONTROL`，用 update callback 驱动重绘；每次渲染后 `mpv_render_context_report_swap()` 报告显示时间戳，让 mpv 的 `video-sync=display-resample` 准确对齐 vsync；`MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME` 控制阻塞策略。
- 音频时钟由 mpv 的 ao（PipeWire/WASAPI）掌管；音频不出 Rust 层，避免自研同步。
- egui 重绘节奏与视频帧解耦：仅在 mpv 通知新帧 / UI 动画时 `request_repaint`，不做无意义 60fps 重绘。
- 硬解矩阵（按平台自动 `hwdec=auto-safe`，可手动指定）：
  - Linux：**VAAPI**（Intel/AMD，EGL dmabuf 零拷贝）、**NVDEC**（NVIDIA）、**Vulkan Video**（mpv 0.41 优先）。
  - Windows Mode B：**D3D11VA**（默认）、NVDEC（NVIDIA）。
  - 兜底 `*-copy`（拷贝回内存，4K 可能不足，仅兼容性兜底）。
- HDR：Mode B Windows 用 `--target-colorspace-hint=yes` 走 HDR 直通；Mode A 若上下文不支持 HDR → libplacebo tone-map 到 SDR（`--tone-mapping=auto`），UI 明示「HDR→SDR」。

### 4.3 网络侧流畅：`wp-stream` 磁盘预读缓存

网盘播放最大的卡顿源不是解码，而是**带宽抖动/链接过期**。代理设计：

1. 播放器只看到 `http://127.0.0.1:{port}/s/{token}`；
2. 代理收到 `Range` 请求后 → 查磁盘缓存（按 `drive+path+size+mtime` 命名）→ 命中直接返回（本地磁盘速度，seek 回看瞬时）；
3. 未命中 → 向 `DirectLink` 发带 headers 的请求，**多连接分块预取**（若上游支持 Range；否则单连接顺序下载），边下边服务；
4. **预读策略**：目标缓冲 20–60 秒视频时长（4K 蓝光 60–100Mbps → 150–750MB），带宽探测自适应；`Accept-Ranges` 透传；
5. **链接过期热换**：上游 401/403/410 或 `expire_at` 到 → 调 `Drive::direct_link` 重新取链 → 从当前字节偏移继续 Range 请求，播放器无感；
6. 磁盘缓存 LRU（默认上限可配 20GB，播放结束可选保留/清理），缓存目录 `数据/播放器/缓存/`；
7. **备选直连**：mpv 也支持 `--http-header-fields`（任意头）/`--referrer`/`--cookies*`，因此若不想走代理可直接把 `DirectLink.headers` 传给 mpv —— 但没有缓存/换链能力，仅作降级路径。

> 4K 蓝光原盘 60–100Mbps：要求局域网/宽带 ≥ 150Mbps 且缓存目录在 SSD 上体验最佳；代理会在起播前预读 ~2–5 秒再开始播放（可配置），避免开头卡顿。

### 4.4 蓝光原盘（ISO/BDMV）

| 场景 | 方案 | 状态 |
|---|---|---|
| **BDMV 目录 / ISO 主标题** | `--bluray-device=<iso或目录>` + `bd://`；用 libbluray 列出 titles（`bd_get_titles`）→ 用户在「碟片标题」对话框选 title/章节/角度 → `bd://mpls/XXXXX` 播放 | v0.41 稳定支持（`--bluray-device`、`--bluray-angle`；v0.41 `stream_bluray.c` 已能识别 `bd://menu` 标题与 `BD_EVENT_MENU`） |
| **交互菜单（HDMV/BD-J）** | （**v2.1：用户确认 v1 不需要交互菜单，仅要求主标题流畅**）保留 mpv master overlay 方案作为未来可选项，不进入 v1 范围 | 未来可选 |
| **主标题默认选择** | libbluray `bd_get_main_title()` / 最长 title；播放页「碟片标题」列表可选章节/音轨/字幕 | v1 必做 |
| **AACS 加密** | libaacs（AACS 1.0，LGPL）+ 用户自备 `keydb.cfg`；libbdplus（BD+，LGPL） | 支持普通蓝光（需用户提供密钥）；**AACS 2.x / UHD 4K 蓝光无开源解密实现** → 加密 UHD 盘不可播；未加密/已解密备份（BDMV/MKV）可播 |
| 封面/元数据 | libbluray meta_data（片名/语言）→ 播放页展示 | 可选 |

---

## 5. 字幕系统（「完整字幕」的落地设计）

### 5.1 分层职责

| 层 | 组件 | 职责 |
|---|---|---|
| 渲染层 | mpv：libass（文本字幕）+ `sub/sd_lavc.c`（位图字幕：PGS/VobSub/DVB，文件本身 LGPL-2.1+）+ `vo_gpu_next` 字幕混合 | 显示一切格式、完整 ASS 特效、字体嵌入/回退、HDR 字幕峰值（`--secondary-sub-visibility`、`sub-ass-*` 等选项族） |
| 控制层 | `wp-subtitle`（Rust） | 轨道枚举/选择（`track-list`）、双字幕控制、延迟/位置/字号/样式、同名外挂自动加载、字幕搜索/导出、**cue 追踪**（观察 `sub-start`/`sub-text`/`secondary-sub-*`） |
| AI 层 | `wp-ai` + `SubtitleManager` | 翻译（逐句/整片）、ASR 生成字幕、润色、术语表、双语 ASS 注入（`sub-add` + `sub-reload`） |
| UI 层 | SubtitlePanel | 字幕列表/双字幕/样式/AI 翻译开关/时间轴微调/导出 |

### 5.2 完整字幕功能清单（必须逐项实现）

1. **格式**：外挂 SRT/VTT/ASS/SSA/LRC/SUB；内嵌文本（ASS/SRT/VTT/mov_text）；**内嵌位图（PGS/DVD/DVB）**；同名自动匹配（`--sub-auto=fuzzy`、`--sub-file-paths`）。
2. **显示**：ASS 全部特效（由 libass）；字体嵌入（`--embeddedfonts`）、字体目录（`--sub-fonts-dir`）、样式覆盖（`--sub-ass-override`）、字号/位置/边距/缩放、颜色/描边/阴影、HDR 字幕亮度控制。
3. **多字幕**：字幕轨列表与切换（`track-list/N/*`、`--sid`）；**主字幕 + 副字幕**（`--secondary-sub-*`，含可见性/位置/延迟/缩放）。
4. **同步**：`--sub-delay`；片源/字幕帧率差修正（`--sub-speed`）；AI 双语字幕与视频毫秒级对齐。
5. **AI 增强**：字幕翻译（双语双轨）、ASR 生成、提词/术语表、按句问答、导出双语 ASS/SRT。
6. **本地化**：字幕内嵌字体提取/缓存、字体缺失时回退提示、繁简转换（后期可接 AI）。
7. **持久化**：每个文件记住字幕轨/双语开关/字号/延迟；`播放器/字幕偏好.json`。

### 5.3 AI 字幕数据流

```
[播放/暂停/seek] ──► mpv 属性观察（sub-start / sub-text / secondary-sub-* / track-list）
        │                          │
        │                          ▼
        │              ┌────────────────────────┐
        │              │ wp-subtitle::CueTracker │ 生成结构化 cue 列表（时间/文本/轨道）
        │              └───────────┬────────────┘
        │                          │ 批量（整片翻译）或 前瞻窗口（实时翻译）
        ▼                          ▼
┌────────────────┐        ┌──────────────────────┐        ┌────────────────────────┐
│ wp-ai 任务队列  │────────►│ Provider（本地/远端） │────────►│ 双语 ASS 生成（保留原样式）│
└────────────────┘        └──────────────────────┘        └───────────┬────────────┘
                                                                       │ 写临时文件
                                                                       ▼
                                              mpv 命令：sub-add 文件 select + sub-reload
                                              （或 --secondary-sub 双轨：原文/译文各一轨）
```

- **整片翻译（本地/网盘 VOD）**：先把源字幕完整提取（外挂直接解析；内嵌文本用 ffmpeg 解复用或 mpv cue 捕获）→ 批量翻译（保留时间轴）→ 生成双语 ASS（原文在上/译文在下或用样式）→ `sub-add` 添加为一轨，可随时 `sub-reload` 更新。
- **实时翻译（直播/超长片）**：前瞻 3–5 句滚动翻译，缓存命中优先，注视频率限制与超时降级（显示原文+英文）。
- **位图字幕（PGS）**：mpv 负责显示；AI 翻译需要 OCR（后期 P4）：抓取字幕区域帧（`screenshot-to-file` / render API 读帧）→ OCR → 翻译 → 生成 ASS 覆盖层（可选隐藏原 PGS 轨）。

---

## 6. 弹幕系统设计（后续加入；架构预留）

### 6.1 目标与功能清单

| 类别 | 功能 |
|---|---|
| **显示模式** | 滚动（右→左）、顶部固定、底部固定、逆向滚动（可选）；密度上限、显示区域（如只显示上半屏）、速度倍率 |
| **样式** | 颜色、字号、透明度、描边/阴影、字体；按弹幕类型区分；4K/HDR 下按视频分辨率等比缩放并在 HDR 下做亮度适配 |
| **同步** | 时间轴偏移（±ms）、随倍速/seek 正确重排、暂停时弹幕冻结、缓冲时暂停弹幕 |
| **过滤** | 关键词/正则屏蔽、用户屏蔽、类型屏蔽、重复弹幕折叠、低质量弹幕降权（可选） |
| **交互（Mode A）** | 悬停暂停单条弹幕、点击复制/举报（本地）、发送弹幕（写入本地库；在线发送可选）、时间轴密度热力图、弹幕搜索并跳转 |
| **数据源** | 本地自动加载（同名 `.xml`/`.json`/`.ass`，兼容 B 站/常见 XML/JSON）、播放器弹幕库（按文件指纹缓存）、手动导入/导出、在线 Provider（可选：B 站等，需用户配置并遵守 ToS） |
| **AI 扩展（后续）** | 弹幕翻译（外语弹幕→中文/双语）、弹幕精华/高能时刻（密度曲线→章节/片段）、AI 屏蔽降噪、弹幕摘要 |

### 6.2 渲染后端（双通道）

| 后端 | 原理 | 适用 | 优点 | 限制 |
|---|---|---|---|---|
| **A. egui 覆盖层**（主，Mode A） | `DanmakuManager` 驱动 egui `Painter` 在视频矩形上逐帧绘制（文字图集/批量合并），坐标从视频像素映射到 UI 点 | Linux Mode A（X11/Wayland 内嵌渲染） | 完全可控：动态样式/密度/交互/翻译气泡；与 AI 覆盖层同一体系 | 仅 Mode A；4K 下需高效文字渲染与上限控制 |
| **B. mpv ASS 弹幕轨**（兜底，Mode A/B 都能用） | Rust 布局引擎预生成带 `\move`/`\pos` 动画的 ASS → `sub-add` 为字幕轨（建议作为副字幕轨，`--secondary-sub-*`），由 libass 渲染 | **Windows Mode B 必须**；Mode A 可选 | 跨模式、无需 UI 覆盖、复用 libass 字体/描边/HDR 字幕峰值处理；性能好（预生成） | 样式变更需重新生成 ASS 再 `sub-reload`；无法悬停交互 |
| C. 透明覆盖窗口（后置） | Windows layered window / X11 ARGB 覆盖在子窗口上 | Mode B 的交互弹幕（未来） | 兼顾原生硬解与交互 | 跨平台实现复杂，暂不纳入 |

**默认策略**：Mode A = egui 覆盖层（体验/交互完整）；Mode B = ASS 弹幕轨（Windows 首选，保证 4K 原生渲染路径不被破坏）。两者共用同一套**布局引擎与数据模型**，保证同一弹幕在两种模式下时间/车道一致。

### 6.3 布局引擎（`wp-danmaku`，Rust 纯逻辑、可单测）

- **滚动弹幕车道分配**：按进入时间、文本像素宽度、滚动速度做间隔碰撞检测（同道前后弹幕不能追尾），优先选最早空闲车道；车道数按显示区域高度/字号计算。
- **顶部/底部固定弹幕**：按时段占用车道（同屏时间 = 停留时长），多个固定弹幕按先来后到分车道。
- **密度限制**：每秒/每车道最大条数、同屏上限（默认约 120–200 条）、超限丢弃策略（保大字号/彩色/高权重）。
- **坐标体系**：内部以「视频高度为 1.0 的相对坐标」计算，渲染时映射到 1080p/4K/窗口缩放，保证不同分辨率一致。
- **输出**：`Vec<DanmakuFrame>`（egui 用）或 ASS 文本（`\move` 动画，4K 字号换算），支持按时间段分片生成以控制 ASS 体积。

### 6.4 数据模型 / 来源 / 缓存

```text
wp-danmaku/
  model.rs       Comment { time_ms, mode(Scroll/Top/Bottom/Reverse), color, font_size, text, user, weight }
  parsers/       bilibili_xml.rs / json.rs / ass_import.rs / ass_export.rs
  layout.rs      车道分配 + 密度限制 + 4K 坐标换算（纯函数，大量单测）
  render_ass.rs  生成弹幕 ASS（含 \move/\pos、颜色/字号/透明度样式）
  render_egui.rs egui 覆盖层绘制（批量/文本图集/交互命中测试）
  providers/     local.rs（同名文件/弹幕库）/ online.rs（可选 Provider trait，B 站等）
  store.rs       文件指纹 → 弹幕库 JSONL；屏蔽词/偏好
```

- **文件指纹匹配**：本地文件用（size + 头尾各 1MB 的 hash）；网盘文件由 `wp-stream` 在预读时顺带计算（代理本来就在读字节流），播放首次完成后缓存映射 → 下次打开自动加载同名在线/本地弹幕。⚠ 网盘文件首次播放前不知道 hash，因此「自动匹配在线弹幕」会延迟到有指纹后（或用户手动选）。
- **弹幕库格式**：按文件指纹分目录存 JSONL（时间/模式/颜色/文本/来源），支持导入导出（B 站 XML 兼容）。
- **在线源**：`DanmakuProvider` trait（`match_episode(fingerprint/title)`, `fetch(episode)`），默认关闭，用户配置 Cookie/接口；注意外部接口稳定性与 ToS，不内置自动爬取。
- **发送弹幕**：默认写入本地弹幕库（即时显示）；在线发送做成可选 Provider 能力，失败回退本地。

### 6.5 与字幕 / AI / 播放内核的协作

- **层级**：视频 → 弹幕（上层）→ 字幕（最上层，可配置谁在上）；Mode A 由 egui 绘制顺序控制；Mode B 用 ASS 弹幕轨 + 主字幕轨，mpv 主/副字幕叠加顺序（`--secondary-sub-*`）需 M0 实测确认。
- **AI 弹幕（后期）**：翻译（在布局前替换文本，保持时间轴）、精华（按密度曲线生成「高能片段」章节）、AI 屏蔽（分类降噪）；复用 `wp-ai` 任务队列与缓存，弹幕翻译可整片/前瞻批量。
- **播放状态联动**：暂停冻结、倍速重排（按播放速率重算动画时长）、seek 后只加载目标时间窗、缓冲/卡顿时暂停弹幕推进。
- **性能**：egui 侧只绘制当前时间窗活跃弹幕（预计算每条弹幕的起止时间窗，二分查找）；ASS 侧预生成并按 5–10 分钟分片 `sub-reload`，避免一次加载数万条动画带来的 libass 压力。

### 6.6 弹幕验收指标

- 4K HEVC 播放 + **1000 条/分钟** 弹幕密度：丢帧 < 0.5%，CPU 增量 < 10%，UI 可交互。
- 弹幕与视频同步误差 < 50ms（暂停/seek/倍速后不漂移）。
- 切换弹幕开关/样式/密度：Mode A 立即生效（< 100ms），Mode B ASS 重生成 < 300ms。
- 本地弹幕自动加载准确率：同名 XML/JSON/ASS 命中 100%；指纹库跨次播放命中。
- 屏蔽词/正则/类型过滤生效且不影响合法弹幕。

---

## 7. AI 子系统架构（后期功能的地基）

### 7.1 设计原则

1. **播放优先**：AI 推理在**独立进程**（worker），通过 IPC 与本进程通信；播放期间任务自动降优先级/暂停（保护 4K 解码与帧同步）。
2. **Provider 可插拔**：本地（whisper.cpp/llama.cpp/candle/ONNX Runtime）与远端（OpenAI 兼容：OpenAI/DeepSeek/Qwen 等）统一 trait；可全离线。
3. **事件驱动**：AI 任务由播放事件（打开/暂停/seek/字幕 cue/截图）触发，结果异步回 UI 或注入播放器。
4. **缓存可复用**：按（文件指纹/大小/mtime + 轨道 + 目标语言 + 模型）缓存翻译/ASR 结果（SQLite/JSON），跨次播放零成本。
5. **隐私可控**：默认本地；远端需显式配置密钥并提示数据外发；密钥加密存储（OS keyring 可选）。
6. **源码可控**：本地模型运行时全部开源可自建（whisper.cpp/llama.cpp/candle），不绑定闭源 SDK；ONNX 仅用于自备模型。

### 7.2 模块与接口（设计稿）

```text
wp-ai/
  provider.rs     trait AiProvider { translate / transcribe / summarize / vision / ocr }
  providers/
    local_whisper.rs   (whisper-rs / whisper.cpp，ASR)
    local_llm.rs       (llama-cpp-2 或 candle，本地翻译/摘要)
    onnx.rs            (ort，专用模型：OCR/超分/检测)
    remote_openai.rs   (OpenAI 兼容 REST，可选)
  tasks.rs        任务类型/优先级/取消/超时/重试
  cache.rs        结果缓存（文件指纹+语言+模型）
  pipeline/
    translate_subtitle.rs  字幕翻译（整片/实时）
    asr_subtitle.rs        ASR 生成字幕（音频提取→VAD→识别→字幕）
    summarize.rs           摘要/章节/问答（文本或帧）
  worker.rs       独立进程入口（IPC：JSON/CBOR over stdio/socket）
```

### 7.3 AI 功能路线（后期）

| 优先级 | 功能 | 依赖挂钩 |
|---|---|---|
| P1 | **字幕翻译（双语）**、术语表、繁简转换、字幕润色 | `sub-text`/`sub-start` 观察 + `sub-add`/`sub-reload` |
| P2 | **无字幕视频 ASR 生成字幕**（中文/英文/自动语言）、字幕纠错 | 音频提取（本地/代理缓存）+ whisper |
| P3 | 视频摘要/章节/问答、台词搜索、精彩片段、自动片头片尾跳过 | 时间轴 + 字幕全文 + 帧/音频采样 |
| P4 | PGS/VobSub 位图字幕 OCR 翻译、超分/降噪（GLSL/ONNX）、场景识别 | `screenshot-to-file`/render API 读帧 + OCR/SR 模型 |

> P1 在 M4 落地；P2 在 M6；P3/P4 为后续专项。所有 AI 功能必须可在设置里单独关闭、可完全离线。

---

## 8. 播放源接入（网盘 / 本地 / ISO）

| 源 | 打开方式 | 关键处理 |
|---|---|---|
| 网盘文件 | `Drive::direct_link()` → `wp-stream` 代理 URL → mpv `loadfile` | 代理注入 headers、预读、换链；多盘 headers 差异（百度/夸克需 Cookie，光鸭签名直连） |
| 本地文件 | `loadfile path`（或 `--wid` 模式） | 直接；无字幕时自动同名匹配；本地 4K 同硬解路径 |
| 蓝光 ISO/BDMV | `--bluray-device` + `bd://`；先 libbluray 列标题给用户选 | 主标题默认选 longest；章节/角度/音轨/字幕轨选择；菜单实验特性见 §4.4 |
| 目录批量 | 播放列表（同目录视频顺序/循环） | 复用目录列表 + 缩略图 |

**播放器会话状态机**：`Idle → Opening（取链/解复用） → Buffering（预读达标） → Playing/Paused → Seeking → Ended/Error`；错误分类：链接失效（自动换链重试）、网络差（降码率不可能→提示）、解码失败（切软解/换渲染模式/外部播放器）、文件损坏。

---

## 9. 源码可控与构建发行

### 9.1 vendor 结构（建议）

```
vendor/
├─ mpv/                 # git submodule / 固定 commit（0.41.0 或 0.42-dev pin）
├─ ffmpeg/              # 固定 tag，LGPL 配置
├─ libass/  libplacebo/ # 固定 tag
├─ libbluray/ libaacs/ libbdplus/（可选，AACS/BD+）
└─ patches/             # 我们对上游的补丁（LGPL 合规发布）
scripts/
├─ build-media-stack-linux.sh     # 依赖 + mpv（-Dgpl=false -Dlibmpv=true）→ libmpv.so
├─ build-media-stack-windows.ps1  # MSYS2/MinGW 构建 → libmpv-2.dll + 依赖 DLL
└─ package-player-runtime-*.sh    # 打包到发行目录（含 LICENSE 与源码获取说明）
crates/
├─ wp-media-mpv/        # 自研 FFI 安全封装 + render bridge（不依赖 bindgen 运行期）
│   └─ src/bindings.rs  # 预生成 bindings（随 pin 的 mpv 头文件，checked in）
├─ wp-player/           # 编排/状态机/断点续播/媒体库
├─ wp-stream/           # 流代理+磁盘缓存
├─ wp-subtitle/  wp-ai/
```

- **Rust 侧不依赖 bindgen/clang**：bindings 预生成并提交，CI 校验与 pinned 头文件一致；构建只需要链接 `libmpv`（vendored 路径或系统 `pkg-config` 可选）。
- **系统库兼容**：cargo feature `system-mpv`（Linux 发行版自带 libmpv）与默认 `vendored-mpv`（我们构建，版本可控）二选一；两者 ABI 需匹配 pin 版本（声明最低/最高版本检测）。
- **许可证合规**：动态链接 LGPL libmpv；发行包含 `LICENSE.LGPL`、依赖许可清单、源码获取说明（可提供 vendor 源码包/补丁）；不静态链接 LGPL 库；MPV_GPL 构建可选但单独标注。
- **可复现构建**：固定工具链（Rust stable + MSYS2 包版本）、CI 产出 Linux/Windows 播放器运行时包；record 构建日志哈希。

### 9.2 为什么自研 Rust 封装而不是 crate

- `libmpv2 6.0.0`（LGPL-2.1 声明，2026-05 更新）可用，但：我们需要的 render API + D3D11/GL 回调、属性观察、精确生命周期与线程规则、版本 pin 校验都要可控；**自研薄封装（约 40–80 个符号）**更符合「源码可控 + 可审计」，也能对上游补丁友好。
- 可参考 `libmpv2`/`egui-sharkplayer` 的 render 集成思路（eframe glow + FBO + egui_glow CallbackFn），但代码放在我们仓库。

---

## 10. 关键接口草案（仅设计，不实现）

```rust
// wp-player
pub trait PlayerBackend: Send {
    fn open(&mut self, source: MediaSource) -> Result<()>;
    fn command(&mut self, cmd: PlayerCommand);
    fn snapshot(&self) -> PlayerSnapshot;           // 位置/时长/状态/轨道/音量/缓冲
    fn set_observer(&mut self, tx: Sender<PlayerEvent>);
}

pub struct PlayerSnapshot {
    pub state: PlayState,             // Idle/Opening/Buffering/Playing/Paused/Seeking/Ended/Error
    pub position_secs: f64,
    pub duration_secs: Option<f64>,
    pub cache_secs: f64,              // 已缓冲时长（wp-stream 上报）
    pub tracks: Vec<TrackInfo>,       // 音/视/字幕（含语言、title、default、codec）
    pub current_audio: Option<i64>,
    pub current_sub: Option<i64>,
    pub secondary_sub_visible: bool,
    pub disc: Option<DiscInfo>,       // 蓝光标题/章节/角度/菜单态
    pub volume: f64,
    pub hwdec: HwDecState,            // 实际硬解器（用于「丝滑」诊断面板）
}

pub enum MediaSource {
    Local(PathBuf),
    NetDisk { drive: String, path: String },      // wp-stream 处理 DirectLink
    BluRay { iso_or_dir: PathBuf, title: Option<u32> },
}

pub enum PlayerCommand {
    Play, Pause, SeekAbsolute(f64), SeekRelative(f64), SetVolume(f64), SetRate(f64),
    SelectTrack { kind: TrackKind, id: i64 }, SetSubtitleDelay(f64),
    AddSubtitleFile { path: PathBuf, select: bool },
    ReloadSubtitles, Screenshot { path: PathBuf }, ToggleFullscreen, Stop,
}
```

```rust
// wp-stream：给 mpv 的本地 URL + 预读状态
pub struct StreamHandle { pub url: String, pub token: String, pub drive: String, pub path: String }
impl StreamProxy {
    pub async fn open(&self, drive: Arc<dyn Drive>, path: &str) -> Result<StreamHandle>;
    pub fn stats(&self, token: &str) -> StreamStats;   // buffered_ranges/bitrate/errors
}
```

```rust
// wp-subtitle：AI 双语字幕注入
pub struct Cue { pub start: f64, pub end: f64, pub text: String, pub style: Option<String> }
pub fn build_bilingual_ass(original: &[Cue], translated: &[Cue], opts: &AssOptions) -> String;
// 写入缓存文件后：mpv 命令 `sub-add <file> select` / `sub-reload`
```

---

## 11. 风险与开放问题

| ID | 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|---|
| R1 | Linux X11 EGL/VAAPI 零拷贝在部分驱动失败 | 中 | 4K 掉帧 | M0 实测矩阵；降级 `vaapi-copy`/软件解码；X11 可选 GPL 构建用 Mode B |
| R2 | Windows Mode A（GL/WGL）无法硬解 | 高 | 4K 卡 | 架构已定 **Windows 主选 Mode B**（原生 d3d11va/HDR） |
| R3 | 蓝光交互菜单依赖 0.42-dev | 高 | 菜单不可用 | v1 只承诺主标题播放；菜单作实验 feature；同时保留外部 VLC/mpv 兜底 |
| R4 | UHD 蓝光 AACS2 加密无开源解密 | 确定 | 无法播加密 UHD 原盘 | 文档明示；仅未加密/已解密备份；建议用户先备份为 BDMV/MKV |
| R5 | LGPL 构建缺少 X11 vo：Linux X11 无法用 Mode B | 确定 | 需 GPL 构建才能 Mode B | 默认 Mode A；如用户要 X11 Mode B，提供 GPL 构建 profile（明确许可影响） |
| R6 | 4K60 / 8K / AV1 高码率带宽不足 | 中 | 卡顿 | 磁盘预读 + 带宽探测 + 提示码率；8K 需硬解与高速网络 |
| R7 | AI 抢占 GPU/CPU 影响播放 | 中 | 帧率波动 | AI 独立进程 + 优先级 + 播放期暂停重任务；GPU 显存/占用监控 |
| R8 | mpv master 稳定性 | 中 | 崩溃/回归 | 默认用 0.41.0；菜单 feature 用固定 commit + 回归矩阵；可一键切回 |
| R9 | 内嵌位图字幕（PGS）无法直接文本翻译 | 确定 | AI 翻译受限 | P4 OCR 方案；v1 文案说明 |
| R10 | 打包体积（Windows 运行时 DLL ~60–100MB） | 中 | 安装包大 | 运行时包独立下载/按需安装；或系统 libmpv 方案 |
| R11 | Mode B（Windows 原生窗口）无法叠加 egui 弹幕层 | 高 | 交互弹幕缺失 | Mode B 用 **ASS 弹幕轨**渲染（无交互但显示完整）；交互弹幕仅 Mode A，或后续透明覆盖窗口 |
| R12 | ASS 弹幕在 4K/高密度下的 libass 性能与双轨叠加顺序 | 中 | 卡顿/层级错误 | M0 实测 `\move` 动画 + 主/副字幕轨叠加；分片 `sub-reload`、密度上限、必要时降级为纯 egui 模式 |
| R13 | 在线弹幕源（B 站等）接口不稳定/合规风险 | 中 | 功能不可用/法务 | 默认仅本地弹幕；在线源做成可选 Provider（用户配置、遵守 ToS），不内置爬取 |

### 待用户确认

1. Windows 接受 **Mode B 原生窗口嵌入**（控制条在视频区域外）作为主形态吗？还是必须「UI 完全覆盖视频」？（后者需等 D3D11 render API/ANGLE 路线）
2. 蓝光需求是「**主标题流畅播放**」即可，还是必须**原盘交互菜单**（BD-J/HDMV）？
3. AI 优先本地离线（需 GPU/CPU 推理、模型体积大）还是允许云端 API（省资源、需联网/密钥/隐私考虑）？
4. 播放器是否可接受引入 `libmpv` 运行时（Windows 发行包 +60–100MB）？还是要求全部静态打进单文件（LGPL 静态链接合规成本高，不推荐）？
5. 优先级：先「4K+字幕+网盘」还是先「AI 字幕翻译」？
6. **弹幕来源**：只做本地弹幕（同名 XML/JSON/ASS + 本地库），还是需要内置/可配置的在线弹幕源（B 站等）？
7. **弹幕交互深度**：只要「显示 + 开关/样式/屏蔽」即可，还是需要悬停/点赞/发送/搜索跳转等交互（交互仅在 Mode A 可用）？
8. 弹幕与字幕的**前后层级**偏好（弹幕在上还是字幕在上）？是否需要弹幕翻译/精华等 AI 弹幕功能进 v1 范围？

---

## 12. 证据索引（本次新增的关键事实）

| 事实 | 证据 |
|---|---|
| mpv GPL/LGPL 双许可与 `gpl=false` 选项 | `mpv/meson.options:2`；`LICENSE.GPL`、`LICENSE.LGPL` 并存 |
| LGPL 构建禁用的功能（x11/dvdnav/dvda/cdda/dvbin/jack/oss/caca/direct3d） | `mpv/meson.build:643-670,677-681,886-928,977-992,1125-1133` |
| `d3d11` 渲染/上下文未受 gpl 限制（Windows Mode B 可用） | `mpv/meson.build:1066-1074` |
| render API：v0.41 仅 OpenGL + SW | `mpv/v0.41.0:include/mpv/render.h`（grep D3D11 无命中） |
| render API：master 新增 D3D11 API | `mpv/master:include/mpv/render.h:52,480-481`；`video/out/d3d11/libmpv_d3d11.c`；`include/mpv/render_d3d11.h` |
| render API 关键参数/同步 | `render.h:MPV_RENDER_PARAM_ADVANCED_CONTROL(289)`、`BLOCK_FOR_TARGET_TIME(319)`、`OPENGL_FBO(204)` 等 |
| 完整字幕命令/属性 | `sub-add:687`、`sub-reload:749`、`track-list:3529`、`sub-text:3339`、`secondary-sub-text:3363`、`sub-start:3366`、`secondary-sub-start:3376`、`secondary-sub-visibility(option):3031` |
| 字幕选项族（自动加载/字体/样式/缩放/位置/延迟） | `options.rst:2899(sub-auto)`、`3006(sub-file-paths)`、`3351(sub-fonts-dir)`、`2608(embeddedfonts)`、`2721(sub-ass-override)`、`2512(sub-delay)` 等 |
| 位图字幕由 LGPL 代码路径处理 | `mpv/sub/sd_lavc.c` 文件头 LGPL-2.1+；`sub/` 目录 |
| 硬件解码要求（VAAPI/D3D11VA/NVDEC） | `options.rst:1326-1430`（如 d3d11va 需 `--gpu-context=d3d11/angle`） |
| HDR/色彩管理 | `options.rst:7346(target-colorspace-hint)`、`7675(tone-mapping)`；release notes: gpu-next 为 0.41 默认 |
| 蓝光设备/角度 | `options.rst:3963(--bluray-device)`、`4019(--bluray-angle)` |
| v0.41 已有 `bd://menu` 标题与 `BD_EVENT_MENU` | `mpv/v0.41.0:stream/stream_bluray.c:59,132,527-528` |
| 交互菜单 overlay + `disc-menu-active` 仅 master | `mpv/master:stream/stream_bluray.c`（`bd_register_overlay_proc`/`bd_register_argb_overlay_proc`、overlay plane 结构）；`input.rst:2488(disc-menu-active)` |
| mpv 支持任意 HTTP 头/Referer/Cookie | `options.rst:5714(--http-header-fields)`、`5765(--referrer)`、`5823(--cookies*)` |
| 依赖许可：libass ISC；libplacebo LGPL-2.1；libbluray/libaacs/libbdplus LGPL-2.1+ | 各自 `COPYING`/GitHub API；`code.videolan.org/.../COPYING` 实测抓取 |
| AI 生态 crates | whisper-rs 0.16（Unlicense）、candle-core 0.11（MIT/Apache）、ort 2.0-rc（MIT/Apache）、llama-cpp-2、symphonia 0.6（MPL-2.0） |
| 本项目接入点 | `DirectLink` `wp-core/src/model.rs:303-313`；三盘 headers `wp-drive-*/src/download.rs`；动图/缩略图 `wp-gui/src/thumbs.rs`；页面枚举 `wp-gui/src/app.rs:21-29`；双击入口 `open_entry app.rs:1030`、`browser.rs:579-581` |
| 弹幕可用的 mpv 能力 | 动态字幕轨 `sub-add:687`/`sub-reload:749`/`sub-remove`；主+副字幕（`--secondary-sub-visibility` options.rst:3031、`--secondary-sub-*` 系列）；ASS 动画由 libass 渲染（`\move`/`\pos` 标准 ASS 能力）；HDR 字幕峰值控制（gpu-next）|
| 弹幕性能/坐标注意 | egui 覆盖层需按视频矩形映射（Mode A）；ASS 方案按 4K 字号生成、分时段 `sub-reload` 控制 libass 压力；4K 高密度需实测 |

---

## 13. 已确认决策（2026-10-19，用户拍板）

| # | 问题 | 决策 | 对架构的影响 |
|---|---|---|---|
| 1 | Windows 窗口形态 | **不接受原生窗口嵌入** | 移除 Mode B 主方案；Windows 走 Mode A + ANGLE（见 [PLAYER_WINDOWS_ANGLE.md](PLAYER_WINDOWS_ANGLE.md)，`vendor/glutin-winit` 已打补丁） |
| 2 | 蓝光范围 | **只需主标题流畅** | v1 不做 BD-J/HDMV 交互菜单；`bluray-menu` 特性转为未来可选项；v0.41.0 固定版本即可 |
| 3 | AI 本地/远端 | **两者都要**，可分别启用/停用、设置优先级 | `wp-ai` 按 priority 选择与回退；UI 提供 provider 开关/优先级；离线/在线均可 |
| 4 | Windows 运行时体积 | **接受 60–100MB** | 发行包内置 `播放器运行时/`（libmpv + 依赖 DLL + ANGLE + LICENSES） |
| 5 | 功能优先级 | 确认顺序 | 4K+字幕+网盘 → AI 字幕翻译 → ASR → 摘要/问答 → OCR/超分 |
| 6 | 弹幕数据源 | **在线 + 本地** | `wp-danmaku` 增加在线 Provider（可选配置/缓存/合规提示），本地同名文件/弹幕库保留 |
| 7 | 弹幕交互 | **显示+开关+样式+屏蔽+悬停/发送/搜索跳转** | 交互弹幕用 egui 覆盖层（Mode A）；发送默认写本地库、在线发送可选；搜索/时间轴热力图纳入 UI |
| 8 | 弹幕/字幕层级 & AI 弹幕 | 层级**可配置**；AI 弹幕（翻译/精华）**列入后续版本** | 层级配置进播放器设置；AI 弹幕任务进 `wp-ai` 路线（M7） |

## 14. 阶段计划摘要

详细任务见 [PLAYER_TASKS.md](PLAYER_TASKS.md)（v2）。总体：

- **M0 技术验证（必做，6–10 人日）**：LGPL media-stack 构建（Linux/Windows）；eframe + render API 收帧；4K HEVC 硬解矩阵（Linux EGL/VAAPI + **Windows ANGLE/D3D11VA**）；字幕格式矩阵（含 PGS/双字幕）；BD ISO **主标题**探测（菜单不做）；流代理缓存原型；ASS 弹幕轨 + 副字幕轨叠加实测；AI 挂钩 spike。
- **M1 播放核心**（8–12）：`wp-media-mpv` + `wp-player` + Mode A/B 渲染桥 + 状态机 + 4K 性能达标。
- **M2 完整字幕**（6–8）：字幕控制/双字幕/样式/外挂自动加载/持久化 + 字幕矩阵。
- **M3 源接入**（8–10）：网盘（代理/换链/缓存）、本地、ISO/BDMV 标题选择、播放列表、入口接线。
- **M4 AI 基础设施 + 字幕翻译**（8–12）：`wp-ai` worker/provider/缓存 + P1 双语翻译。
- **M5 弹幕系统**（10–14）：`wp-danmaku` 模型/解析（本地+在线 Provider）/车牌布局引擎 + egui 覆盖层（悬停/发送/搜索跳转/时间轴热力图）+ ASS 弹幕轨兜底 + 本地弹幕库/自动加载 + 屏蔽/密度/层级配置。
- **M6 性能/打包/验收**（6–8）：Windows/Linux 运行时打包、许可合规、性能基线、文档、回归。
- **M7 AI/弹幕进阶**（10–18，按需）：ASR 字幕（P2）、摘要/问答（P3）、OCR/超分（P4）、**AI 弹幕（翻译/精华）**。
- 合计约 **55–85 人日**（单人 3–4 个月；两人 6–8 周）。M0 结论可能调整后续估算。
