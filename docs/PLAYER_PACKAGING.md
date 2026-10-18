# 播放器运行时打包与 LGPL 合规（M6）

> 状态：**Linux 系统依赖模式已落地并本机实测生成**；Windows 为结构级脚本 + 文档（本机无 Windows，
> PE/DLL 递归依赖需 Windows 构建机复核）；自包含模式已实现脚本路径、待 LGPL 依赖构建后启用。
>
> 关联：任务 task-16（M6 打包合规）；构建来源 [build-media-stack-linux.sh](../scripts/build-media-stack-linux.sh)、
> [build-media-stack-windows.ps1](../scripts/build-media-stack-windows.ps1)；
> Windows 渲染路线 [PLAYER_WINDOWS_ANGLE.md](PLAYER_WINDOWS_ANGLE.md)；
> libmpv 加载逻辑 `crates/wp-media-mpv/src/lib.rs`（`library_candidates()`）。

---

## 0. TL;DR

| 项 | 现状 |
|---|---|
| Linux 运行时打包 | ✅ `scripts/package-player-runtime-linux.sh` 实测通过（sha256/readelf/ldd/许可证全绿） |
| Linux 依赖模式 | **系统依赖模式（system-deps）**：发行包只带 LGPL libmpv；FFmpeg/libass/libplacebo 等由用户系统提供，脚本输出 139 个 Arch 包名清单 |
| 自包含模式 | 脚本已实现（拷依赖 .so + `patchelf --set-rpath '$ORIGIN'`）；当前 prebuilt 无语义依赖库 → **明确报错**，待 LGPL FFmpeg 等构建后启用 |
| Windows 打包 | 结构级脚本 `scripts/package-player-runtime-windows.ps1`（DLL/ANGLE/许可证/加载顺序校验，不下载 DLL） |
| 许可材料 | ✅ `assets/licenses/`（10 份许可证文本 + 来源说明），打包脚本自动随包 |
| 发布验收 | ✅ `scripts/release-verify-linux.sh` 实测 **PASS=22 FAIL=0**：随包 libmpv 真实加载 + 发布布局模拟 + 短播放断言（见 §8） |
| LGPL 合规 | 动态链接 + 可替换 + 源码获取说明 + 许可证随包；GPL 可选构建单独说明（§6） |

**一键生成 Linux 运行时**：

```bash
./scripts/package-player-runtime-linux.sh            # → ./播放器运行时/（gitignore）
./scripts/package-player-runtime-linux.sh --verify-only
./scripts/build-linux.sh --portable                 # 自动拷入 出/网盘管理-linux-x64/
```

---

## 1. 运行时来源（vendored build）

播放器内核不用系统 `libmpv`，而是**固定版本 + LGPL 构建**后随包（源码可控要求）：

* 构建脚本：`scripts/build-media-stack-linux.sh`（Linux）、`scripts/build-media-stack-windows.ps1`（Windows 骨架）
* 固定版本：**mpv v0.41.0**，meson 选项 `-Dgpl=false -Dlibmpv=true -Dcplayer=false`
  → mpv 核心以 **LGPL-2.1+** 发布（不是 GPL）
* 产物目录：`vendor/prebuilt/<平台>/`
  * Linux：`lib/libmpv.so.2.5.0`（SONAME `libmpv.so.2`）+ `include/mpv/*.h` + `lib/mpv.pc`
    + `licenses/{LICENSE.LGPL,LICENSE.GPL,Copyright}` + `BUILDINFO.txt`
  * Windows：`lib/libmpv-2.dll`（待 Windows 构建机产出）
* `BUILDINFO.txt` 记录：`built_at / mpv_version / builder / build_type / soname / size_bytes / sha256 /
  linked_deps（完整 ldd）/ meson summary`——打包脚本以其中的 `sha256` 作为校验基准。

> 为什么不用系统 libmpv：发行版 libmpv 通常是 GPL 构建（`-Dgpl=true`）且版本不可控，既违反
> 「源码可控」也不满足 LGPL 发行条件。我们只链接**自己构建的 LGPL 版 libmpv**。

---

## 2. Linux 发行结构

### 2.1 目录树（系统依赖模式，默认）

```text
出/网盘管理-linux-x64/            # scripts/build-linux.sh --portable 产出
├─ 网盘管理                      # Rust GUI 主程序（动态加载 libmpv）
├─ wp-cli                        # 命令行工具
├─ portable                      # 绿色版标记（数据写 exe 旁）
├─ docs/  README.md
├─ 图标/
└─ 播放器运行时/                 # ← scripts/package-player-runtime-linux.sh 生成
   ├─ lib/
   │  ├─ libmpv.so.2.5.0         # 我们构建的 LGPL libmpv（3.4MB）
   │  ├─ libmpv.so.2 -> libmpv.so.2.5.0
   │  └─ libmpv.so -> libmpv.so.2
   ├─ LICENSES/
   │  ├─ LGPL-2.1.txt  GPL-2.0.txt  mpv-Copyright.txt
   │  ├─ libass-ISC.txt  FFmpeg-LGPL-2.1.txt  libplacebo-LGPL-2.1.txt
   │  ├─ libbluray-LGPL-2.1.txt  libaacs-LGPL-2.1.txt  libbdplus-LGPL-2.1.txt
   │  ├─ ANGLE-BSD-3-Clause.txt
   │  └─ README.md               # 每份许可证对应的组件/来源
   ├─ BUILDINFO.txt              # 打包信息 + sha256/SONAME/ldd + 源构建信息 + 许可证校验和
   ├─ DEPENDENCIES.txt           # 系统依赖清单：soname → 路径 → 发行版包 → 类别
   ├─ README.txt                 # 运行/替换/依赖说明
   └─ 源码获取说明.txt            # LGPL 源码获取方式（合规必需）
```

`make` / 打包脚本没有构建 `include/` 时，运行包只有 `lib/`（发行不需要头文件）；
`--with-headers` 会额外附带头文件与 `mpv.pc`（仅开发联调用）。

### 2.2 系统依赖模式（默认，当前可发布）

当前 `libmpv.so.2.5.0` 是**动态链接系统库**的构建（`ldd` 见 BUILDINFO），
因此发行包**不随附** FFmpeg/libass/libplacebo/libmujs 等，目标机器必须已安装它们。
打包脚本自动生成 `DEPENDENCIES.txt`：

