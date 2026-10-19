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

- **ANGLE DLL 来源（已固定，`scripts/fetch-angle.ps1` 自动获取+校验）**：
  * 来源：**ANGLE prebuilt**（Chrome stable 同源构建）<https://github.com/dsafdsaf132/angle-prebuilt>
  * 固定发布：`angle-stable-chrome-155.0.8059.80-cccf60c`（2026-10-14）
    → `angle-cccf60c-win32-x64.zip`
  * 归档 sha256：`2f13aee2…20ccbf`；`libEGL.dll`/`libGLESv2.dll`/`d3dcompiler_47.dll`
    的 sha256 + 大小 + PE x86-64 校验都在脚本里 pin 死，不匹配即失败并提示更新 pin；
  * ⚠️ 2026-10 实测：**mpv 官方 Windows 发行包不含 ANGLE**（只有 d3dcompiler_43.dll），
    因此不能再用「从 mpv 包取 ANGLE」的方案；
  * 备选（离线/受限环境）：自行从 Google ANGLE 源码构建（BSD-3，成本高），或从 Qt/Chromium
    的 ANGLE 构建提取（版本不确定性大，不推荐；脚本支持 `-Archive` 传入离线 zip）。
  * 获取命令：`powershell -ExecutionPolicy Bypass -File .\scripts\fetch-angle.ps1`
    （`-PrintOnly` 看 pin；`-Archive` 离线；`-WithHeaders` 附带 EGL/GLES 头文件）
- Windows DLL 搜索顺序：exe 所在目录优先，因此 `播放器运行时/` 需加到进程 DLL 搜索路径
  （应用侧 `AddDllDirectory`，避免全局污染；见 `scripts/package-player-runtime-windows.ps1`
  生成的 `LOAD-ORDER.txt`）。
- 许可：ANGLE = **BSD-3-Clause**，zip 内 LICENSE 会随脚本落到 `ANGLE-LICENSE.txt`；
  发行包 `LICENSES/ANGLE-BSD-3-Clause.txt` 来自 `assets/licenses/`。
  ⚠️ 该预编译包由 Chrome stable 构建链产出，我们**只取 ANGLE 的 BSD-3 DLL**，不提取/不复用
  mpv、Chrome 等其它许可组件，也不把归档提交进仓库。

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

## 4.5 CI 实测记录（GitHub Actions Windows runner，2026-10-19）

本仓库没有 Windows 机器，Windows 侧实测全部通过 **GitHub Actions `windows-latest`** 完成。

### 4.5.1 快速预检 workflow `windows-smoke`（run 39197504582，全部 success）

| 检查 | 结果 |
|---|---|
| MSYS2 工具链诊断（cc/gcc/meson/avrt） | ✅ `libavrt.a` 位于 **`/ucrt64/lib/`**（不在 `x86_64-w64-mingw32/lib/`）；`cc t.c -lavrt` 链接 OK |
| **meson 中文构建路径对照实验** | ✅ 同一最小 meson 项目：ASCII 路径 → `MESON_AVRT_OK`；中文路径 → `MESON_SETUP_FAIL`，**证实 meson 在 Windows 上不能用于中文构建目录** |
| PowerShell 4 脚本语法校验 | ✅ `build-media-stack-windows.ps1` / `package-player-runtime-windows.ps1` / `fetch-angle.ps1` / `windows-verify.ps1` |
| `cargo build --release -p wp-gui -p wp-cli`（Windows MSVC） | ✅ `网盘管理.exe` 19,870,208 B、`wp-cli.exe` 5,718,016 B |
| `wp-cli --help` | ✅ 版本/usage 正常（中文正常） |
| `package-player-runtime-windows.ps1 -Generate` 结构自检 | ✅ LICENSES（10 份）/LOAD-ORDER/DEPENDENCIES/BUILDINFO/README 生成；无 media-stack 时可读错误（预期） |

### 4.5.2 重型 workflow `player-windows` 迭代记录（同日）

