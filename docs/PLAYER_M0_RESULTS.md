# 播放器 M0 技术验证报告（E1–E5）

> 日期：2026-10-19 · 负责人：media-core（M1 媒体内核）· 状态：**主体完成（E1–E5 全部有结论），真机 GPU 零拷贝/4K60 指标待补**
> 关联：`docs/PLAYER_ARCHITECTURE.md`（v2 架构）、`docs/PLAYER_TASKS.md`（M0 验收门槛）
> 代码：`crates/wp-media-mpv/**`、`scripts/build-media-stack-linux.sh`、`vendor/prebuilt/linux-x86_64/`
> 实验原始数据：`/tmp/wp-m0/`（JSON/PPM 日志）；关键截图已拷到 `出/验收截图/媒体内核-M0/`

---

## 0. 结论先行

| 实验 | 内容 | 结论 |
|---|---|---|
| **E1** | 源码构建 LGPL libmpv v0.41.0 | ✅ **通过**。脚本一键构建，产物 3.3 MB / client API 2.5 / 依赖全动态解析；LGPL-2.1+ 许可证随包 |
| **E2** | Mode A：进程内 GL render API + eframe/egui 合成 | ✅ **链路打通**（render context → FBO → egui 同帧合成 → update callback/report_swap 全部工作，`gl_error=0`，方向校准正确）。性能数字受 Xvfb/llvmpipe **软件 GL** 限制，不代表真机；真机 GPU 指标待 GUI 桌面复测 |
| **E3** | ASS 弹幕轨当副字幕轨 + 主字幕叠加 | ✅ **可行**。主/副字幕同屏渲染，`secondary-sub-text` 事件正常（密度样本 52 条 cue）；4K+1000 条/分钟弹幕在软件 GL 下丢帧明显 → 真机复测；弹幕主路径仍建议 egui 覆盖层 |
| **E4** | hwdec 探测（Intel Iris Xe，headless） | ✅ **VAAPI 可用（copy-back）**：装 iHD 驱动后 `auto-safe/auto` → `hwdec-current=vaapi-copy`，4K HEVC 10bit CPU **0.76→0.27 核**（软解→硬解+回拷）；`hwdec=vaapi` 零拷贝在 headless 仍失败（需 EGL/DRM，真机桌面待测） |
| **E5** | HLS（网盘云端转码）播放 | ✅ **通过**。本地 HLS 取证：**m3u8 与每个 .ts 分片都带 Cookie/Referer/UA/X-WP-Test**；夸克真机 HLS（98.764s/810x1440）系统 mpv 与**自建 LGPL libmpv** 均可播放、可 seek(60s)、无 Cookie 时报可恢复错误 |

**环境**：Arch 系 Linux（内核 7.2.8）x86_64 · Intel i7-1195G7（8 线程）· Intel Iris Xe（`/dev/dri/card1`+`renderD128`）· 15 GB RAM · 无物理 DISPLAY（Xvfb `1280x800x24`，Mesa **llvmpipe 软件 GL**）
**工具链**：rustc 1.98.1 · meson 1.12.1 · ninja 1.13.2 · FFmpeg n9.0.2（dev 头齐全）· libass 0.17.5 · libplacebo 7.360.1 · libbluray 1.5.1 · 系统 mpv 0.41.0（`libmpv.so.2.5.0`）

---

## E1. LGPL libmpv 源码构建 ✅

**命令**（可复现，一条）：

```bash
cd 网盘管理_V10
./scripts/build-media-stack-linux.sh            # 默认 mpv v0.41.0；--clean 强制重建
```

脚本流程：检查依赖（缺 meson 自动 `pacman -S`）→ 下载 mpv v0.41.0 tarball（基座下载/gh-proxy/GitHub 三级回退）
→ `meson setup --buildtype=release -Dgpl=false -Dlibmpv=true -Dcplayer=false -Dbuild-date=false -Dtests=false -Dmanpage-build=disabled`
→ `ninja` → `DESTDIR` 安装 → 拷贝到 `vendor/prebuilt/linux-x86_64/` → 生成 BUILDINFO + ctypes 自检。

**实测数据**：