| 列 | 含义 |
|---|---|
| soname | 依赖的共享库 SONAME（如 `libass.so.9`） |
| 系统路径 | 本机 ldd 解析出的实际路径 |
| 所属发行版包 | 用 `pacman -Qo`（Arch）或 `dpkg -S`（Debian）反查包名+版本；基础库标记「系统基础」 |
| 类别 | 系统基础（glibc/GCC 运行时）/ 需要安装 / 第三方（未能映射到包） |

末尾自动汇总**去重后的发行版包清单**与安装建议：

```text
# ── 需要安装的发行版包（去重；Arch 可用）──
  - ffmpeg libass libplacebo libbluray ...
#   Arch:  sudo pacman -S --needed <上面列出的包>
#   Debian/Ubuntu: sudo apt install <对应运行库包>（soname→包名映射不同）
```

真机实测（本机 Arch，FFmpeg 9.0.2 / libass 0.17.5 / libplacebo 7.360.1）：

* 依赖 191 项（含 glibc 基础库），全部可解析；去重后 **139 个 Arch 包**（其中 `ffmpeg` 覆盖全部
  libav* 组件，`mesa`/`vulkan-icd-loader`/`libva`/`libvdpau` 覆盖 GPU 硬解路径）；
* 硬件解码另需目标机器安装对应 GPU 驱动（Mesa / NVIDIA 私有驱动），脚本不强制、文档说明；
* Wayland/X11 运行时库在依赖清单内（libwayland*、libxcb*、libx11 等）。

### 2.3 自包含模式（后续，脚本已实现路径）

目标：发行包自带全部依赖 `.so`，目标机器零安装。做法：

1. 用 **LGPL 构建的 FFmpeg/libass/libplacebo/libbluray/libmujs** 等重构建 libmpv
   （或静态链接进 libmpv；注意 LGPL 静态链接需提供可重链接的目标文件，见 §4.7）；
2. 把依赖 `.so` 放进 `vendor/prebuilt/linux-x86_64/lib/`（或 `--deps-dir` 指定目录）；
3. 运行打包脚本自包含模式：

```bash
./scripts/package-player-runtime-linux.sh --self-contained
# 或： --self-contained --deps-dir /path/to/lgpl-deps/lib
```

脚本会：拷贝依赖到 `播放器运行时/lib/` → `patchelf --set-rpath '$ORIGIN'`（每库，需安装 patchelf）
→ 用 `LD_LIBRARY_PATH=lib/ ldd` 校验无缺失 → 写入 BUILDINFO 的 `package_mode: self-contained`。

**当前状态**：AUR prebuilt 只有 libmpv 本体、依赖系统库，因此 `--self-contained` 会明确报错：

```text
✗ 自包含模式失败：prebuilt/lib 与 --deps-dir 均未提供依赖 .so（当前构建动态链接系统库）。
  后续做法见 docs/PLAYER_PACKAGING.md §2.3：用 LGPL 构建的 FFmpeg/libass/libplacebo 等
  （或静态链接）重构建 libmpv，把依赖 .so 放到 prebuilt/lib/ 后重跑 --self-contained。
```

> 这是**有意的失败**：宁可明确拒绝，也不生成一个「看起来自包含、实际缺库」的发行包。

### 2.4 与 `scripts/build-linux.sh` 的对接

`build-linux.sh --portable` 已有逻辑：若仓库根存在 `播放器运行时/`，整目录拷入
`出/网盘管理-linux-x64/播放器运行时/`（见该脚本 :68-72）。因此标准发布流程：

```bash
./scripts/build-media-stack-linux.sh          # 可选：重建 vendor/prebuilt（已有产物可跳过）
./scripts/package-player-runtime-linux.sh     # 生成 ./播放器运行时（=可发布的运行时）
./scripts/build-linux.sh --portable           # 构建并把运行时拷进便携目录
./scripts/package-player-runtime-linux.sh --verify-only   # 复核（也可对便携目录 --out 校验）
```

### 2.5 libmpv 加载顺序（决定「随包运行时」何时生效）

`wp-media-mpv::library_candidates_annotated()` 的候选顺序（Linux，**发布形态优先随包库**）：

| 优先级 | 来源 | 路径 | `CandidateSource` |
|---|---|---|---|
| 1 | 显式指定 | `MpvConfig.lib_path` / `load_library(Some(path))` | `Explicit` |
| 2 | 环境变量 | `$WP_MPV_LIB` | `Env` |
| 3 | **随包运行时** | `<exe目录>/播放器运行时/lib/libmpv.so.2`（及 `.so` / 根目录变体） | `BundledRuntime` |
| 4 | 程序目录 | `<exe目录>/libmpv.so.2`（某些打包方式直接放 exe 旁） | `ExeDir` |
| 5 | 系统库兜底 | `/usr/lib/libmpv.so.2`、`/usr/local/lib/...`、多架构目录 | `System` |

要点：

* **随包运行时排在系统库之前**：发布目录里只要存在 `播放器运行时/lib/libmpv.so.2`，就会优先用随包库
  （不再出现「开发机装了 mpv 就永远用系统库」的问题）；
* 候选路径**相对 `current_exe()` 解析**，因此 dev（`target/debug/`）与发布目录通用；
* **实际加载路径会记录到日志**：`已加载 libmpv：<path>（来源：随包运行时，client API 2.5）`；
  也可用 `wp_media_mpv::probe_version_with_path() -> Option<(描述, PathBuf)>` 在启动自检/验收中读取；
* dev 环境（`target/debug/` 旁没有 `播放器运行时/`）自动回退系统库；要验证随包产物可显式指定：
  ```bash
  WP_MPV_LIB=./出/网盘管理-linux-x64/播放器运行时/lib/libmpv.so.2 ./出/网盘管理-linux-x64/网盘管理
  ```
* 也可据此实现「用户自选运行时」：改 `WP_MPV_LIB` 即可替换，无需重编译（LGPL 可替换性要求，见 §4.5）。

---

## 3. Windows 发行结构（结构级）

### 3.1 目录树

