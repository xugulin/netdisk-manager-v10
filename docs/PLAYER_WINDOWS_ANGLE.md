# Windows 内嵌渲染路线（ANGLE + libmpv render API）

> 背景：用户已明确 **不接受原生窗口嵌入（旧 Mode B）**，要求播放器页像普通页面一样——
> 视频画面嵌在应用窗口内、UI/字幕/弹幕/AI 覆盖层都能叠在视频上。
> 因此 Windows 必须走 **Mode A：进程内 GPU 渲染**。本文记录该路线的实现方案与 M0 验证清单。

## 1. 问题本质

| 平台 | Mode A 所需条件 | 现状 |
|---|---|---|
| Linux | eframe/glow 通过 EGL（Wayland/X11 均可）创建 GL/GLES 上下文 → libmpv render API 渲染到 FBO → egui 自定义绘制 | 可行；VAAPI 零拷贝需 EGL dmabuf（M0-E2/E4 实测） |
| **Windows** | eframe 默认用 **WGL** 创建 OpenGL 上下文；mpv 文档要求 `d3d11va` 硬解需 `--gpu-context=d3d11` 或 `angle` → WGL 下 4K 硬解不可用 | 需让 eframe 拿到 **ANGLE 的 EGL/GLES 上下文** |

libmpv render API 只支持 `OPENGL`（GL/GLES）与 `SW`（v0.41；D3D11 render API 是 0.42-dev 新特性）。
EGL/GLES 上下文（ANGLE）是 Windows 内嵌渲染 + D3D11VA 互操作的最现实路径。

## 2. 已落地的补丁（源码可控）

- `vendor/glutin-winit/`：固定 crates.io `glutin-winit 0.5.0`（MIT），只改一处：
  Windows 的 `ApiPreference::FallbackEgl` 从 `WglThenEgl` → **`EglThenWgl`**（优先 ANGLE，无 ANGLE 时自动回退 WGL，行为不变）。
- 根 `Cargo.toml` 通过 `[patch.crates-io]` 指向本地 vendor；Linux 行为完全不变。
- 动机：eframe 固定传 `ApiPreference::FallbackEgl`；若不改，Windows 永远先走 WGL，ANGLE 不会被选中。

## 3. ANGLE 运行时打包（Windows 发行包）

随包目录（与 exe 同级）：

```text
网盘管理.exe
播放器运行时/
  libmpv-2.dll          # 我们 vendored 构建的 LGPL libmpv
  ffmpeg 相关 DLL…       # libmpv 依赖（见构建脚本产物）
  libEGL.dll            # ANGLE（BSD-3-Clause）
  libGLESv2.dll         # ANGLE
  d3dcompiler_47.dll    # 可选（ANGLE/系统缺失时）
  LICENSES/…            # 所有第三方许可与源码获取说明
```

- ANGLE DLL 来源候选（M0 验证其一）：
  1. **mpv 官方 Windows 发行包**自带的 ANGLE（与 libmpv 同源、版本匹配，最稳）；
  2. 自行从 Google ANGLE 源码构建（源码可控但构建成本高）；
  3. Chromium/Edge 附带的 ANGLE（版本不确定性大，不推荐）。
- Windows DLL 搜索顺序：exe 所在目录优先，因此 `播放器运行时/` 需加到进程 DLL 搜索路径或用
  `SetDllDirectoryW`/清单方式（实现时加 `AddDllDirectory`，避免全局污染）。
- 许可：ANGLE 为 BSD-3-Clause，需在 LICENSES 内附许可证与版权声明。

## 4. libmpv 初始化建议（Windows Mode A）

```text
vo=libmpv
hwdec=auto-safe          # M0 验证 d3d11va 是否在 ANGLE 上下文下生效；失败记录降级
gpu-api=opengl           # ANGLE 提供 GLES；render API 使用 OPENGL 类型
gpu-context=angle        # 仅供 mpv 内部路径参考；render API 由 app 提供上下文
video-sync=display-resample
target-colorspace-hint=auto   # HDR：M0 记录 GL/ANGLE 下是否可直通，不可则 tone-map 到 SDR
```

RenderBridge 由 `wp-media-mpv` 暴露：用 eframe 当前上下文创建 `mpv_render_context`（`MPV_RENDER_PARAM_OPENGL_INIT_PARAMS` + `get_proc_address`），渲染到 FBO →
egui 侧用 `egui_glow` paint callback 输出到页面视频矩形；`mpv_render_context_report_swap` 用于帧同步。

## 5. M0-Win 验证清单（需要在 Windows 机器上跑；Mac 不可替代）

| # | 验证项 | 通过标准 |
|---|---|---|
| W1 | 启动后 GL 上下文类型 | 日志/诊断显示 `EGL`/`ANGLE`（GL_RENDERER 含 ANGLE） |
| W2 | libmpv render API 出帧 | 本地 1080p/4K 视频画面正常，UI 可覆盖 |
| W3 | 硬解 | `hwdec-current` = `d3d11va`（或 `nvdec`），无回读 |
| W4 | 4K HEVC 10bit 性能 | 丢帧 < 0.5%，CPU 单核 < 40% |
| W5 | HDR | 记录 `target-colorspace-hint` 表现（直通/降级 tone-map），写入文档 |
| W6 | 无 ANGLE 环境 | 删除 ANGLE DLL 后能回退 WGL 启动（软件解码/提示），不崩溃 |
| W7 | 打包完整性 | 发行包在干净 Windows 10/11 可启动并播放 |

> 若 W3/W4 不达标：Fallback 顺序 = ① 换 ANGLE/mpv 版本重试；② 4K 场景提示「用外部播放器」；
> ③ 评估 mpv 0.42 D3D11 render API + wgpu 合成（另立 Phase 2，不改当前计划）。

## 6. 风险

- ANGLE 与 WGL 混用/驱动兼容性（Intel/NVIDIA/AMD 分别验证）；
- GLES 下 egui 渲染正确性（eframe/egui_glow 官方支持 GLES，问题概率低）；
- mpv 在 ANGLE 上下文下的 d3d11va 互操作需实测（W3 是关键 gate）；
- HDR 直通在 GL/ANGLE 上可能不可用（可通过 Windows 原生 HDR 路径后续优化）。