| run | 结果 | 结论 |
|---|---|---|
| 39170354614 | ❌ | mpv meson 报 `find_library('avrt')` 失败（初判为工具链问题） |
| 39173284183 | ❌ | 日志定位根因：构建目录含中文 + 用了 runner 预装的 `C:\msys64` 而非 setup-msys2 安装的 MSYS2；meson 编译器探测全部返回 NO |
| 39195656980（smoke） | ✅ | 加入 MSYS2 诊断/中文-ASCII meson 对照，根因锁定 |
| 39197502500 | job1 ✅ / job2 ❌ | 修复 #4 生效：**media-stack job 全绿**（LGPL FFmpeg + libmpv-2.dll 构建、缓存保存、artifact 上传）；job2 Strict 校验发现 prebuilt 缺 FFmpeg `av*.dll` → 修复 #5 |
| 39200091084 | job1 ✅ / job2 ❌ | 修复 #5 生效：运行时 DLL 齐全、LICENSES 10 份、**Strict Errors=0**；job2 在 windows-verify 步骤因 PowerShell 数组 splat 误用失败 → 修复 #6（改哈希表 splat） |
| 39202348549 | job1 ✅ / job2 ❌ | 修复 #6 生效：windows-verify 真正跑起来，W7a–d 全 PASS；W3/W4 因 `-SkipBuild` 未构建 `mpv-probe` 被判 FAIL → 修复 #7（CI 语义：探针缺失时 W3/W4 SKIP，不算失败） |
| **39204754721** | **✅ 全绿** | **Windows 发行包产出**：job1 构建 `libmpv-2.dll`（sha256 `58214a9e…`，3,180,620 B，prebuilt 依赖 DLL 20 个）；job2 编译/组装/运行时 Strict 校验/验收/打包/上传**全部 success**；artifact `windows-player-runtime` 42,752,825 B（内含 `网盘管理-win64.zip` 96,096,567 B / 80 文件 + `windows-verify-report.json`）；验收报告 **PASS=4 / FAIL=0 / SKIP=6** |

**修复 #4（media-stack 构建环境）**
1. **构建目录必须纯 ASCII**：`出\构建缓存` 含中文 → meson 的编译器探测/链接测试用绝对路径，
   MinGW gcc 经 ANSI argv 打不开 → 所有 `-W` 探测返回 NO、`find_library('avrt')` 失败。
   已改为 `build\windows-media-stack`；脚本对非 ASCII 路径自动回退 `%TEMP%\wp-media-stack`。
2. **不要写死 `C:\msys64`**：`msys2/setup-msys2` 实际装到 `D:\a\_temp\msys64`。Workflow 新增
   「记录 MSYS2 安装位置」一步（MSYS2 shell 里 `cygpath -w /` 写入 `msys2-root.txt`），pwsh 构建步骤读它。
3. **先跑 20 秒预检**：`build-media-stack-windows.ps1 -Preflight`（cc / `-lavrt` / 最小 meson 项目
   `find_library('avrt')`），失败即止，避免白跑约 30 分钟 FFmpeg 构建。

**修复 #5（Windows DLL 收集/校验）** stage 步骤显式拷贝 `build-prefix/ffmpeg/bin/*.dll`
（Windows 版 FFmpeg DLL 名**没有 lib 前缀**：`avcodec-62.dll` / `avformat-62.dll` / `swresample-6.dll` …），
ldd 依赖收集扩展到 `__PREFIX__` 目录；打包校验通配符改 `*avcodec-*.dll` 形式以同时兼容两种命名；
`libwinpthread-1.dll` 降为可选（该构建为静态链接时不产生此 DLL）。

**修复 #6（PowerShell 参数传递）** workflow 里调用 `windows-verify.ps1` 的参数改用**哈希表 splat**
（`@{ RuntimeDir = ...; SkipBuild = $true }`）；PowerShell **数组 splat 不会解析 `-Name`**，而是按位置绑定，
导致 `-SkipBuild` 被当成位置参数绑到 `-Seconds` 并报类型转换错误。