```text
出/网盘管理-win64/
├─ 网盘管理.exe
├─ wp-cli.exe
├─ portable
├─ docs/  README.md
└─ 播放器运行时/
   ├─ libmpv-2.dll              # 我们 vendored 的 LGPL libmpv（带 OpenGL render API）
   ├─ libass-*.dll  libavcodec-*.dll  libavformat-*.dll  libavutil-*.dll
   ├─ libavfilter-*.dll  libswresample-*.dll  libswscale-*.dll  libavdevice-*.dll
   ├─ libplacebo-*.dll  libbluray-*.dll  libluajit-*.dll  libwinpthread-1.dll …
   ├─ libEGL.dll  libGLESv2.dll  # ANGLE（BSD-3-Clause）；d3dcompiler_47.dll 可选
   ├─ LICENSES/…                # 同 Linux（含 ANGLE-BSD-3-Clause.txt）
   ├─ BUILDINFO.txt
   ├─ DEPENDENCIES.txt          # 必需/可选 DLL 清单 + 来源
   ├─ LOAD-ORDER.txt            # DLL 搜索顺序 + 发布前检查
   └─ README.txt
```

DLL 来源：
1. `libmpv-2.dll` 与 FFmpeg/libass/libplacebo 等：MSYS2 UCRT64 下按
   [build-media-stack-windows.ps1](../scripts/build-media-stack-windows.ps1) 构建/收集，放入
   `vendor/prebuilt/windows-x86_64/lib/`；
2. ANGLE `libEGL.dll`/`libGLESv2.dll`：**用 `scripts/fetch-angle.ps1` 从固定来源（ANGLE prebuilt，
   Chrome stable 同源，已 pin 版本 + 归档/DLL sha256）下载并校验**；⚠️ 2026-10 实测 **mpv 官方
   Windows 发行包已不再自带 ANGLE**（旧方案失效），不建议从 Chromium/Edge 安装目录抠（版本不确定）。
   放 `vendor/prebuilt/windows-x86_64/angle/`，打包脚本会一并复制。

### 3.2 DLL 加载顺序（关键）

* 应用侧（`wp-media-mpv`）：先找 exe 同级 `播放器运行时\libmpv-2.dll`、`播放器运行时\lib\libmpv-2.dll`、
  再找 exe 同级；支持 `WP_MPV_LIB` 覆盖。**libmpv 必须与自己依赖的 DLL 同目录**（或都在搜索路径内）。
* libmpv 加载依赖 DLL 时，Windows 默认只搜「exe 目录 + System32 + PATH」，**不会自动搜
  `播放器运行时\` 子目录** → 应用启动时必须把该目录注册进进程搜索路径：
  * 推荐：`SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS)` +
    `AddDllDirectory(<exe>\播放器运行时)`（进程级、不污染全局 PATH）；
  * 不要用 `SetDllDirectory`（全局、会覆盖 exe 目录语义）或改用户 PATH（污染环境）。
* ANGLE 的 `libEGL.dll`/`libGLESv2.dll` 同样通过上述目录注册后加载（应用创建 GL 上下文前）。
* 细节与渲染链路见 [PLAYER_WINDOWS_ANGLE.md](PLAYER_WINDOWS_ANGLE.md)。

### 3.3 结构级脚本用法（本机无 Windows，只做结构/清单校验）

```powershell
# 只校验（默认；缺必需 DLL 时 Errors>0，Strict 模式退出码 1）
powershell -ExecutionPolicy Bypass -File .\scripts\package-player-runtime-windows.ps1 -Check

# 生成目录骨架 + LICENSES + 清单 + 加载顺序说明（开发机无 DLL 也可跑，缺 DLL 只警告）
powershell -ExecutionPolicy Bypass -File .\scripts\package-player-runtime-windows.ps1 -Generate

# 生成到便携目录并严格校验
powershell -ExecutionPolicy Bypass -File .\scripts\package-player-runtime-windows.ps1 -Generate `
    -RuntimeDir "出\网盘管理-win64\播放器运行时" -PrebuiltDir "vendor\prebuilt\windows-x86_64"
```