| 项 | 值 |
|---|---|
| 版本 | mpv **v0.41.0**，client API **2.5** |
| 产物 | `libmpv.so.2.5.0`，3,473,520 B（3.3 MB），SONAME `libmpv.so.2` |
| sha256 | `b248040ff54bf2e06fed8c639f04f7d8d839c593f9272dc146d9f5197896eab4` |
| 构建耗时 | 全量 **≈2.5–3 分钟**（8 核；含下载/解压/编译/安装）；增量重建 < 1 分钟 |
| 功能开关（meson 摘要） | `libmpv: YES` `opengl: YES` `drm: YES` `wayland: YES` `javascript: YES` `lua: YES`；`gpl: NO`、`cplayer: NO`、`x11: NO`、`vulkan: NO`（详见 §E4 说明） |
| 依赖 | 全部**动态链接系统库**（ldd 解析 30+ 项：libass/libav*/libplacebo/libbluray/libva/libX11… 均在 `/usr/lib`） |
| 许可证 | mpv 核心 **LGPL-2.1-or-later**；`vendor/.../licenses/{LICENSE.LGPL,LICENSE.GPL,Copyright}` 已随包 |
| 复现性 | 两次从零构建均成功（探索构建 + 脚本构建），产物 client API 一致；`BUILDINFO.txt` 记录 sha/ldd/构建参数 |

**产物布局**：`vendor/prebuilt/linux-x86_64/{lib/,include/mpv/,licenses/,BUILDINFO.txt}`；二进制不进 git（`vendor/.gitignore`），头文件/许可证/BUILDINFO 进 git。
**运行期加载顺序**（`load_library`）：显式 `MpvConfig.lib_path` > `WP_MPV_LIB` > 系统 `libmpv.so.2` > 可执行文件同级 `播放器运行时/`。

> 许可证注意：本构建 `-Dgpl=false` 是 LGPL；依赖当前用系统 FFmpeg（部分发行版为 GPL 构建）。**正式发行前**需换 LGPL FFmpeg 并随包声明（已列 M6 发行合规；见 `vendor/README.md`）。

---

## E2. Mode A：libmpv render API + eframe/egui 收帧

**实现**（`crates/wp-media-mpv/src/render.rs`）：
mpv `MPV_RENDER_API_TYPE_OPENGL` + `MPV_RENDER_PARAM_ADVANCED_CONTROL` → 渲染到自管 FBO/纹理（尺寸=视频矩形物理像素）
→ 在 `egui_glow::CallbackFn`（GL 上下文 current）内用着色器把纹理铺满视频矩形 → `report_swap()`。
新帧由 mpv update callback 通知 → `egui::Context::request_repaint()`（无新帧不重绘）。

**测试命令**（headless + 软件 GL）：

```bash
Xvfb :99 -screen 0 1280x800x24 &
DISPLAY=:99 cargo run -p wp-media-mpv --example render_probe -- \
   /tmp/wp-m0/media/h264_1080p_30s.mp4 --seconds 6 --size 640x360 --hwdec no \
   --screenshot /tmp/wp-m0/e2/e2_1080p_640x360.ppm --json
```

| 用例 | 视频 | 窗口 | 状态 | video_size | 渲染帧/复用帧 | 平均出帧 | CPU(核均值) | 丢帧 | GL 错误 |
|---|---|---|---|---|---|---|---|---|---|
| E2-1 | 1080p H.264 30s | 640x360 | Playing | 1920x1080 | **26 / 3** | 4.13 fps | **1.88** | 40 | **0** |
| E2-2 | **4K HEVC 10bit** 10s | 640x360 | Playing | 3840x2160 | 6 / 15 | 0.87 fps | **2.35** | 35 | **0** |
| E2-3 | 1080p 方向测试片（上半红/下半蓝） | 480x270 | Playing | 1920x1080 | 23 / – | 2.9 fps | ~1.5 | – | 0 |

**验收点实测**：

1. **帧真的贴进 egui**：`render_probe` 在 `on_exit` 从 FBO `glReadPixels` 写 PPM —— 1080p 画面 100% 非黑，平均像素 ≈127（testsrc2 彩条图案）；`first_frame=true`、`last_update_flags=0x1 (MPV_RENDER_UPDATE_FRAME)`、`GL error=0`。
2. **UI 与视频同帧合成**：在视频绘制之后画一个红色半透明 `UI OVERLAY` 角标，截图像素校验红块命中 **39.9%**（视为覆盖成功）→ UI 可叠在视频上（控制条/字幕/弹幕/AI HUD 可用）。
3. **方向正确**：红顶/蓝底测试片，PPM 上三分之一 `(191,18,0)=红`、下三分之一 `(0,11,191)=蓝` → **正向**（曾发现上下颠倒，已用 `MPV_RENDER_PARAM_FLIP_Y=1` 校准，见 §坑 3）。
4. **首帧延迟**：`open()→FirstFrame` ≈ **3.78–4.18 s**（含渲染桥就绪 + llvmpipe 首次渲染）；真实 GPU 会低一个数量级，**需 GUI 桌面复测**。
5. **结论**：Mode A 架构在 Linux/GL 上成立；**当前数字是软件 GL 下限**（llvmpipe 单/少线程软渲染 1080p→RGBA 缩放），不能用于判定 4K 流畅指标。真机 GPU（Iris Xe + EGL）预期远好，列入下一轮（见 §7）。