**修复 #7（CI 验收语义）** CI 传 `-SkipBuild` 时不构建 `mpv-probe.exe`，原逻辑把「探针不可用」
判为 W3/W4 FAIL → job 失败。已改为：`-SkipBuild`（CI）下探针缺失 → W3/W4 **SKIP**（真机项留待目标机器）；
真机省略 `-SkipBuild` 时探针缺失仍提示 WARN。CI 期望结果变为 W7a–d PASS、W1/W2/W3/W4/W5/W6 SKIP、FAIL=0。

### 4.5.3 最终产物（run 39204754721 artifacts，可直接下载）

| artifact | 大小 | 内容 |
|---|---|---|
| `windows-player-runtime` | 42,752,825 B | `网盘管理-win64.zip`（96,096,567 B / 80 文件）+ `验收日志/windows-verify-report.json` |
| `media-stack-windows` | 38,676,182 B | `vendor/prebuilt/windows-x86_64/`（libmpv-2.dll + 20 个依赖 DLL + include/licenses/BUILDINFO） |

`网盘管理-win64.zip` 本地解压校验（sha256 `0d82b60f…`，仓库副本 `出/网盘管理-win64.zip`）：

* `网盘管理.exe` 19,870,208 B（sha256 `16f38f0b…`）、`wp-cli.exe` 5,718,016 B；
* `播放器运行时/` 共 **23 个 DLL**：`libmpv-2.dll`（sha256 `58214a9e…`，3,180,620 B，自建 LGPL）、
  `avcodec-63 / avformat-63 / avfilter-12 / avutil-61 / swresample-7 / swscale-10 / avdevice-63`、
  `libass-9 / libplacebo-360 / libbluray-4 / libshaderc_shared / libspirv-cross-c-shared`、
  `libEGL / libGLESv2`（ANGLE）+ `d3dcompiler_47`、
  `libwinpthread-1 / libgcc_s_seh-1 / libstdc++-6`、`zlib1 / libiconv-2 / libjpeg-8 / liblcms2-2`；
* `播放器运行时/LICENSES/` **11 份**（ANGLE-BSD-3-Clause、FFmpeg-LGPL-2.1、LGPL-2.1、GPL-2.0 等）；
* `windows-verify-report.json`：`{pass:4, warn:0, fail:0, skip:6, total:10}`
  （W7a–d PASS；W1/W2/W3/W4/W5/W6 因 CI 无桌面/样本 SKIP，留待目标 Windows 机器按 §5/§8 补跑）。

> CI 无桌面/GPU：`run_gl_verify=false` 时 W1/W3/W4/W5 走 SKIP/WARN，W2/W7 自动判定；
> GL 上下文/硬解/HDR 等真机项仍需按 §5/§8 在目标 Windows 机器补跑。

## 5. M0-Win 验证清单（需要在 Windows 机器上跑；Mac 不可替代）

| # | 验证项 | 通过标准 | 一键脚本 `windows-verify.ps1` |
|---|---|---|---|
| W1 | 启动后 GL 上下文类型 | 日志/诊断显示 `EGL`/`ANGLE`（GL_RENDERER 含 ANGLE） | 半自动：有 `-GlRendererLog` 则自动判，否则 WARN 提示人工确认 |
| W2 | libmpv render API 出帧 | 本地 1080p/4K 视频画面正常，UI 可覆盖 | ✅ 自动（render_probe：`frames_rendered>0 && first_frame`） |
| W3 | 硬解 | `hwdec-current` = `d3d11va`（或 `nvdec`），无回读 | ✅ 自动（mpv-probe：d3d11va/nvdec/dxva2；驱动相关可为 WARN） |
| W4 | 4K HEVC 10bit 性能 | 丢帧 < 0.5%，CPU 单核 < 40% | ✅ 自动（需 `-Sample4K`；无样本 SKIP） |
| W5 | HDR | 记录 `target-colorspace-hint` 表现（直通/降级 tone-map），写入文档 | 半自动（需 HDR 显示器 + `-HdrScreen`；结论需人工记录） |
| W6 | 无 ANGLE 环境 | 删除 ANGLE DLL 后能回退 WGL 启动（软件解码/提示），不崩溃 | ✅ 自动（复制运行时→删 ANGLE→再跑；CI 无桌面时 SKIP） |
| W7 | 打包完整性 | 发行包在干净 Windows 10/11 可启动并播放 | ✅ 自动（package -Check -Strict + exe/运行时同级布局 + ANGLE 就位） |

