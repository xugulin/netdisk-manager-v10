# 播放器页面：只读调研报告与方案评估

> 日期：2026-10-19 · 状态：**只读调研完成，未写任何实现代码**（按用户要求：先报告 + 任务计划 + 方案评估）
> 调研对象：[tealinuxos/tealinux-iso](https://github.com/tealinuxos/tealinux-iso) · [videolan/vlc](https://github.com/videolan/vlc)
> 目标：为「**播放网盘视频** + **播放本地视频**」选定可集成进本项目（Rust / eframe+egui）的播放方案。
> 配套任务计划见 [PLAYER_TASKS.md](PLAYER_TASKS.md)。
>
> ⚠️ **2026-10-19 需求升级修订**：用户新增「**完整字幕** / **4K·ISO 蓝光原盘流畅** / **AI** / **源码可控** / **后续弹幕**」要求。
> 本轮推荐的「libVLC 3.x vmem」已不满足（vmem 丢字幕、VLC4 未发布、无 GPU 嵌入路径），
> **修订后的目标架构见 [PLAYER_ARCHITECTURE.md](PLAYER_ARCHITECTURE.md)**；
> 本文保留为第一轮只读调研记录（VLC 许可证/嵌入 API、Tea Linux 发行范式等结论仍有参考价值），
> 第二轮针对 mpv 的补充事实见文末「附录 A」。

---

## 0. TL;DR（结论先行）

| 项 | 结论 |
|---|---|
| **推荐方案** | **libVLC 3.0.x（LGPL-2.1+）动态加载 + 自研薄 FFI + 本地流代理 + vmem 回读渲染**；无 libVLC 时回退「外部播放器」 |
| **为什么不是 vlc-rs** | 该 crate 最后一次更新 2018 年，只封装了**音频**回调，没有视频 vmem 回调；VLC 4 新 API 完全未覆盖。FFI 面很小（约 30 个函数），自研更可控 |
| **为什么不直接 libmpv** | libmpv 渲染性能最好（有 `egui-sharkplayer` 先例），但 mpv 核心默认 GPLv2+，MIT 项目要捆绑就必须自建 `-Dgpl=false` 的 LGPL 版 libmpv，发行与编译成本高 → 列为 Phase 2 备选 |
| **为什么不 GStreamer** | LGPL 友好、Linux 成熟，但 Windows 运行时打包重（100MB+ 插件）、插件矩阵复杂，且同样需要代理解决 Cookie |
| **必须做的基础设施** | **本地流代理**：VLC 的 HTTP 访问只有 `http-user-agent/http-referrer/http-proxy` 选项，**没有 Cookie/任意 Header**（`modules/access/http.c:182,217,220`），而百度/夸克直链必须带 Cookie → 代理统一注入头、透传 Range、过期自动换链 |
| **最大未验证项** | 三盘直链的 **HTTP Range** 支持（决定「随机 seek」）。此前真机下载 12/20/23MB 走的是单流（分段阈值 32MB），不能证明 206 → 列为 M0 实验 |
| **VLC3 vmem 的硬伤** | vmem 模块**丢弃 subpicture**（`modules/video_output/vmem.c:245-279`）→ 内嵌字幕/OSD 不显示；v1 接受此限制或走外部播放器，Phase 2 评估 VLC4 GPU 路径/libmpv |
| **版本格局** | videolan 官网最新稳定版 **3.0.24**；master 是 4.x 开发线、4.0 未正式发布 → 生产目标锁定 3.0.x，接口设计预留 4.x |

---

## 1. 调研方法与边界

- **只读原则**：不 clone 全仓（VLC 仓库约 700MB、tealinux-iso 约 520MB），改用 GitHub API 取文件树 + 镜像拉取需要的文件；Rust 生态用 crates.io `.crate` 包解包读源码。未修改两个源仓库任何文件。
- **阅读范围**：
  - VLC：`include/vlc/libvlc_media_player.h`（3.0.x 与 master 各一份）、`libvlc_media.h`、`libvlc.h`、`libvlc_events.h`、`modules/video_output/vmem.c`、`modules/access/http.c`、`doc/libvlc/{player.c,CHANGES-v3-to-v4.md,sdl_opengl_player.cpp,d3d11_player.cpp,gtk_player.c}`；
  - Tea Linux：`build.sh`、`tealinux/{profiledef.sh,packages.x86_64}`、3 个 `PKGBUILD`、`docs/add-tealinux-modularity.md`、`.gitmodules`；
  - Rust 生态：`vlc-rs 0.3.0`、`libvlc-sys 0.2.0`、`libmpv2 6.0.0`、`egui-video 0.9.0`、`egui-sharkplayer 0.5.1`、`door_player 0.3.22` 源码/清单 + crates.io 元数据；
  - 本机媒体栈实测（pkg-config / ldconfig / gst-inspect）。
- **事实优先级**：源码原文/头文件注释 > 官方文档/示例 > crates 元数据 > 博客（未采信）。

### 1.1 本机环境探测（决定实验成本的硬事实）

| 项 | 结果 | 影响 |
|---|---|---|
| libvlc 开发库 | ✅ `pkg-config --modversion libvlc` = **3.0.23**；`libvlc.so.5.6.1` / `libvlccore.so.9` 存在 | 可编译/链接 VLC 后端 |
| libvlc 运行时插件 | ⚠️ `/usr/lib/vlc/plugins/` 只有 `plugins.dat`（24B），**无实际插件** | 本机直接播片会失败，M0 需先装 `vlc` 运行时包 |
| VLC/mpv 可执行文件 | ❌ 未安装（`vlc`/`mpv` 均无） | 外部播放器兜底需目标机装有播放器 |
| GStreamer | ✅ 1.28.8，有 `appsink/videoconvert/videoscale/pipewiresink`；❌ 缺解码器（`avdec_h264/h265`、`vp8dec/vp9dec`、`aacdec` 均 MISSING） | GStreamer 方案需装 `gst-libav` 等插件 |
| FFmpeg 库 | ✅ `libavformat/libavcodec 63.1.102` | 纯 FFmpeg 路线有库，但实现成本高 |
| 字体 | ✅ Noto Sans CJK | 播放页 UI 无字体问题 |

> 结论：**两条内嵌引擎都要在测试机补运行时**（VLC 补插件、GStreamer 补解码器）。这对方案评估影响不大（发行时反正要处理依赖），但 M0 实验要先解决。

---

## 2. 仓库 A：tealinuxos/tealinux-iso —— 学「发行与打包」，不学「播放实现」

### 2.1 它是什么

- Arch Linux 系的 **Plasma 6 桌面发行版 ISO 构建工程**（`mkarchiso`），仓库约 520MB/5 万文件，主导语言 Shell（GitHub API 元数据）。
- 关键文件：`build.sh`（mkarchiso 构建入口）、`tealinux/profiledef.sh`（ISO 名称/引导/压缩配置）、`tealinux/packages.x86_64`（**217 个包**的 ISO 包清单）、`localrepo/`（自定义包本地仓库）、`tealinux-installer/`、`tealinux-modularitea/`（Rust/Tauri 应用 PKGBUILD）。
- 仓库里**没有播放器实现、也没有 Rust 播放代码**（全树检索 `Cargo.toml`/`*.rs` 无命中，媒体播放相关文件名 0 命中）→ 它的价值只在**发行方式与桌面集成范式**。

### 2.2 与播放器相关的关键发现（带证据）

1. **ISO 不预装任何视频播放器**：`packages.x86_64` 里没有 `mpv`/`vlc`/`haruna`/`celluloid`/`gstreamer` 插件；只有：
   - `pipewire`、`pipewire-pulse`、`wireplumber`、`sof-firmware`（音频栈，第 48–51 行）；
   - `ffmpegthumbs`（ffmpeg 缩略图，第 121 行）、`kdegraphics-thumbnailers`；
   - Plasma 6 桌面（`plasma-desktop` 等）、`dolphin`、`gwenview`。
   → **如果我们的播放器要进这个 ISO/发行版，必须自己把播放运行时写进依赖**（`libvlc`/`gstreamer` 插件），不能指望系统自带。
2. **自定义应用的分发范式**（`docs/add-tealinux-modularity.md`）：
   `PKGBUILD → 打本地包 → localrepo/ → 注册进 packages.x86_64 → mkarchiso 构建 ISO`。
   Rust/Tauri 先例：`tealinux-installer-git`（`makedepends=("rust")`、`depends=("webkit2gtk-4.1")`、安装 `target/release/tealinux-installer` 到 `/usr/bin/`、`.desktop` 到 `/usr/share/applications/`）、`tealinux-modularity-git`（同款）。
   → 本项目（纯 Rust、无 WebView）完全可以照此打包：`depends=('vlc')`（Arch 的 `vlc` 包提供 libvlc + 插件）或打包时捆绑；`.desktop` 安装位置一致（我们已有桌面文件与安装脚本，可直接复用并加 `MimeType=video/*`）。
3. **桌面/音频基线**：Plasma 6 + PipeWire + `plasma-pa`；播放器音频直接交给系统输出即可（libVLC 的 Pulse/PipeWire 兼容输出、GStreamer 的 `pipewiresink` 均可用）。
4. **构建机有 `notif.mp3` 且 build.sh 用 `mpv notif.mp3`**——只是构建机提示音，ISO 内无 mpv。

### 2.3 对项目的三条可执行结论

- 发行面：以 PKGBUILD（Arch/Tea Linux）或 AppImage/绿色版（含 `libvlc/` 子目录）分发；`depends` 里显式声明播放运行时。
- 桌面集成：`.desktop` 增加 `MimeType=video/*;audio/*;` 与 `Categories` 补充；安装脚本已就绪。
- 不把「系统自带播放器」当默认前提——必须有自己的运行时策略（捆绑/依赖/外部兜底三选一）。

---

## 3. 仓库 B：videolan/vlc —— libvlc 嵌入能力是核心

### 3.1 版本与许可（决定技术路线）

- 整体 license：GitHub 元数据 **GPL-2.0**；但 **libvlc 公共头文件是 LGPL-2.1+**，证据：
  `include/vlc/libvlc_media_player.h` 头注释 —— *"GNU Lesser General Public License … version 2.1 or later"*（`vlc3/libvlc_media_player.h:12-20`）。
  → **MIT 项目动态链接 libvlc 是可行的**（保持动态链接、可替换、附 LGPL 声明）。
- 版本格局：videolan 官网最新稳定 **3.0.24**（2026-10 实测抓取）；`master` 是 **4.x 开发线**（回调模型重构，`doc/libvlc/CHANGES-v3-to-v4.md`），**4.0 尚未正式发布**。
  → 生产目标锁定 **3.0.x**；FFI 层做版本探测/降级。

### 3.2 嵌入视频的两条技术路线

#### 路线 A：CPU 回读（vmem 回调）—— 3.x/4.x 都有，跨平台最稳

API（3.0.x 头文件）：

- `libvlc_video_set_callbacks(mp, lock, unlock, display, opaque)` — `vlc3/libvlc_media_player.h:418`
- `libvlc_video_set_format(mp, chroma, w, h, pitch)` — `:441`（RV32/YUYV 等，所有 plane 同 pitch）
- `libvlc_video_set_format_callbacks(mp, setup, cleanup)` — `:455`（可协商 I420/NV12 多平面，推荐）

上游头文件明确写的**性能警告**（`vlc3/libvlc_media_player.h:386-412`，原文摘要）：

- 自定义内存渲染 **"considerably less efficient"**，推荐窗口嵌入；
- 硬件解码要么被禁用、要么需要从显存回读到内存（慢）；
- 字幕/OSD 等 sub-picture 必须由 CPU 合成而不是 GPU；
- 像素格式转换/缩放/旋转都由 CPU 做；
- 参考帧缓冲与应用缓冲之间需要 memcpy。

实现细节（`modules/video_output/vmem.c`，LGPL-2.1 头）：

- 默认 `vmem-chroma` = **RV16**（`:72`），可通过 `:vmem-chroma=` 覆盖；
- `Prepare()`（`:245`）里 `lock → picture_CopyPixels → unlock`，`Display()`（`:271`）只回调通知；
- **subpicture 被忽略**：`Prepare` 里 `(void) subpic;`、`Display` 里 `VLC_UNUSED(subpic)` → **字幕/OSD 不会出现在回读帧里**；要字幕只能靠窗口嵌入/GPU 路径或自己做字幕。

→ 这是 MVP 最省事、跨平台（含 Wayland）的路线，**但字幕是硬伤、4K 有 CPU 压力**。

#### 路线 B：GPU 渲染（VLC 4 的 output callbacks）—— 性能最优，但等 4.0

API（master 头文件）：

- `libvlc_video_set_output_callbacks(mp, engine, setup, cleanup, …resize, swap, make_current, get_proc_address…)`（`vlc4/libvlc_media_player.h:1222+`）
- engine 枚举：`libvlc_video_engine_opengl / gles2 / d3d11 / d3d9 / anw / disable`（`:1233-1241`）
- 官方示例 `doc/libvlc/sdl_opengl_player.cpp`：
  `libvlc_video_set_output_callbacks(..., libvlc_video_engine_opengl, setup, cleanup, nullptr, resize, swap, make_current, get_proc_address, …)`（`:82-84`），
  `resize` 里建 FBO+纹理并声明 `render_cfg->u.opengl_format = GL_RGBA`（`:119-155`），`swap` 回调提示新帧（`:174-196`）；
- Windows 示例 `doc/libvlc/d3d11_player.cpp:757`（engine=d3d11）；
- 该路径下字幕由 VLC 渲染进纹理（仅 ANativeWindow 场景另有 subtitle surface，`:1116-1119`）。

→ 性能/字幕都最好，且 egui 的 glow 后端正好是 OpenGL，可复用（`egui::PaintCallback` + `egui_glow::CallbackFn` 已具备：`egui/src/lib.rs:458`、`egui_glow/src/lib.rs:15`、`egui_glow/src/painter.rs:117`）。
**但 VLC 4 未正式发布，API 还在变** → Phase 2 再评估。

#### 路线 C：窗口嵌入（set_xwindow / set_hwnd / set_nsobject）

- `libvlc_media_player_set_xwindow`（`vlc3/libvlc_media_player.h:543`）、`set_hwnd`（`:567`）、`set_nsobject`（`:493`）；
- 头文件警告：X11 句柄必须活到播放停止，否则 EGL/GLX/VA/VDPAU 可能崩溃（`:530-540`）；
- 只支持 X11/Win32/macOS，**Wayland 无此能力**；在 egui 窗口里创建并定位子窗口，跨平台要写三套，且事件/缩放/全屏同步麻烦。
→ 不作为主路线，仅在「需要字幕的高性能模式（X11/Windows）」时作为可选后端。

### 3.3 音频 / 控制 / 事件 / 轨道 API（做播放页够用）

| 能力 | API（3.0.x 头文件行号） |
|---|---|
| 播放/暂停/停止 | `play:251` `pause:268` `set_pause:260` `stop:275` `is_playing:243` |
| 进度/时长 | `get_length:788` `get_time:796` `set_time:805` `get_position:813` |
| 音量/静音 | `libvlc_audio_set_volume` / `get_volume` / `set_mute`（同文件音频段） |
| 倍速 | `libvlc_media_player_set_rate`（3.x 有，需逐版本确认符号） |
| 轨道/字幕轨 | `libvlc_audio_get/set_track`（`1815-1842`）、`libvlc_video_get/set_track`（`1372-1390`）、`libvlc_video_get_spu/set_spu` |
| 事件（UI 状态更新） | 事件枚举 `libvlc_events.h:59-81`：`MediaPlayerOpening/Buffering/Playing/Paused/Stopped/EndReached/EncounteredError/TimeChanged/PositionChanged/SeekableChanged/LengthChanged/Vout/ESAdded/ESSelected` |
| 音频回调（一般不需要） | `libvlc_audio_set_callbacks`（`:703`）、`libvlc_audio_set_format(_callbacks)` —— MVP 建议**不接管音频**，让 VLC 直接输出到系统 |

### 3.4 网络流能力（决定「需要代理」的关键证据）

- `modules/access/http.c` 支持的 per-media 选项：`http-user-agent`（`:182`）、`http-referrer`（`:217`）、`http-proxy`（`:220`）、`http-reconnect`（`:71`）；
- **没有 Cookie、也没有任意 Header 注入**（全文件 grep `cookie` 无命中）；
- `libvlc_media_add_option`（`libvlc_media.h:520`）可传 per-media 选项，但文档明确警告：部分选项（尤其视频/字幕渲染类）必须通过 `libvlc_new()` 全局设置才生效；
- 而本项目直链需要什么头？见下节——百度/夸克都要 **Cookie**，光鸭为空。

→ **必须在本机起一个「流代理」**：把 `DirectLink.url + headers` 包装成 `http://127.0.0.1:<port>/s/<token>`，VLC 只访问本地地址。这同时解决：Cookie/Referer/UA、403/过期换链、Range 透传、统一给未来引擎（mpv/GStreamer/外部 mpv）复用。

---

## 4. 播放需求拆解与项目现状（集成点）

### 4.1 要播放的两类源

| 源 | 获取方式 | 关键点 |
|---|---|---|
| **网盘文件** | `Drive::direct_link(path) -> DirectLink` | 现有 `DirectLink { url, headers: Vec<(String,String)>, expire_at, size, file_name }`（`crates/wp-core/src/model.rs:303-313`）已包含播放所需全部信息 |
| **本地文件** | 文件选择 / 拖放 / 路径 | 已有 `rfd 0.15` 依赖与文件选择器；本地直接 `libvlc_media_new_path` |

### 4.2 三盘直链的请求头差异（来自本项目实现）

| 盘 | headers | 证据 |
|---|---|---|
| 百度 | `User-Agent` + `Referer: https://pan.baidu.com/` + `Accept*` + **Cookie** | `crates/wp-drive-baidu/src/download.rs:19-30` |
| 夸克 | 适配器 `download_headers()`（UA/Referer/Cookie） | `crates/wp-drive-quark/src/download.rs:51` |
| 光鸭 | **空 headers**（OSS 签名 URL 自带鉴权） | `crates/wp-drive-guangya/src/download.rs:74` |

→ 代理必须支持「按盘注入不同头」；光鸭可直连但统一走代理更简单。

### 4.3 Range / 流式可行性现状

- 本项目下载器已实现单流断点续传与**分段 Range**：`crates/wp-net/src/download.rs:194`（`Range: bytes=existing-`）、`:264-307`（分段并发，若服务端不支持会直接报错「服务器不支持分片 Range」）。
- 但**分段阈值 32MB**（`crates/wp-core/src/config.rs:100-101`：`segments: 4, segment_threshold: 32MiB`），此前真机下载的百度 20MB / 夸克 12MB / 光鸭 23MB 都在阈值以下 → **走的是单流，不能证明 206/Range 可用**。
- 研究文献里的既有结论只对百度有 206 实测（`docs/research/百度_协议.md` 第 7 条：「下载：302 跟随；无 Cookie 403/31045；**Range 206**；200 从头覆盖」）；夸克 Python 版明确「只接受 200、无 Range（服务端是否支持待确认）」；光鸭未验。
- **结论：Range 支持是 M0 必须实测的第一件事**；它决定「能否无缝 seek」：
  - 三盘都 206 → 代理透传 Range，播放器任意拖动进度条；
  - 部分盘不支持 → v1 对这些盘只保证「顺序播放 + 有限 seek（前进需重新打开链接）」，或先下载到临时文件再播。

### 4.4 GUI 集成点（已定位，后续任务直接落点）

| 集成点 | 位置 |
|---|---|
| 页面枚举（需新增 `Page::Player`） | `crates/wp-gui/src/app.rs:21-29` |
| 双击文件当前行为：目录=进入；文件=详情弹窗 | `open_entry` `app.rs:1030`；触发 `pages/browser.rs:579-581` |
| 播放器需要的事件循环/异步模式 | 已有 `Event` mpsc + `spawn` 模式（工具页/缩略图同款） |
| 封面/播放列表缩略图 | 已有 `crates/wp-gui/src/thumbs.rs` 缓存可复用 |
| 本地文件选择 | `rfd` + 已有文件选择器 |
| 系统打开（兜底外部播放器） | `crates/wp-core/src/paths.rs:156` `open_path` |
| 自定义 GL 绘制（未来 GPU 路径） | `egui::PaintCallback`（`egui/src/lib.rs:458-459`）+ `egui_glow::CallbackFn`（`egui_glow/src/lib.rs:15`、`painter.rs:117`） |

---

## 5. Rust 生态现状（crates 实测）

| crate | 版本/更新 | 许可 | 能力/评价 |
|---|---|---|---|
| `vlc-rs` | 0.3.0 / **2018-06** | MIT | 只封装**音频**回调（`media_player.rs:88-125` 实为 `libvlc_audio_set_callbacks`）；有 `set_xwindow/set_hwnd`；**无 video vmem 回调**；未跟进 VLC4 → 放弃 |
| `libvlc-sys` | 0.2.0 / 2019-12 | MIT | bindgen + pkg-config（`build.rs`，whitelist `.*vlc.*`），绑定全但构建期依赖头文件；可作为对照，不直接用 |
| `libmpv2` | 6.0.0 / **2026-05** | LGPL-2.1（crate 声明） | 有 `render` feature（`src/mpv.rs:14-16`）；控制/属性/命令齐全；配合 mpv render API（`include/mpv/render_gl.h`：`MPV_RENDER_API_TYPE_OPENGL`、`mpv_render_context_create`） |
| `egui-sharkplayer` | 0.5.1 / 2026-07 | Unlicense | **最相关先例**：`eframe::glow` + `libmpv2::render` + FBO + `egui_glow` paint callback（`src/backend.rs:1-3,122-160`），wgpu 后端走 CPU 读回 |
| `egui-video` | 0.9.0 / 2024-10 | MIT | `ffmpeg-the-third` 解码 + `sdl2` 音频 + egui 纹理（`Cargo.toml`/`src/lib.rs`）：只做视频，同步/seek/字幕需自研 |
| `door_player` | 0.3.22 / 2026-09 | Apache-2.0 | ffmpeg + cpal + eframe(wgpu)；纯自研解码路线的另一个先例 |
| `gstreamer`(+video/app) | 0.25.x / 活跃 | MIT（绑定）| 下载量千万级；`appsink` 取帧；Windows 需 GStreamer runtime |
| `ffmpeg-next` | 9.0.0 | WTFPL/LGPL 取决于 FFmpeg 构建 | 底层能力强，但一切自研 |
| `libloading` | 0.9.0 / 2025-11 | ISC | 运行时 dlopen libvlc（不绑死构建环境） |

**mpv 的许可细节**（`include/mpv/client.h:10-18`）：client API 头是 ISC，但 **mpv core 默认 GPLv2+**，只有 `-Dgpl=false` 构建才变成 LGPLv2+。
→ 若走 libmpv：捆绑必须用 LGPL 构建；否则只能「用户自装 libmpv」（发行复杂度上升）。这是把 libmpv 放 Phase 2 的主因。

---

## 6. 候选方案对比

评分：1（差）～5（好）。工作量按 1 人熟悉 Rust/egui 估算。

| 方案 | 跨平台 | 字幕/OSD | 硬解 | 性能 | 许可风险 | 运行时体积 | 工作量 | 成熟度 | 综合 |
|---|---|---|---|---|---|---|---|---|---|
| **A. libVLC3 + vmem CPU（推荐 MVP）** | 5（Win/Linux/Wayland） | 1（vmem 丢 SPU） | 2（可能禁用/回读） | 3 | 低（LGPL 动态链接） | 中（Win 捆绑 ~40–60MB） | **3–5 人日** | 5（VLC 核） | **首选** |
| B. libVLC4 GPU output callbacks | 5 | 4~5（VLC 渲染进纹理） | 5 | 5 | 低 | 中 | 8–15 人日 | 2（未发布） | Phase 2 |
| C. libmpv render（libmpv2 render） | 5 | 5 | 5 | 5 | 中（GPL 核心，需 LGPL 构建） | 中（30–50MB） | 5–8 人日（有先例） | 4 | 备选 |
| D. GStreamer appsink | 4（Win 打包重） | 2（需 overlays 插件链） | 4 | 4 | 低（LGPL） | 大（Win 100MB+） | 6–10 人日 | 4 | 备选 |
| E. 纯 FFmpeg 自研解码（egui-video 路线） | 4 | 1（全自研） | 2 | 3 | 中 | 小 | 15–25 人日 | 2 | 不推荐 v1 |
| F. 外部播放器 handoff | 5 | 5（播放器自带） | 5 | 5 | 无 | 0 | **0.5–1 小时** | 5 | **兜底** |

> 组合策略：**F 作为零依赖兜底 + A 作为内嵌主路线**；B/C 在 M4 评估。

---

## 7. 推荐方案架构（设计稿，未实现）

```
┌────────────────────────────── GUI (wp-gui) ──────────────────────────────┐
│  pages/player.rs  ── 视频区(egui texture / 未来 paint callback)            │
│                     控制条：播放/暂停 进度 时间 音量 倍速 音轨/字幕 全屏     │
└───────────────▲───────────────────────────────────────────┬──────────────┘
                │ PlayerEvent(channel→UI)                   │ 命令
┌───────────────┴───────────────────────────────────────────▼──────────────┐
│ wp-player（新 crate）：PlayerEngine trait + 会话管理 + 进度持久化          │
│   ├─ VlcBackend  ── libloading 动态加载 libvlc（3.x），vmem 帧通道         │
│   ├─ ExternalBackend ── 系统播放器/mpv/vlc CLI（兜底）                    │
│   └─ (预留) MpvBackend / GstBackend                                      │
└───────────────▲───────────────────────────────────────────┬──────────────┘
                │ 帧(Arc<Frame>/RGBA)                        │ URL
┌───────────────┴───────────────────────────────────────────▼──────────────┐
│ StreamProxy（本地 HTTP，tokio）：Range/HEAD 透传 + 注入 headers +          │
│   403/410/过期 → 重新 Drive::direct_link 并续传 Range（热换链）            │
└───────────────▲───────────────────────────────────────────┬──────────────┘
                │ DirectLink{url, headers, expire_at}       │
┌───────────────┴───────────────────────────────────────────▼──────────────┐
│ wp-drive 适配器（百度/夸克/光鸭）—— 已有 direct_link()，无需改动           │
└──────────────────────────────────────────────────────────────────────────┘
```

### 7.1 关键设计点

1. **动态加载**：`libloading` 在运行时尝试 `libvlc.so.5/.so.12`（Linux）/ `libvlc.dll`（Windows，优先 exe 同目录 `libvlc/`）；缺失 → 自动降级外部播放器并提示安装方式。**不把 libvlc 加进编译期链接** → 构建机不需要装 VLC；绿色版/包管理器都灵活。
2. **自研薄 FFI**：约 30 个函数（instance/media/media_player/event/video callbacks/audio volume/track/option），`extern "C"` + `Option<unsafe extern "C" fn>` 声明，避免 stale crate。所有回调 `Send` 到 mpsc，由 UI 线程消费；vmem lock 回调里 memcpy 到 2–3 帧环形缓冲，unlock 标记新帧，UI 用 `ctx.request_repaint()` 跟帧率。
3. **帧上传策略**：MVP = `chroma=RV32` 单平面直接 `TextureHandle::set()`；进阶 = `set_format_callbacks` 协商 I420/NV12 三平面 → 自定义 shader 做 YUV→RGB（省 CPU），或 VLC4 GPU 路径。
4. **流代理**：`127.0.0.1:0` 随机端口 + 启动时生成的 token（只接受带 token 的路径，防本机其它进程盗链）；实现 `GET/HEAD`，解析并透传 `Range/If-Range/Content-Range/Accept-Ranges/Content-Length`；上游 401/403/410 或 `expire_at` 到点 → 调 `Drive::direct_link` 换链后按原 Range 重试（≤3 次）；可选复用限速器（或播放流量独立不限速）。
5. **播放页入口**：浏览器双击视频/音频文件 → 播放页；右键菜单加「播放」（现有菜单在 `browser.rs:571` `context_menu`）；「本地文件」按钮 + 拖放（winit 拖放事件需接入，列为任务）；工具页/传输页也可跳播放。
6. **状态持久化**：播放进度、最近播放列表、音量/倍速、字幕轨道选择 → 写现有 JSON 数据目录（`Paths`）。
7. **错误恢复**：VLC `EncounteredError` / 网络错误 → 刷新直链 + 从当前时间重开一次；仍失败 → 提示「用外部播放器打开」。
8. **v1 明确不做**：字幕渲染（vmem 限制）、窗口嵌入、GPU 渲染、HLS/DASH/DRM、投屏、播放列表高级编辑（先做简单「最近播放/目录顺序播放」）。

### 7.2 备选升级路径（M4 再定）

- 路径 1（推荐优先评估）：**VLC 4 正式发布后**切到 output callbacks OpenGL（复用 eframe glow 上下文，字幕/硬解全有）。
- 路径 2：**libmpv render（LGPL 构建）** 作为可选后端（参考 `egui-sharkplayer`），用配置开关切换；代价是发行两个引擎。
- 路径 3：**X11/Windows 窗口嵌入模式**作为「高性能+字幕」模式（Linux Wayland 自动禁用）。

---

## 8. 风险与开放问题

### 8.1 风险登记（详细缓解见任务计划）

| ID | 风险 | 概率 | 影响 | 摘要缓解 |
|---|---|---|---|---|
| R1 | 三盘直链 Range 不支持/不一致 | 中 | 高（seek） | M0 先用 curl/CLI 实测；不支持则 v1 顺序播放+限制 seek，或临时文件 |
| R2 | VLC3 vmem 无字幕/OSD | 高（既定事实） | 中高（观影体验） | v1 提示限制或外部播放器；M4 上 VLC4/libmpv |
| R3 | Windows 捆绑 libvlc 体积 + 插件 GPL 合规 | 中 | 中 | 首选「不捆绑，提示装 VLC」；捆绑走官方原版+许可/源码链接，法务确认 |
| R4 | 本机测试环境缺运行时 | 高 | 低 | M0 安装 `vlc`/`gst-libav` 即可 |
| R5 | 4K/HEVC CPU 回读性能 | 中 | 中 | M1 做性能基线；必要时提前评估 C/B 路径 |
| R6 | libvlc ABI/soname 差异（5 vs 12） | 低中 | 中 | 动态加载多 soname 尝试 + 版本探测 + 清晰错误 |
| R7 | 播放流量与限速/代理冲突 | 低 | 低 | 代理独立开关；playback 流量默认不限速 |
| R8 | 敏感词守卫/命名规则与播放无关 | 极低 | 无 | 播放不写盘，无需过守卫 |

### 8.2 需要用户拍板的开放问题

1. **运行时策略**：Windows/Linux 上接受「依赖用户安装 VLC」还是「捆绑 libvlc（体积+合规）」？（影响 M3 打包方案）
2. **字幕优先级**：如果 v1 内嵌播放无字幕，能接受吗？还是先只做「外部播放器打开」的高质量模式？
3. **功能优先级**：v1 是否要「播放列表/记住进度/倍速/音轨切换」，还是先做最小可用（播放/暂停/进度/音量/全屏）？
4. **语言/范围**：确认不做投屏、HLS/DASH、DRM、移动端？

---

## 9. 参考证据索引

| 主题 | 证据（仓库路径 / 本项目路径 : 行） |
|---|---|
| libvlc 头 LGPL-2.1+ | `vlc/include/vlc/libvlc_media_player.h:12-20` |
| vmem 性能警告 | `vlc/include/vlc/libvlc_media_player.h:386-412` |
| vmem 回调 API | 同上 `:418,:441,:455`；实现 `vlc/modules/video_output/vmem.c:72,107,245-279` |
| vmem 丢字幕 | `vlc/modules/video_output/vmem.c:263-268,271-279`（`(void) subpic` / `VLC_UNUSED(subpic)`） |
| GPU 输出回调 + 引擎枚举 | `vlc/master:include/vlc/libvlc_media_player.h:1222,1233-1241`；示例 `doc/libvlc/sdl_opengl_player.cpp:82-84,119-196`；`d3d11_player.cpp:757` |
| 窗口嵌入 + X11 警告 | `vlc/include/vlc/libvlc_media_player.h:493,530-543,567`；示例 `doc/libvlc/gtk_player.c:31` |
| 3→4 迁移 | `vlc/master:doc/libvlc/CHANGES-v3-to-v4.md`（回调结构体化/命名 union/微秒单位等） |
| HTTP 无 Cookie 选项 | `vlc/modules/access/http.c:71,182,217,220`；`vlc/include/vlc/libvlc_media.h:505-541` |
| VLC 稳定版 3.0.24 | videolan 官网/`get.videolan.org/vlc/last/` 实测抓取 |
| libmpv 许可 | `mpv/include/mpv/client.h:10-18`（ISC 头；core GPLv2+，`-Dgpl=false` → LGPL） |
| mpv render API | `mpv/include/mpv/render_gl.h:75-76,106-123` |
| vlc-rs 仅音频回调 | `vlc-rs-0.3.0/src/media_player.rs:88-125` |
| libmpv2 render feature | `libmpv2-6.0.0/src/mpv.rs:14-16` |
| egui+libmpv 先例 | `egui-sharkplayer-0.5.1/src/backend.rs:1-3,122-160` |
| Tea Linux 包清单（无播放器/PipeWire/ffmpegthumbs） | `tealinux/packages.x86_64:48-51,121` |
| Tea Linux Rust/Tauri 打包范式 | `tealinux-installer/PKGBUILD:11,16-18`；`tealinux/docs/add-tealinux-modularity.md` |
| 本项目 DirectLink | `网盘管理_V10/crates/wp-core/src/model.rs:303-313` |
| 三盘 headers | `wp-drive-baidu/src/download.rs:19-30`；`wp-drive-quark/src/download.rs:51`；`wp-drive-guangya/src/download.rs:74` |
| Range/分段能力 | `wp-net/src/download.rs:194,264,297,307`；分段阈值 `wp-core/src/config.rs:100-101` |
| GUI 集成点 | `wp-gui/src/app.rs:21-29,1030`；`wp-gui/src/pages/browser.rs:571,579-581`；`wp-core/src/paths.rs:156` |
| egui 自定义绘制 | `egui/src/lib.rs:458-459`；`egui_glow/src/lib.rs:15`、`src/painter.rs:117` |

---

## 10. 结论

1. **技术路线可行**：libVLC 3.x 动态加载 + vmem 回读是最稳的 MVP 路径；「本地流代理」是绕不开的基础设施（VLC 无 Cookie 注入，百度/夸克直链必须带 Cookie）。
2. **项目侧零阻塞**：`DirectLink` 已含 url/headers/expire_at，三盘直链与下载器 Range/分段都已具备；GUI 事件/页面/缩略图体系可复用。
3. **两个必须先验证的实验**：① 三盘直链 Range（决定 seek）；② 本机装好 VLC 运行时后 vmem 收帧/性能基线。这两个实验（M0）通过后再进入实现，避免返工。
4. **体验短板要提前对齐预期**：VLC3 vmem 无字幕/OSD、4K 可能吃 CPU；若字幕/4K 是硬需求，建议 M4 直接评估 VLC4（等正式版）或 libmpv LGPL 后端，而不是在 vmem 上硬扛。
5. **发行策略**：Tea Linux/Arch 走 PKGBUILD `depends=('vlc')`；Windows 绿色版优先「提示安装 VLC + 外部播放器兜底」，捆绑方案需先解决 GPL 插件合规与体积问题。

---

## 附录 A：第二轮补充调研 —— 为什么转向 libmpv（摘要）

> 完整设计与证据见 [PLAYER_ARCHITECTURE.md](PLAYER_ARCHITECTURE.md)；这里只记录第一轮之后新查到的关键事实。

1. **mpv 双许可**：默认 GPLv2+，`meson -Dgpl=false` 可构建 LGPLv2.1+ 版（`LICENSE.GPL`/`LICENSE.LGPL` 并存），
   MIT 应用可动态链接；LGPL 构建只禁用 `x11/dvdnav/dvda/cdda/dvbin/jack/oss-audio/caca/direct3d` 等无关项，
   **保留** gpu-next/libplacebo、libmpv render API、d3d11（Windows）、字幕（libass+位图字幕）、libbluray、硬解。
2. **完整字幕栈在线**：`sub-add`/`sub-reload`/`sub-remove` 命令、`sub-start`/`sub-text`/`track-list` 属性、
   `--secondary-sub-*` 双字幕、`--sub-auto`/`--sub-file-paths` 同名自动加载、`--embeddedfonts`/`--sub-fonts-dir`、
   `--sub-ass-override` 样式覆盖；位图字幕（PGS/DVD/DVB）由 `sub/sd_lavc.c`（LGPL-2.1+ 文件头）配合 FFmpeg 解码，
   由 libplacebo/gpu-next 渲染（含 HDR 字幕峰值控制）。
3. **4K/HDR 渲染**：mpv 0.41（2025-12）默认 gpu-next，Vulkan hwdec 优先；硬件解码 API（VAAPI/NVDEC/Vulkan Video/D3D11VA）
   + render API 的 OpenGL 路径（v0.41 仅 OpenGL/SW）；master（0.42-dev）新增 **D3D11 render API**（`render_d3d11.h`）。
   render API 提供 `ADVANCED_CONTROL`/`BLOCK_FOR_TARGET_TIME`/`GL_FBO`/`report_swap` 等帧同步手段。
4. **蓝光原盘**：`--bluray-device`/`--bluray-angle`；v0.41 `stream_bluray.c` 已有 `bd://menu` 标题与 `BD_EVENT_MENU`，
   但**交互菜单 overlay 渲染（HDMV IG + BD-J ARGB）与 `disc-menu-active` 属性在 0.42-dev**；
   AACS 1.0/BD+ 可由 libaacs/libbdplus（均 LGPL-2.1+，需用户密钥）处理；**AACS 2.x/UHD 加密盘无开源解密方案**。
5. **网络/字幕/AI 注入能力**：mpv 支持 `--http-header-fields`/`--referrer`/`--cookies*`（可直接给网盘链接加头，
   但仍建议走本地流代理做预读缓存与换链）；`sub-add`/`sub-reload` + `sub-text` 观察可让 AI 翻译/ASR 结果作为外部字幕实时注入。
6. **AI 生态**（本地优先）：whisper-rs（whisper.cpp）、candle-core、ort（ONNX Runtime）、llama-cpp-2 均可源码构建；
   建议 AI 推理放独立 worker 进程，避免影响 4K 解码与帧同步。
7. **弹幕**：mpv 的动态字幕注入（`sub-add`/`sub-reload`）与 `--secondary-sub-*` 双字幕轨可承载**预生成的 ASS 弹幕轨**
   （跨 Mode A/B 可用）；交互式弹幕（悬停/搜索/发送）建议用 egui 覆盖层（仅 Mode A），见架构 §6。