**本轮踩到的 3 个关键坑（已修复，GUI 可直接用）**：

1. **`vo=libmpv` 必须在媒体加载前创建好 render context**，否则 mpv 会「无声无画」起播（只走音频、`video-params` 为空，update 永远不返回新帧）。→ 引擎新增「open 排队」：Mode A 下 open 若早于渲染桥就绪，会等 render context 创建后自动执行，**对 GUI 透明**。
2. **不能在首帧前设置 `video-sync=display-resample`**（依赖 `report_swap` 的显示时间戳，会卡住视频初始化）→ 已从默认选项移除，留 T6 帧同步调优时动态启用。
3. **FBO 方向**：mpv 默认输出「行 0=画面顶部」，而 GL 纹理采样 v=0 在底部 → 上下颠倒；置 `MPV_RENDER_PARAM_FLIP_Y=1` 修复（已像素级验证）。

---

### E2-4 GUI 真实播放联调（合成位置 bug 修复 + 像素回归）

gui-player 接入后首次真实播放发现「视频尺寸正确但整体偏移、盖住侧栏」。根因：`mpv_render_context_render()` 返回后 **GL 状态未定义**（framebuffer/viewport/scissor 被 mpv 改掉），原实现 render 后直接 composite。
修复：render 后先 `restore_gl_state(saved)` 再 composite，最后再恢复一次。像素级回归（读**窗口默认 framebuffer**，等价 GUI 截图，检测彩色区域 bbox）：

| 用例 | 期望 | 实测 | 结论 |
|---|---|---|---|
| 1280x800 窗口，视频矩形 `--rect 158,152,780,438`（模拟 GUI 16:9 inner rect） | x=[158,938] y=[152,590] | **彩色 bbox x=[160,936] y=[152,589]** | ✅ 位置/尺寸正确 |
| 视频矩形外围抽点（左侧栏/右侧/下方） | egui 背景色 | `(30,30,36)` | ✅ 不再越界覆盖侧栏 |

同时 `gl_error=0`、方向正确（红顶/蓝底片像素校验）、UI 覆盖层与视频同帧合成（红角标像素命中 39.9%）。

## E3. ASS 弹幕轨当「副字幕轨」+ 主字幕叠加

**夹具**（来自 subtitle-danmaku 的 M0 交付，已在工作区）：

- `出/验收截图/弹幕/m0-density-1000pm.ass`：1000 条/分钟、峰值同屏 129 条（122,948 B）
- `出/验收截图/弹幕/m0-5000.ass`：5000 条 4K 弹幕、峰值同屏 68 条（613,588 B）
- 自研 `primary.ass`：主字幕 30 句（底部居中、带描边样式）

**方法**（引擎 API，GUI 将来同样调用）：

```rust
// ① 主字幕（select=true → sid=1）
engine.command(PlayerCommand::AddSubtitleFile { path: primary_ass, select: true });
// ② 弹幕 ASS 作为第二条字幕轨（select=false）
engine.command(PlayerCommand::AddSubtitleFile { path: danmaku_ass, select: false });
// ③ 指定第 2 条字幕轨为「副字幕」并显示（wp_media::PlayerCommand 尚无此能力，
//    先用 set_property 逃生门；T8 完整字幕时再进正式命令）
engine.set_property("secondary-sid", "2");
engine.set_property("secondary-sub-visibility", "yes");
```

**实测**（4K HEVC 10bit，640x360 窗口，Xvfb 软件 GL）：

| 用例 | 弹幕密度 | 状态 | 渲染帧 | 平均出帧 | CPU(核) | 丢帧 | 副字幕 cue 事件 |
|---|---|---|---|---|---|---|---|
| E3-1 | 1000 条/分钟（同屏峰值 129） | Playing | 10 | 1.05 fps | **3.27** | 89 | **52 条**（样例 `"细节拉满"` 等） |
| E3-2 | 5000 条（同屏峰值 68） | Playing | 8 | 0.83 fps | 2.85 | 68 | **28 条** |