脚本输出 `出\验收日志\windows-verify-report.json`（host/GPU/checks/summary，退出码 0=无 FAIL）：

```powershell
# 全量（本机 Windows；需先准备好 播放器运行时\ 与本地视频样本）
powershell -ExecutionPolicy Bypass -File .\scripts\windows-verify.ps1 `
  -Sample1080 D:\test\1080p.mp4 -Sample4K D:\test\4k-hevc-10bit.mp4 `
  -GlRendererLog .\gl.log          # gl.log 可选：含 GL_RENDERER 的日志

# CI/无桌面会话：跳过 GL 项（W2/W7 仍自动）
powershell -ExecutionPolicy Bypass -File .\scripts\windows-verify.ps1 -SkipGl
```

> 若 W3/W4 不达标：Fallback 顺序 = ① 换 ANGLE/mpv 版本重试；② 4K 场景提示「用外部播放器」；
> ③ 评估 mpv 0.42 D3D11 render API + wgpu 合成（另立 Phase 2，不改当前计划）。

## 6. 风险

- ANGLE 与 WGL 混用/驱动兼容性（Intel/NVIDIA/AMD 分别验证）；
- GLES 下 egui 渲染正确性（eframe/egui_glow 官方支持 GLES，问题概率低）；
- mpv 在 ANGLE 上下文下的 d3d11va 互操作需实测（W3 是关键 gate）；
- HDR 直通在 GL/ANGLE 上可能不可用（可通过 Windows 原生 HDR 路径后续优化）。

---

## 7. Windows 构建结构（源码可控 + LGPL）

### 7.1 两条构建路径

| 路径 | 入口 | 用途 |
|---|---|---|
| **CI（推荐）** | `.github/workflows/player-windows.yml` | push tag `v*` 或 Actions 手动触发；产出 `windows-player-runtime` artifact |
| 本地 Windows | `scripts/build-media-stack-windows.ps1 -Run` | 开发机/离线构建；默认只看步骤，`-Run` 执行 |

### 7.2 构建链（关键：FFmpeg 必须 LGPL 构建）

```text
MSYS2 UCRT64
  ├─ pacman: gcc/meson/ninja/pkgconf + libass/libplacebo/libbluray/luajit（动态依赖）
  └─ 源码构建 LGPL FFmpeg（--disable-gpl --disable-nonfree --disable-version3）
       └─ 源码构建 mpv v0.41.0
            meson -Dgpl=false -Dlibmpv=true -Dcplayer=false
            → libmpv-2.dll（LGPL-2.1+）
  stage → vendor/prebuilt/windows-x86_64/
```

> ⚠️ **不能直接用 MSYS2 的 `mingw-w64-ucrt-x86_64-ffmpeg` 包**：那是 GPL 构建
> （见 `docs/PLAYER_PACKAGING.md` §5 的 GPL 风险分析）。随包发行必须用源码 LGPL 构建；
> 构建脚本的 `BUILDINFO.txt` 会记录 `ffmpeg_version` 与 LGPL 配置，发布验收据此审计。
>
> ⚠️ **Windows 构建环境三个坑（CI 实测，2026-10-19，详见 §4.5.2）**：
> 1. **构建目录必须纯 ASCII**：meson 的编译器探测/链接测试用绝对路径，中文目录下 MinGW gcc
>    打不开测试文件 → 所有 `-W` 探测返回 NO、`find_library('avrt')` 失败（看起来像工具链坏了）。
> 2. **不要写死 `C:\msys64`**：`msys2/setup-msys2` 实际安装位置需从 MSYS2 shell 的
>    `cygpath -w /` 获取后传给 pwsh 构建脚本。
> 3. **先跑 `-Preflight`**（cc / `-lavrt` / 最小 meson 项目 `find_library('avrt')`，约 20 秒），
>    再做完整的 FFmpeg+mpv 构建。
>
> 另：`libavrt.a` 位于 `/ucrt64/lib/`（不在 `x86_64-w64-mingw32/lib/`）；mingw meson 用
> `/ucrt64/bin/meson.exe`（自带 UCRT64 python，避免 meson 拒绝 MSYS python + MinGW 环境）。