脚本职责：必需/可选 DLL 清单检查（通配符）、LICENSES 完整性、LOAD-ORDER/README/BUILDINFO 存在性、
与 exe 同级布局提示；**不联网下载 DLL、不构建**。PE 架构、签名、递归依赖仍需在 Windows 构建机用
`ntldd -R libmpv-2.dll`（MSYS2）或 [Dependencies.exe](https://github.com/lucasg/Dependencies) 复核。

### 3.4 Windows 待实测项（沿用 ANGLE 文档清单）

W1 GL 上下文类型为 EGL/ANGLE；W2 render API 出帧；W3 `hwdec-current=d3d11va`；
W4 4K HEVC 10bit 性能（丢帧<0.5%）；W5 HDR 行为记录；W6 无 ANGLE 可回退 WGL；W7 干净 Win10/11 可启动播放。
详见 [PLAYER_WINDOWS_ANGLE.md](PLAYER_WINDOWS_ANGLE.md) §5。

---

## 4. LGPL 合规义务（重点）

我们发行的 `播放器运行时/` 中，**libmpv 是 LGPL-2.1+ 的库**（`-Dgpl=false` 构建）。
LGPL 允许闭源程序动态链接它，但必须满足以下义务：

### 4.1 动态链接 + 可替换

* libmpv 以**动态库**（`libmpv.so.2` / `libmpv-2.dll`）随包，不被静态链接进 `网盘管理` 可执行文件；
* 用户必须能**替换**该库：应用运行时动态加载（`libloading` + `WP_MPV_LIB` / 目录候选），
  用户放一个 ABI 兼容（client API 2.x）的 libmpv 即可替换，无需重编译本程序；
* 加载失败的候选会继续尝试下一个，缺库时给可读错误（不 panic）；
* `README.txt` / `源码获取说明.txt` 中写明替换方法。

### 4.2 对应源码提供（LGPL-2.1 §6 / GPL §3 精神）

发行包必须让接收者能获得**对应源码**。本仓库做法（`播放器运行时/源码获取说明.txt` 自动生成）：

* mpv 源码：`https://github.com/mpv-player/mpv/archive/refs/tags/v0.41.0.tar.gz`（固定 tag）；
* 构建脚本：`scripts/build-media-stack-linux.sh`（meson 参数完整可复现）；
* 对 mpv 的修改：**无**（直接上游源码构建；`vendor/glutin-winit` 的补丁属 Windows 窗口层 crate，
  不进入 libmpv 产物，但其源码随仓库提供）；
* 动态链接依赖（system-deps 模式不随包分发）：各库上游源码地址 + 构建机版本记录在说明文件；
* 建议在发行页/README 提供**书面承诺**：「本发行包所含 LGPL 组件的对应源码可向发行方索取，
  有效期 3 年」—— 至少保证上述 URL 长期可达。

### 4.3 许可证文本与版权声明随包

* `LICENSES/` 必含：LGPL-2.1、GPL-2.0（可选构建参考）、mpv Copyright、libass ISC、FFmpeg LGPL、
  libplacebo LGPL、libbluray/libaacs/libbdplus LGPL、ANGLE BSD-3；
* 每份文本在 `LICENSES/README.md` 注明对应组件与来源 URL；
* 打包脚本校验 `LICENSES/` 完整性（缺一份即失败）；`BUILDINFO.txt` 记录各许可证 sha256。

### 4.4 修改声明

* 若自行 patch 了 mpv/FFmpeg 等 LGPL 组件：必须在发行说明中声明修改、并随源码提供 patch；
* 当前 libmpv **无 patch**（`BUILDINFO.build_type` 可证：stock upstream + `-Dgpl=false`）；
* 若未来 patch，则同步更新 `scripts/build-media-stack-*.sh` 与 `源码获取说明.txt`。

### 4.5 用户可替换性验证（发布前手工做一次）

```bash
# 干净机器（无系统 libmpv）或强制指定：
WP_MPV_LIB=./播放器运行时/lib/libmpv.so.2 ./网盘管理 --demo
# 用系统/另一份 libmpv 替换：
WP_MPV_LIB=/usr/lib/libmpv.so.2 ./网盘管理 --demo      # 应能启动（版本兼容时）
# 删除运行时 → 应给可读错误（缺少 libmpv），而不是崩溃
```

### 4.6 禁止事项

* ❌ 静态链接 LGPL libmpv 进闭源可执行文件而不提供可重链接目标文件；
* ❌ 去掉/隐藏许可证文本、修改 libmpv 后不声明；
* ❌ 用 GPL 构建的 libmpv（`-Dgpl=true`）随闭源程序分发（若要用 GPL 构建，见 §6，整体按 GPL 处理）。

### 4.7 若未来走静态链接

静态链接 LGPL 库需满足 LGPL-2.1 §6(a)：「提供目标文件/链接脚本，使接收者能修改库后重新链接」，
或改用自包含的**动态** `.so` 随包方案（§2.3，首选）。

---

## 5. FFmpeg / 依赖的 GPL 风险（发行必读）

> ✅ **2026-10-19 更新**：已落地「自建 LGPL FFmpeg 8.0 + 重链 LGPL libmpv + 自包含运行时」，
> 实测数据见 **§10**。发行请用 `播放器运行时/`（自包含，含 LGPL FFmpeg），
> 不要用 system-deps 模式链接发行版 GPL FFmpeg。

**事实（本机实测）**：Arch Linux 的 `ffmpeg 2:9.0.2` 是 **GPLv3 构建**：

```console
$ ffmpeg -version | head -1
ffmpeg version n9.0.2 ...
$ ffmpeg -L | head -3
ffmpeg is free software; ... GNU General Public License ... version 3
$ ffmpeg -buildconf | grep -E "enable-(gpl|version3)"
  --enable-gpl
  --enable-version3
```

含义与对策：

| 场景 | 合规判断 |
|---|---|
| **system-deps 模式（当前）**：我们只分发「应用 + LGPL libmpv」，**不分发 FFmpeg**；用户系统里的 GPL FFmpeg 由用户自行安装/使用 | 发行包本身不含 GPL 代码；libmpv 动态加载用户系统的 FFmpeg 属于「用户组合」，我们不做 GPL 声明义务。**这是当前可发布形态** |
| **自包含模式（未来）**：若把 ffmpeg 的 `.so` 打进发行包 | **必须换成 LGPL 构建的 FFmpeg**（`--disable-gpl`，或 `--enable-lgpl` 等价配置），否则整个组合按 GPLv3 分发，需履行 GPL 全部义务（源码、许可证、可再分发条款），与「闭源主程序」冲突 |
| **GPL 可选版**：故意提供 GPL 构建的整包 | 单独的 GPL 发行渠道，随包 LICENSE.GPL + 对应源码；不得与闭源版混发 |

因此：

* `package-player-runtime-linux.sh` 的**自包含模式**在正式发行前，必须先确认依赖库全为 LGPL/兼容许可；
  脚本会把 `ldd` 结果列入 `DEPENDENCIES.txt` 供审计；
* 官方发布检查清单（§7）要求：核对 `BUILDINFO.build_type` + 依赖清单中每个库的许可证；
* 文档中 `FFmpeg-LGPL-2.1.txt` 是为「LGPL 构建的 FFmpeg」准备的文本 —— 若发行方用 GPL FFmpeg，
  应改为 GPL-3.0 文本并按 §6 处理。

---

## 6. GPL 构建可选说明

mpv 支持 `-Dgpl=true`（引入 GPL-only 组件，例如部分蓝光/DVD 相关功能）。本项目默认使用
`-Dgpl=false`（LGPL）构建。若某发行版希望启用 GPL-only 特性：

1. 用 `-Dgpl=true` 重新构建 libmpv，产物按 **GPL-2.0+** 分发；
2. 该发行版整体（应用 + libmpv）必须按 **GPL** 兼容方式分发：随包 `LICENSE.GPL`、提供完整对应源码、
   允许再分发与修改；
3. 不能与 LGPL 发行版共用一个下载页/安装包而不加区分；
4. 我们的 `vendor/prebuilt/` 只保留 LGPL 构建；GPL 构建应放独立目录（如
   `vendor/prebuilt/linux-x86_64-gpl/`）并在 BUILDINFO 标注 `build_type: ... -Dgpl=true`；
5. 现有脚本预留：`build-media-stack-linux.sh` 可通过传入自定义 meson 参数扩展（当前默认 `-Dgpl=false`）。

> 结论：**默认发行 = LGPL**；GPL 版本仅作为可选、单独分发。

---

## 7. 发布前验证清单

### 7.1 Linux（脚本自动 + 人工复核）

自动化一（`package-player-runtime-linux.sh` 每次运行/`--verify-only` 都执行）：

- [x] `lib/libmpv.so.2.5.0` 存在且为 ELF 64-bit x86-64
- [x] **sha256 与 `vendor/prebuilt/.../BUILDINFO.txt` 一致**
- [x] `readelf -d` SONAME = `libmpv.so.2`（ABI 正确）
- [x] 符号链接 `libmpv.so.2`、`libmpv.so` 正确
- [x] `ldd` 无缺失依赖（system-deps 模式要求构建机全可解析；目标机按 DEPENDENCIES.txt 安装）
- [x] `LICENSES/` 10 份必需文本齐全
- [x] `源码获取说明.txt` 存在（LGPL 合规）
- [x] `DEPENDENCIES.txt` 存在

自动化二（`scripts/release-verify-linux.sh`，发布门禁/CI，见 §8）：

- [x] 打包自校验（上表）通过
- [x] 结构 / 许可证 / sha256 / SONAME 独立复核
- [x] **随包 libmpv 真实加载**（`WP_MPV_LIB` 指向随包库，strace 证据确认打开的是随包路径）
- [x] **未误用系统库**（加载日志无 `/usr/lib/libmpv`）
- [x] **发布布局模拟**：把探针放到「应用目录 + 同级 `播放器运行时/`」，不设 `WP_MPV_LIB` 也能按 exe 相对路径加载随包库
- [x] 系统库兜底可用（信息性）
- [x] headless 短播放断言（版本 / Opened 事件 / 无 open_error）

人工（发布前一次）：

- [ ] 干净 Linux（无系统 libmpv）解压即用：启动 → 播放本地 4K 视频 → 拖进度条 → 字幕/音轨切换
- [ ] `WP_MPV_LIB` 显式指定随包运行时生效（验证加载顺序）
- [ ] 删除 `播放器运行时/` → 启动给可读错误而非崩溃
- [ ] `DEPENDENCIES.txt` 包清单在一台干净机 `pacman -S --needed` 后播放正常（Arch 实机）
- [ ] 发行页挂出「源码获取」说明（或书面承诺）
- [ ] `LICENSES/` 文本与 `BUILDINFO` 校验和一致（脚本已记录）
- [ ] 若用自包含模式：依赖库许可证审计为 LGPL/兼容（§5）

### 7.2 Windows

- [ ] `package-player-runtime-windows.ps1 -Check -Strict` 全绿（必需 DLL/许可证/清单）
- [ ] `ntldd -R libmpv-2.dll` 递归依赖齐全（与 `DEPENDENCIES.txt` 一致）
- [ ] 干净 Win10/11：启动 → 内嵌播放 1080p/4K → ANGLE 生效（W1/W2）
- [ ] 硬解 `d3d11va` 生效（W3）；4K HEVC 10bit 性能达标（W4）
- [ ] 无 ANGLE 回退 WGL 不崩溃（W6）
- [ ] 删除 `libmpv-2.dll` → 可读错误；删除 `播放器运行时/` → 同
- [ ] `LICENSES/` 含 ANGLE BSD-3 文本；发行页挂源码获取说明

---

## 8. 发布验收（release-verify）与排查

### 8.1 一键验收

```bash
./scripts/release-verify-linux.sh                 # 全量（需已生成 播放器运行时/）
./scripts/release-verify-linux.sh --skip-play      # 只做静态校验（无 ffmpeg 的 CI 环境）
./scripts/release-verify-linux.sh --runtime-dir 出/网盘管理-linux-x64/播放器运行时
./scripts/release-verify-linux.sh --skip-package-verify   # 分阶段 CI
```

* 退出码 **0 = 全部 PASS**，非 0 = 有 FAIL（可直接作为发布门禁 / CI step）；
* 证据目录 `出/验收日志/release-verify-<时间戳>/`（gitignore），含：
  `summary.txt`、`package-verify.log`、`strace.log` + `strace-decoded.log`、
  `load-evidence.txt`、`load-evidence-release-layout.txt`、`play.json`、
  `play-release-layout.json`、`play-system-fallback.json`、`build-probe.log`；
* 依赖：`bash python3 sha256sum readelf file cargo`；`ffmpeg`（生成测试视频，可 `--skip-play` 跳过）；
  `strace`（推荐；缺省时自动退回 `LD_DEBUG=libs` 取证）。

### 8.2 检查项与期望输出（2026-10-19 本机实测：PASS=22 FAIL=0）

| 组 | 检查 | 期望 |
|---|---|---|
| 打包 | `package-player-runtime-linux.sh --verify-only` | 通过 |
| 结构 | libmpv 实体/符号链接/sha256/SONAME | 全部一致（sha256 与 BUILDINFO） |
| 许可 | `LICENSES/` 10 份 + `源码获取说明.txt` 等 | 齐全 |
| 探针 | 强制重建 `mpv-probe` | 构建成功（防止用陈旧二进制假验收） |
| 加载 | `WP_MPV_LIB=<运行时>/lib/libmpv.so.2` 真实加载 | strace 显示打开**随包路径**，无 `/usr/lib/libmpv` |
| 播放 | headless 短播放（2s testsrc） | `mpv v0.41.0（client API 2.5）`、Opened/FirstFrame、`open_error=null` |
| 兜底 | 不设 `WP_MPV_LIB`（dev 探针旁无运行时） | 回退系统 `libmpv`（信息性） |
| **发布布局** | 探针拷到「应用目录」，`播放器运行时/` 与其同级，**不设** `WP_MPV_LIB` | 按 exe 相对路径加载随包库 ✅ |

本次实测关键证据（证据目录 `出/验收日志/release-verify-20261019-032211/`）：

```text
# ① WP_MPV_LIB 显式指向随包库 → 打开的是随包路径（strace 解码后）
openat(AT_FDCWD, ".../播放器运行时/lib/libmpv.so.2", O_RDONLY|O_CLOEXEC) = 3

# ② 发布布局模拟（无 WP_MPV_LIB，exe 旁放 播放器运行时/ 软链）
openat(AT_FDCWD, "/tmp/wp-release-verify.jzFC8y/app/播放器运行时/lib/libmpv.so.2", ...) = 3
# play.json：mpv v0.41.0（client API 2.5），state=Ended，video=160x120，open→opened≈25ms

# ③ 主验收 play.json（WP_MPV_LIB 方式）
mpv_version = "mpv v0.41.0（client API 2.5）"
state = Ended, video_size = [160,120], tracks = 1, dropped_frames = 0
open_to_opened_ms ≈ 18.0ms, open_to_first_frame_ms ≈ 18.0ms
```

> 说明：短播放用 `mpv-probe --hwdec no`（`auto-safe` 的硬件解码设备初始化在高负载下会把
> open 拖到秒级，导致 2s 窗口内只看到 `Opening` 的假失败；验收口径固定为软解快速 open）。

### 8.3 排查：系统库被误用 / 缺依赖 / 版本不符

| 症状 | 可能原因 | 排查与修复 |
|---|---|---|
| 日志/`probe_version` 显示 `来源：系统库`，或 strace 出现 `/usr/lib/libmpv` | ① `播放器运行时/` 放错位置（不在 exe 同级）；② `WP_MPV_LIB` 指到了系统库；③ 用了旧版二进制 | `ls <exe目录>/播放器运行时/lib/libmpv.so.2`；确认日志「已加载 libmpv」路径；重新 `cargo build -p wp-media-mpv`/用 release 构建；`release-verify` 会自动重建探针并给出 strace 证据 |
| 启动报「无法加载 libmpv / cannot open shared object file」 | system-deps 模式缺系统依赖（FFmpeg/libass/libplacebo…） | 按 `播放器运行时/DEPENDENCIES.txt` 安装（Arch：`sudo pacman -S --needed <清单>`）；**不要**用 `LD_LIBRARY_PATH` 指向运行时 lib 掩盖缺依赖（那里只有 libmpv） |
| `libmpv client API 版本过低（… < 2.0）` | 系统 libmpv 太旧且被优先选中 | 把随包运行时放到 exe 同级（候选 3 会先于系统库），或用 `WP_MPV_LIB` 指向随包库 |
| 播放失败但库加载正常 | 与加载无关（编解码/网络/文件） | 看 `play.json` 的 `open_error` / mpv 日志；回到 M0/流媒体排查 |
| 手工 strace 找不到中文路径 | strace 把非 ASCII 文件名写成 `\ooo` 八进制转义 | `release-verify` 已自动解码；手工排查可 `python3` 将 `\([0-7]{3})` 逐字节解码后再 grep（见脚本 `decode_escaped_paths`） |
| 验收结果与预期相反但「看起来通过」 | 复用了**陈旧**的 mpv-probe/二进制（源码改了但没重编） | 验收脚本已**每次强制** `cargo build -q -p wp-media-mpv --bin mpv-probe`；CI 注意 cargo 缓存与源码 hash |

### 8.4 CI 集成建议

```yaml
# 伪代码：发布 job
- run: ./scripts/package-player-runtime-linux.sh
- run: ./scripts/release-verify-linux.sh          # 退出码 0 才继续
- uses: actions/upload-artifact@v4
  with: { name: release-verify-evidence, path: 出/验收日志/ }
- run: ./scripts/build-linux.sh --portable
```

验收证据（PASS/FAIL 明细 + strace + play.json）建议随发布产物归档，便于事后复核「当时确实加载了随包库」。

## 9. 脚本速查

| 脚本 | 作用 | 备注 |
|---|---|---|
| `scripts/build-media-stack-linux.sh` | 构建 LGPL libmpv；`--lgpl-ffmpeg` 先自建 LGPL FFmpeg 8.0 再重链 → `vendor/prebuilt/linux-x86_64-lgpl/` | 需 meson/ninja/nasm/FFmpeg 构建依赖；见 §10 |
| `scripts/build-media-stack-windows.ps1` | Windows 构建骨架（MSYS2 UCRT64） | 文档级，未实测 |
| `scripts/package-player-runtime-linux.sh` | **运行时打包/校验**（系统依赖/自包含） | 默认 `--system-deps`；`--self-contained` 自动收集依赖+合规校验（§10）；`--verify-only` |
| `scripts/package-player-runtime-windows.ps1` | Windows 运行时结构级生成/校验 | 不下载 DLL；PE 校验需 Windows |
| `scripts/release-verify-linux.sh` | **发布验收**：静态校验 + 随包库真实加载 + 发布布局模拟 + 短播放 | 退出码可进 CI；见 §8 |
| `scripts/build-linux.sh --portable` | 构建便携目录并自动拷入 `播放器运行时/` | 已对接 |
| `scripts/build-windows.ps1 -Portable` | 构建 Windows 便携目录 | 需手工/GUI 拷运行时（后续可对接） |

常用命令：

```bash
# Linux
./scripts/package-player-runtime-linux.sh                 # 生成 ./播放器运行时
./scripts/package-player-runtime-linux.sh --verify-only   # 校验
./scripts/package-player-runtime-linux.sh --with-headers  # 附带头文件（开发）
./scripts/build-linux.sh --portable                      # 打包到 出/

# Windows（在 Windows PowerShell 中）
powershell -ExecutionPolicy Bypass -File .\scripts\package-player-runtime-windows.ps1 -Check -Strict
powershell -ExecutionPolicy Bypass -File .\scripts\package-player-runtime-windows.ps1 -Generate
```

---

## 10. M6 合规自包含运行时实测（LGPL FFmpeg + libmpv，2026-10-19）

> 目标：解决「vendored libmpv 动态链接发行版 **GPL(v3) FFmpeg** → 随包分发不合规」。
> 结果：**自建 LGPL FFmpeg 8.0 + 重链 LGPL libmpv v0.41.0 + 108 个依赖全自包含运行时**，
> 合规校验 PASS，clean-env dlopen/播放/render API 均实测通过。体积 **126 MB**。

### 10.1 结论速览

| 项 | 结果 |
|---|---|
| LGPL FFmpeg | ✅ **8.0** 源码构建，configure 输出 `License: LGPL version 2.1 or later`；`vendor/build/ffmpeg-lgpl/` |
| LGPL libmpv | ✅ mpv **v0.41.0** 用自建 FFmpeg 重链；sha256 `4994092c…`；NEEDED `libavcodec.so.62`（自建 ABI，非系统 .63） |
| 自包含运行时 | ✅ `播放器运行时/` 126 MB：libmpv + **108 个依赖 .so**（含 7 个 libav*/libsw*）+ 103 个 SONAME 符号链接 |
| 合规校验 | ✅ `gpl_only_components: none`；无 x264/x265/rubberband/dvd*；7 个 FFmpeg 库均来自自建目录 |
| ldd 全解析 | ✅ `--verify-only` PASS（0 错误）；`LD_LIBRARY_PATH=播放器运行时/lib ldd`：0 个 not found |
| 干净 dlopen | ✅ `env -i`（无任何库路径）`ctypes.CDLL(播放器运行时/lib/libmpv.so.2)` → client API 2.5、`mpv_create` 成功 |
| 依赖来源审计 | ✅ `LD_DEBUG=libs`：系统 `libavcodec` 加载 **0** 次，运行时 `libavcodec` 加载 1 次；运行时库共加载 106 个 |
| 播放实测 | ✅ mpv-probe：1080p 播放 5s，首帧 38 ms，无错误；render_probe（Mode A）：47 帧/5s、`gl_error=0` |
| 既有流程回归 | ✅ `--system-deps` 模式仍可用（默认仍用 M0 prebuilt；LGPL 版只用于 self-contained） |

### 10.2 自建 LGPL FFmpeg（8.0）

```bash
# 一条命令（含下载/configure/make/install；首次约 5-8 分钟，8 核）
./scripts/build-media-stack-linux.sh --lgpl-ffmpeg
# 只建 FFmpeg（调试 configure 用）：
./scripts/build-media-stack-linux.sh --lgpl-ffmpeg --ffmpeg-only

# 等价手工命令
curl -fL -o ffmpeg-8.0.tar.xz https://ffmpeg.org/releases/ffmpeg-8.0.tar.xz
tar -xJf ffmpeg-8.0.tar.xz && cd ffmpeg-8.0
./configure --prefix="$PWD/vendor/build/ffmpeg-lgpl" \
  --disable-gpl --disable-version3 --disable-nonfree \
  --disable-doc --disable-programs \
  --enable-shared --disable-static --enable-pic --disable-debug \
  --enable-vaapi
make -j"$(nproc)" && make install
```

- 依赖：`nasm`（x86 汇编；缺了 configure 直接报错，脚本依赖检查里可加）、gcc/glibc、make；
- 产物（`vendor/build/ffmpeg-lgpl/lib/`）：`libavcodec.so.62.11.100`(19.4MB) / `libavformat.so.62` /
  `libavutil.so.60` / `libavfilter.so.11` / `libavdevice.so.62` / `libswresample.so.6` / `libswscale.so.9` + pkgconfig；
- 构建日志：`${XDG_CACHE_HOME}/wp-media-stack-build/ffmpeg-build.log`（含 `License: LGPL version 2.1 or later`）。

### 10.3 重链 LGPL libmpv v0.41.0

```bash
PKG_CONFIG_PATH="$PWD/vendor/build/ffmpeg-lgpl/lib/pkgconfig" \
./scripts/build-media-stack-linux.sh --lgpl-ffmpeg
# 产物：vendor/prebuilt/linux-x86_64-lgpl/{lib,include,licenses,mpv.pc,BUILDINFO.txt}
```

meson 选项：`-Dgpl=false -Dlibmpv=true -Dcplayer=false -Drubberband=disabled -Djavascript=disabled`
（**rubberband 是 GPL 依赖**；**禁用 mpv 内置 JS（libmujs）**——系统 mujs 无 SONAME 会写成绝对路径
NEEDED，禁用后运行时更干净；本应用不使用 mpv JS 脚本）。

实测数据：

| 字段 | 值 |
|---|---|
| mpv | v0.41.0 / client API **2.5** |
| 产物 | `libmpv.so.2.5.0`，3,401,784 B，sha256 `4994092c2e54dfcde94b158bbe51feebd6a4d4f62b6071082dfab13bf42fe7b4` |
| FFmpeg ABI | NEEDED `libavcodec.so.62`（FFmpeg 8；系统 FFmpeg 9 是 `.63`，可作为「没有误链系统库」的判据） |
| 构建信息 | `vendor/prebuilt/linux-x86_64-lgpl/BUILDINFO.txt`（sha256 / NEEDED / ldd / ffmpeg 来源 / configure license） |

### 10.4 自包含运行时打包（自动收集 + 合规校验）

```bash
./scripts/package-player-runtime-linux.sh --self-contained
# → ./播放器运行时/（.gitignore 已忽略）：
#    lib/libmpv.so.2.5.0 + 108 依赖 .so + SONAME 符号链接（rpath=$ORIGIN）
#    LICENSES/（11 份，含 FFmpeg-LGPL-2.1.txt）、源码获取说明.txt、DEPENDENCIES.txt、BUILDINFO.txt
```

脚本行为（本次扩展）：

1. **prebuilt 选择**：self-contained 自动优先 `vendor/prebuilt/linux-x86_64-lgpl`；system-deps 仍用 M0 prebuilt；
2. **依赖自动收集**：从 libmpv 的 `ldd`（带自建 FFmpeg 搜索路径）递归收集全部非 glibc 依赖；
   **glibc + GPU/驱动栈**（GL/EGL/GBM/DRM/VA/VDPAU/CUDA）保持系统提供，清单写入 BUILDINFO；
3. **SONAME 符号链接**：按 `readelf SONAME` 建链（如 `libavcodec.so.62 → libavcodec.so.62.11.100`）；
4. **绝对路径 NEEDED 修正**：如系统 mujs 这类无 SONAME 依赖，`patchelf --replace-needed` 改为 basename 并随包；
5. **patchelf --set-rpath '$ORIGIN'**：每个库都设；libmpv 被改写 → sha256 走「打包后」校验，
   源 sha 记在 `libmpv_source_sha256`；
6. **合规校验**（失败即退出非 0）：libav* 必须来自 `vendor/build/ffmpeg-lgpl/`；打包集合禁
   `libx264/libx265/libfdk-aac/librubberband/libdvd*`；`ldd` 全解析；clean-env dlopen。

实测产物数据：

| 字段 | 值 |
|---|---|
| 体积 | **126 MB**（lib/ 126MB；109 个实体 .so + 102 个 SONAME 符号链接） |
| 依赖 | 122 项（8 项为系统包：libdrm/libglvnd/libva/libvdpau/libx11/libxcb/expat/mesa），108 项随包 |
| 合规 | `gpl_only_components: none`；无 GPL-only 组件；7 个 FFmpeg 库来自自建目录 |
| 许可证 | LICENSES/ 11 份（LGPL-2.1/GPL-2.0/FFmpeg-LGPL-2.1/libass-ISC/libplacebo/libbluray/libaacs/libbdplus/ANGLE/mpv-Copyright + README） |

### 10.5 验证证据（本机实测）

```text
# ① 干净环境 dlopen（env -i，无任何 LD_LIBRARY_PATH / 系统 FFmpeg 参与）
dlopen OK client API 2.5 mpv_create=True

# ② ldd 全解析（仅靠 rpath=$ORIGIN，无 LD_LIBRARY_PATH）
$ env -u LD_LIBRARY_PATH ldd 播放器运行时/lib/libmpv.so.2 | grep -c 'not found'
0
$ env -u LD_LIBRARY_PATH ldd 播放器运行时/lib/libmpv.so.2 | grep libavcodec
  libavcodec.so.62 => .../播放器运行时/lib/libavcodec.so.62

# ③ LD_DEBUG 依赖来源审计（Rust 进程内真实加载）
system_libavcodec_loaded=0
runtime_libavcodec_loaded=1
runtime_lib(已加载数)=106
system_lib(允许清单外)=（无）

# ④ 播放（mpv-probe，随包运行时，rpath 解析）
{'state': 'Playing', 'video_size': [1920, 1080], 'duration_secs': 30.0,
 'hwdec_current': 'no', 'position_secs': 5.0, 'open_to_first_frame_ms': 38.2,
 'mpv_version': 'mpv v0.41.0（client API 2.5）'}  errors: []

# ⑤ Mode A 渲染（render_probe，Xvfb + 随包 libmpv）
{'state': 'Playing', 'video': [1920,1080], 'frames_rendered': 47, 'first_frame': True,
 'gl_error': 0, 'open_to_first_frame_ms': 115.3}

# ⑥ 既有流程回归
package-player-runtime-linux.sh --system-deps --out /tmp/rt-system  → 通过（默认 M0 prebuilt）
package-player-runtime-linux.sh --verify-only                        → 校验通过（0 错误）
```

> 说明：`LD_DEBUG` 里仍会看到少量 `/usr/lib` 库（`libexpat`/`libX11-xcb`/`libxcb-dri3`），
> 原因是**宿主进程已加载同名库**（Python/GUI 工具链）→ 动态加载器按 SONAME 复用已加载实例，
> 属正常 ABI 行为；仅 GPU/驱动栈（GL/EGL/GBM/DRM/VA）与 glibc 保留系统提供（必须匹配用户硬件）。

### 10.6 使用方式

- 发行：`./scripts/build-linux.sh --portable` 会把 `播放器运行时/` 拷进便携目录；应用加载顺序
  见 §2.5（`WP_MPV_LIB` > 随包 `播放器运行时/lib` > 系统兜底）。
- 开发联调：`WP_MPV_LIB="$PWD/vendor/prebuilt/linux-x86_64-lgpl/lib/libmpv.so.2"`；
  或自包含运行时 `WP_MPV_LIB="$PWD/播放器运行时/lib/libmpv.so.2"`（依赖走 rpath，无需 LD_LIBRARY_PATH）。
- 可替换性（LGPL 要求）：用户可把同 ABI 的 `libmpv.so.2` 及其依赖替换进 `播放器运行时/lib/`，无需重编译应用。

### 10.7 本轮踩坑记录（脚本已内置处理）

1. `meson setup --reconfigure` **必须同时传 `-D` 选项**，否则新增选项（本次 `-Djavascript=disabled`）不生效；
2. 发行版 `libmujs.so` 无 SONAME，mpv NEEDED 会写成**绝对路径** → 自包含需 `patchelf --replace-needed`；
   更彻底的办法是构建时 `-Djavascript=disabled`；
3. `patchelf` 改 rpath 会改变 libmpv 的 sha256 → 打包校验要用「打包后 sha + 记录源 sha」；
4. LGPL 构建里 `-Drubberband=disabled` 必须显式加（系统 librubberband 是 GPL）；
5. 系统 FFmpeg 9 与自建 FFmpeg 8 的 SONAME 不同（`.63` vs `.62`），可用 NEEDED 快速判定是否误链。

---

## 11. 已知限制与后续

| # | 项 | 说明 / 后续 |
|---|---|---|
| P1 | ~~Linux 自包含模式未启用~~ ✅ 已启用 | `scripts/build-media-stack-linux.sh --lgpl-ffmpeg` + `package-player-runtime-linux.sh --self-contained`；126MB 运行时，见 §10 |
| P2 | Windows 全链路未实测 | 本机无 Windows；结构级脚本已备，PE/ntldd/ANGLE 验证待 Windows 机器（M0-Win 清单） |
| P3 | 系统依赖版本漂移 | system-deps 依赖用户发行版版本；DEPENDENCIES.txt 记录构建机版本，发行说明注明「Linux x86_64 + 常见发行版」 |
| P4 | ~~FFmpeg GPL 构建~~ ✅ 已解决（自包含） | 自建 LGPL FFmpeg 8.0 + 重链 libmpv，自包含运行时已含 LGPL FFmpeg 与许可证；见 §10。system-deps 模式仍有 GPL 风险，仅用于开发联调 |
| P5 | 字体/编解码器 | 字幕字体走系统字体目录；某些私有编码（如 H.266）依赖系统 FFmpeg 能力，不额外打包 |
| P6 | Windows 便携目录对接 | `build-windows.ps1 -Portable` 尚未自动拷贝 `播放器运行时\`（可在 Windows 侧补一行） |
| P7 | 运行时体积 | 当前 3.6MB（仅 libmpv）；自包含模式预计 80–150MB（FFmpeg 全家桶），需评估分发方式 |