**结论**：

- ✅ **主+副字幕双轨叠加可行**：libass 把主字幕与弹幕 ASS 同时渲染进同一视频帧；引擎 `secondary-sub-text/secondary-sub-start` 属性观察正常（副字幕 cue 事件按时间轴上报，可供 AI/弹幕对齐）；无 GL 错误。
- ⚠️ **性能**：4K 视频 + 千条/分钟弹幕在 **llvmpipe 软件渲染**下 CPU 3.27 核、丢帧 89/9.5s——这是 CPU 软渲染瓶颈（libass 单线程 + 软件 GL 缩放），**不代表真机**；但「4K 高密度弹幕不能只靠 ASS 轨」的结论成立。
- **建议**（与架构一致）：弹幕**主路径 = egui 覆盖层**（`wp-danmaku` 车道引擎 + 我们 `paint_video` 之上绘制，零 libass 负担）；**ASS 弹幕轨作为跨引擎/兜底路径**，并配合**密度上限/分片 sub-reload**。
- 未做：真机 GPU 复测、百万级弹幕压测、主/副字幕层级（`sub-ass-override`/字体）矩阵（E5 字幕矩阵，属 M2）。

---

## E4. hwdec 探测（Intel Iris Xe，headless）✅ VAAPI copy-back 可用

**驱动状态**（本轮由 Lead 安装后复测）：`intel-media-driver 26.3.5`（iHD）+ `libva-utils` + `vulkan-intel` + `mesa-utils`；
`/usr/lib/dri/iHD_drv_video.so` 存在，`vainfo`：Intel iHD driver 26.3.5 / VA-API 1.24，profile/entrypoint 正常。

**矩阵**（4K HEVC 10bit 测试片 10s，`vo=null` headless；system=系统 mpv 0.41.0，vendor=本次自建 LGPL libmpv）：

| libmpv | hwdec 请求 | 状态 | `hwdec-current` | CPU(核均值) | 首帧(open→) | 说明 |
|---|---|---|---|---|---|---|
| system | `no`（软解基准） | Ended | no | **0.76** | 150 ms | 4K HEVC 10bit 软解也能实时（8 线程） |
| system | **`auto-safe`** | Playing | **vaapi-copy** | **0.27** | 4398 ms | 自动选 copy-back，CPU −65% |
| vendor | `auto-safe` | Playing | **vaapi-copy** | **0.33** | 4397 ms | 自建 LGPL 构建同样可用 |
| system | **`vaapi-copy` 显式** | Playing | **vaapi-copy** | **0.61** | **235 ms** | headless 推荐显式 `vaapi-copy`（避开 auto 探测延迟） |
| system | `vaapi`（零拷贝） | 回退软解 | **no** | 0.97 | 160 ms | mpv 日志 `Could not create device` → 回退 |
| vendor | `vaapi`（零拷贝） | 回退软解 | no | 1.45 | 410 ms | 同上 |

**Mode A（render API + Xvfb/llvmpipe）**：`--hwdec auto-safe` 时引擎快照 `hwdec=vaapi-copy` → **VAAPI 解码 + 回拷进入 GL 渲染链路成立**；4K 在 llvmpipe 下只出 1–8 帧属软件 GL 瓶颈（见 E2），不是硬解问题。

**结论**：
1. **VAAPI 硬件解码在本机 headless 可用（copy-back 路径）**：4K HEVC 10bit CPU **0.76 → 0.27 核（≈2.8×）**，无丢帧。
2. `hwdec=vaapi`（**零拷贝** dmabuf/EGL interop）在 headless **不可用**：无 DRM master / 无 EGL 显示上下文 → mpv `Could not create device` → 自动回退软解。
3. `auto-safe/auto` 的探测回退链导致**首帧慢 ~4.4s**（依次尝试 vulkan/vaapi direct 失败）；自动化/headless 场景建议显式 `hwdec=vaapi-copy`（首帧 235ms）。GUI 可在拿到 `hwdec-current` 后按环境自适应。
4. 真机桌面（X11/Wayland + EGL + DRM）的**零拷贝 VAAPI + 4K60 丢帧**指标仍需 GUI 桌面会话复测（下一轮）。