### 7.3 产物结构（`vendor/prebuilt/windows-x86_64/` → `出/网盘管理-win64/`）

```text
出/网盘管理-win64/
├─ 网盘管理.exe  wp-cli.exe  portable  docs/  README.md  图标/
└─ 播放器运行时/
   ├─ libmpv-2.dll
   ├─ libass-*.dll  avcodec-*.dll  avformat-*.dll  avutil-*.dll
   ├─ avfilter-*.dll  swresample-*.dll  swscale-*.dll  avdevice-*.dll
   │   # ↑ Windows 版 FFmpeg 的 DLL 名没有 lib 前缀（如 avcodec-62.dll），
   │   #   来自 build-prefix/ffmpeg/bin（stage 步骤显式拷贝）
   ├─ libplacebo-*.dll  libbluray-*.dll  libluajit-*.dll  libwinpthread-1.dll（静态链接时可能无）…
   ├─ libEGL.dll  libGLESv2.dll  [d3dcompiler_47.dll]     # scripts/fetch-angle.ps1
   ├─ LICENSES/…            # 10 份第三方许可 + README（来源）
   ├─ BUILDINFO.txt  DEPENDENCIES.txt  LOAD-ORDER.txt  README.txt
   └─ 源码获取说明.txt        # LGPL §6 源码提供方式
```

### 7.4 CI 手动触发（本仓库暂无 GitHub 远端时）

1. 把仓库推送到 GitHub（任意私有/公开仓库均可，workflow 路径不依赖远端配置）；
2. Actions 页 → **player-windows** → **Run workflow**（可选改 `mpv_version` / `ffmpeg_version`；
   `run_gl_verify` 默认 false，CI 无 GPU 时保持 false）；
3. 完成后下载 artifact：`windows-player-runtime`（zip + `windows-verify-report.json`）
   与 `media-stack-windows`（libmpv 构建产物）。
4. 也可 `git tag v1.0.0 && git push origin v1.0.0` 自动触发。

### 7.5 Linux 上的静态检查（本仓库开发机为 Linux，无 pwsh/Windows）

* PowerShell 脚本（`fetch-angle.ps1` / `windows-verify.ps1` / `build-media-stack-windows.ps1` /
  `package-player-runtime-windows.ps1`）**无法在 Linux 上执行**（无 pwsh，且涉及 Windows API/PE）；
  本仓库用「括号/here-string/引号配平 + UTF-8 BOM + 关键字结构」静态检查，见任务报告；
  真正的语法/运行验证在 Windows 或 `pwsh` 环境（`pwsh -NoProfile -File`）。
* YAML 用 PyYAML 解析校验；`.github/workflows/player-windows.yml` 与现有 `build.yml` 并存，
  **不影响 Linux 流水线**。
* ANGLE 归档的 URL/sha256 已在 Linux 侧实际下载并核对（归档与 DLL hash 均与 pin 一致），
  仅「在 Windows 上跑 fetch-angle.ps1」这一步无法在本机验证。

---

## 8. 用户机器逐步验证（目标 Windows 10/11）

> 要求：Windows 10/11 x64、显卡驱动较新（NVIDIA/AMD/Intel 均可）、可用桌面会话
> （远程桌面/RDP 亦可，但 GL 上下文类型可能不同；物理机最稳）。
> 不需要安装 mpv/FFmpeg——运行时随包。