**排查记录**：驱动未装时 `/usr/lib/dri` 无 `iHD/i965`，libva 无法建 device → 全部回退软解（`hwdec-current=no`）；装 iHD 后上表成立。
另：自建 LGPL 构建 meson 检测 `vulkan: NO`（缺 Vulkan 开发环境），故本构建无 Vulkan Video 路径；非当前阻塞项，后续可补 `vulkan-headers` 重构建。

## E5. HLS（网盘云端转码 m3u8）播放 ✅

### E5-1 本地 HLS 取证：请求头是否透传到「分片」

**动机**：夸克大视频（>100MB）走云端转码 HLS，m3u8 与 `.ts` 分片都需 Cookie/Referer/UA（否则 CDN 412）。
mpv 侧实现链路（读源码确认）：`--http-header-fields` → `stream/stream_lavf.c::mp_setup_av_network_options()`
→ 对 HLS（ffmpeg hls demuxer，`no_stream` 分支，`demux/demux_lavf.c`）以 avformat `headers` 选项透传 → **分片请求同样携带**。

**做法**：ffmpeg 生成 4 段 HLS + 本地 HTTP 服务器（记录每个请求的 UA/Referer/Cookie/X-WP-Test/Range），
引擎以 `NetworkSource { url, headers }` 打开，headers 用 **MPV_FORMAT_NODE_ARRAY 逐项**设置（防逗号拆碎）。

**结果**（`/tmp/wp-m0/hls_requests.log`）：

```text
m3u8 请求 2 条，分片请求 4 条
m3u8 首条 headers: ua=WP-Test-Agent/1.0, referer=https://pan.quark.cn/, cookie=wp_cookie=abc123, x_wp_test=media-core
分片 首条 headers: ua=WP-Test-Agent/1.0, referer=https://pan.quark.cn/, cookie=wp_cookie=abc123, x_wp_test=media-core
分片带 Cookie: 4/4   分片带 X-WP-Test: 4/4   Range 请求: 6
```

播放：1080p HLS、duration 30.0s、6s 内推进到 5.82s、丢帧 0、CPU 0.13 核、FirstFrame 正常 → ✅ **m3u8+分片全带自定义头，HLS 可播**。

### E5-2 夸克真机 HLS（样本由 quark-stream 提供）

样本：`/tmp/wp-m0/quark_hls_sample.json`（173 MB 视频云端转码，`{url, headers[Cookie(720B), Referer, User-Agent]}`，auth_key 3h 过期）。
命令示例：

```bash
cargo build -p wp-media-mpv --bin mpv-probe
mpv-probe "$URL" --header "Cookie: $CK" --header "Referer: https://pan.quark.cn/" \
  --header "User-Agent: $UA" --seconds 10 --hwdec no --json
```

| 场景 | 结果 |
|---|---|
| 系统 libmpv 0.41 | ✅ Playing，**duration 98.764s**，video **810x1440**（竖屏），tracks 2；open→Opened **3114 ms**，open→首帧 **3139 ms**；CPU 0.23 核 |
| **自建 LGPL libmpv**（`WP_MPV_LIB=vendor/...`） | ✅ Playing，duration 98.764s，810x1440；open→Opened 2392 ms，首帧 2463 ms |
| seek 60s（系统） | ✅ 跳转到 61.97s 正常出画，丢帧 0 |
| **负向：不带 Cookie** | ✅ 按预期失败：`PlayerEvent::Error{recoverable:true, "网络流错误（链接可能已过期…）: Failed to open …m3u8"} + END_FILE error` → UI 可提示「链接过期」 |

> 交叉验证（quark-stream 真机数据）：m3u8/分片无 Cookie → **HTTP 412**；带完整 Cookie → 200，22 个分片、VOD、`#EXT-X-TARGETDURATION:5`，分片 Range → 206；ffprobe `format_name=hls, duration=98.764`；mpv 日志 m3u8→识别 hls→请求首个 .ts→解码 ≈ **1.8s 量级**（与本报告 open→首帧 2.4–3.1s 同量级，差异含网络抖动与本地探测开销）。

**结论**：HLS 直连路径 **可播、可 seek、headers 透传分片、失败可上报**；NetworkSource 不需要 `is_hls` 字段（mpv 靠 URL/内容嗅探），调用方用 `is_hls` 决定跳过 wp-stream 代理即可。

---

## 2/3/4/5 之外：接口与测试现状（供 GUI 对接）

- **公开 API**（`wp-media-mpv`，`cargo check/test` 0 警告）：
  - `MpvConfig` / `MpvEngine::new(MpvConfig) -> Result<MpvEngine>` / `impl wp_media::MediaEngine`（签名冻结不变）
  - `MpvEngine::{mpv_version, last_error, render_source, set_property}`（`set_property` 为逃生门：双字幕 `secondary-sid`、调参）
  - `render::MpvRenderContext::{new, paint_video, paint_rect, paint_with_painter, ensure_init, is_ready, render, render_offscreen, update_flags, report_swap, has_new_frame, set_update_callback, texture, read_pixels, stats, last_error, destroy}`
  - 辅助程序：`src/bin/mpv-probe`（headless 播放/HLS/hwdec 探针，JSON 输出）、`examples/render_probe.rs`（Mode A 真渲染 + 截图 + seek + 字幕注入）
- **测试**：`cargo test --workspace` = **25 个测试套件 / 271 passed / 0 failed**（wp-media-mpv 占 30 单元测试 + 2 doctest + 1 ignored 真实播放集成测试，`--ignored` 手动跑亦通过）；`cargo check --workspace --all-targets` = **0 错误 / 0 警告**。
- **GUI 集成契约**（已消息 gui-player）：eframe 0.37 入口是 `fn ui(&mut self, ui, frame)`（不是旧 `update`）；`App::on_exit` 必须先 `render.destroy(gl)` 再 `engine.shutdown()`；open 早于首帧绘制也不会丢视频（引擎自动排队）。

---

## 7. 未完成 / 下一轮建议（明确列出，不含糊）

1. **真机 GPU 的 E2/E3 复测**（本机无物理 DISPLAY）：X11/Wayland 桌面 + Iris Xe + EGL 下测 1080p/4K 出帧率、首帧、CPU/GPU、UI 同帧合成、VAAPI 零拷贝（需 GUI 端跑或用户桌面会话）。
2. **真机桌面零拷贝 VAAPI**：headless 已验证 vaapi-copy（CPU 0.27 核）；零拷贝 `hwdec=vaapi` 需 EGL/DRM 桌面环境，补 4K60 10bit HEVC 丢帧 <0.5% 与 GPU 占用指标（GUI 桌面会话跑）。
3. **蓝光 ISO/BDMV 主标题**（E6）：需未加密原盘/ISO 样本；`bluray-device` + `bd://` 路径已实现，`SelectDiscTitle` 走 `bd://<n>`，标题枚举（libbluray DiscInfo）尚未实现（disc 快照仍为 None）。
4. **T6 帧同步调优**：拿到首帧后动态启用 `video-sync=display-resample`、`BLOCK_FOR_TARGET_TIME`、repaint 节流（需真机 vsync 数据）。
5. **发行合规**：LGPL FFmpeg 随包（当前动态链接系统 FFmpeg）；Windows ANGLE + libmpv-2.dll 构建/实测（脚本骨架已给）。
6. 弹幕百万级/4K 真机压测，确定 ASS 轨 vs egui 覆盖层的最终阈值与降级策略。

---

## 附录 A：实验产物与复现命令

| 产物 | 路径 |
|---|---|
| E1 构建脚本 | `scripts/build-media-stack-linux.sh`（Windows 骨架 `scripts/build-media-stack-windows.ps1`） |
| E1 产物/许可证/构建信息 | `vendor/prebuilt/linux-x86_64/{lib,include,licenses,BUILDINFO.txt}` |
| headless 探针 | `cargo run -p wp-media-mpv --bin mpv-probe -- <file|url> [--header 'K: V'] [--sub path] [--prop k=v] [--seek N] [--json]` |
| Mode A 渲染探针 | `DISPLAY=:99 cargo run -p wp-media-mpv --example render_probe -- <video> --seconds 6 --size 640x360 --hwdec no --screenshot out.ppm --json` |
| 测试/检查 | `cargo fmt -p wp-media-mpv && cargo check -p wp-media-mpv --all-targets && cargo test -p wp-media-mpv`；集成测试 `cargo test -p wp-media-mpv --lib -- --ignored --nocapture` |
| 原始实验数据 | `/tmp/wp-m0/{e2,e3,e4,hls}/`（JSON + PPM；关键截图转 PNG 存 `出/验收截图/媒体内核-M0/`） |

测试视频（ffmpeg 生成，不入 git）：1080p H.264 30s、4K HEVC 10bit 10s、方向测试片（上红下蓝）；HLS 由 1080p 转 4 段。