```powershell
# 0) 解压 网盘管理-win64.zip 到任意目录（路径建议无空格/中文亦可）
cd D:\网盘管理-win64

# 1) 结构自检（无需运行 GUI）：确认 exe 与 播放器运行时\ 同级、DLL/许可证齐全
powershell -ExecutionPolicy Bypass -File .\scripts\package-player-runtime-windows.ps1 -Check -Strict
#   （若发行包内未带 scripts\，可用仓库里的这份脚本并 -RuntimeDir 指向 播放器运行时\）

# 2) 一键 W1–W7 验收（生成 JSON 报告；建议准备 1080p 与 4K HEVC 10bit 样本）
powershell -ExecutionPolicy Bypass -File .\scripts\windows-verify.ps1 `
  -RuntimeDir .\播放器运行时 -AppExe .\网盘管理.exe `
  -Sample1080 D:\test\1080p.mp4 -Sample4K D:\test\4k-hevc-10bit.mp4

# 3) 手动勾选（脚本给 WARN/SKIP 的项）
#    - 启动 网盘管理.exe，播放在线视频 → UI 覆盖正常（W2）
#    - 任务管理器/GPU-Z 看硬解是否启用（W3）
#    - 4K 播放看是否卡顿/丢帧（W4）
#    - HDR 显示器：记录画面是否过曝/发灰（W5）
#    - 把 播放器运行时\libEGL.dll 改名 → 启动 → 应回退 WGL/给提示而非崩溃（W6）
```

期望：`windows-verify-report.json` 的 `summary.fail == 0`；`W1` 若为 WARN，请补 `-GlRendererLog`
或在 GUI 诊断日志中确认 `GL_RENDERER` 含 `ANGLE` 后手写进验收记录。

---

## 9. ANGLE 获取与校验（脚本）

```powershell
# 默认：下载固定版本 → 校验归档 sha256 → 提取 DLL → 校验每个 DLL sha256/PE → 写来源说明
powershell -ExecutionPolicy Bypass -File .\scripts\fetch-angle.ps1

# 只看 pin（不下载）
powershell -ExecutionPolicy Bypass -File .\scripts\fetch-angle.ps1 -PrintOnly

# 离线：浏览器下载 zip 后传入
powershell -ExecutionPolicy Bypass -File .\scripts\fetch-angle.ps1 -Archive .\angle-cccf60c-win32-x64.zip

# 附带 EGL/GLES 头文件（一般不需要；应用不编译 ANGLE）
powershell -ExecutionPolicy Bypass -File .\scripts\fetch-angle.ps1 -WithHeaders
```

产物：`vendor\prebuilt\windows-x86_64\angle\{libEGL.dll,libGLESv2.dll,d3dcompiler_47.dll,
ANGLE-LICENSE.txt,ANGLE-SOURCE.txt}`；`-Force` 覆盖已有。SHA256 不匹配时脚本打印实测值并退出非 0
（更新 pin 的位置在脚本顶部 `$AngleArchiveSha` / `$AngleDlls`）。

---

## 10. 已知限制

| # | 限制 | 说明 / 缓解 |
|---|---|---|
| L1 | **需 Windows 10/11 x64 + 较新显卡驱动** | Win7/8 与老驱动不保证 EGL/ANGLE；W6 可验证回退行为 |
| L2 | ANGLE 版本绑定 Chrome stable（当前 155.0.8059.80） | 升级 = 改 `fetch-angle.ps1` 顶部 pin 并重跑 W1/W6；不要混用不同来源 ANGLE |
| L3 | W1/W3/W4/W5 **只能在 Windows 实测** | CI 可自动 W2/W7；其余项目请按 §8 在目标机跑 `windows-verify.ps1` |
| L4 | HDR 直通未验证 | 需 HDR 显示器；GL/ANGLE 下可能只能 tone-map，记录到验收报告 |
| L5 | CI 无 GPU/桌面，GL 项默认 SKIP | workflow 的 `run_gl_verify` 默认 false；真实验收以 Windows 机器报告为准 |
| L6 | 未做代码签名 | SmartScreen 会提示「未知发布者」；发行前可选购买证书签名 |
| L7 | 首启需要 VC++/系统组件？ | 当前 DLL 均随包（UCRT 由系统提供，Win10+ 自带）；若缺 UCRT 提示安装 VC++ 运行库 |
| L8 | 发行许可合规 | 依赖 DLL 为 LGPL/BSD；FFmpeg 必须用 LGPL 构建（脚本已固定）——见 `docs/PLAYER_PACKAGING.md` §4/§5 |
