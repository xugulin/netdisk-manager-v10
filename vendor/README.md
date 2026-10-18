# vendor：固定版本源码与补丁（源码可控）

| 目录 | 来源 | 版本/许可 | 补丁 |
|---|---|---|---|
| `glutin-winit/` | crates.io `glutin-winit 0.5.0` | MIT（见 LICENSE） | Windows 下把默认 API 优先级 `WglThenEgl` 改为 `EglThenWgl`（优先 ANGLE），见 `src/lib.rs` 注释 |

用途：满足「源码可控 + 可审计」要求。所有第三方源码保留原始 LICENSE，修改处在文件内注明 `[网盘管理 V10 patch]`。
libmpv 及其依赖（FFmpeg/libass/libplacebo/libbluray）的 vendored 构建由 `scripts/build-media-stack-*.{sh,ps1}` 生成，
产物放 `vendor/prebuilt/<platform>/`（不进 git），补丁放 `vendor/patches/`。

---

## media-stack（LGPL libmpv，M0-E1 已完成 Linux；M6 合规版见下）

### M6 合规：LGPL FFmpeg + 重链 libmpv + 自包含运行时（2026-10-19）

- `./scripts/build-media-stack-linux.sh --lgpl-ffmpeg`：先自建 **LGPL FFmpeg 8.0**
  （`--disable-gpl --disable-version3 --disable-nonfree`，装到 `vendor/build/ffmpeg-lgpl/`，
  gitignore），再用它重链 mpv v0.41.0 → `vendor/prebuilt/linux-x86_64-lgpl/`。
- `./scripts/package-player-runtime-linux.sh --self-contained`：自动收集 libmpv 的全部
  非 glibc 依赖到 `播放器运行时/lib/`（108 个 .so、126MB），`patchelf rpath=$ORIGIN`，
  合规校验（libav* 必须来自自建 LGPL FFmpeg、无 x264/x265/rubberband 等 GPL-only 组件）。
- 发行请用自包含运行时；M0 的 `linux-x86_64/` 仅供开发联调（动态链接系统 FFmpeg，
  发行版 FFmpeg 常为 GPL 构建 → 不可随包分发）。
- 详细数据/验证证据：`docs/PLAYER_PACKAGING.md` §10。



`scripts/build-media-stack-linux.sh` 用系统依赖 + mpv v0.41.0 源码构建 **LGPL libmpv**
（`-Dgpl=false -Dlibmpv=true -Dcplayer=false`），产物布局：

```text
vendor/prebuilt/linux-x86_64/
├── lib/libmpv.so.2.5.0 (+ .so.2/.so 符号链接，不进 git)、mpv.pc
├── include/mpv/{client,render,render_gl,stream_cb}.h      # 对照用，进 git
├── licenses/{LICENSE.LGPL,LICENSE.GPL,Copyright}          # 进 git
└── BUILDINFO.txt   # 版本/构建参数/sha256/ldd，进 git
```

- 构建：`./scripts/build-media-stack-linux.sh`（默认 mpv 0.41.0；`--clean` 重建）。
- 运行期加载顺序（`wp-media-mpv::library_candidates`）：显式 `MpvConfig.lib_path` >
  环境变量 `WP_MPV_LIB` > 系统 `libmpv.so.2` > 可执行文件同级 `播放器运行时/`。
- Windows：`scripts/build-media-stack-windows.ps1`（MSYS2/MinGW-w64 文档级骨架，M0 未实测）。

### 许可证（重要）

- mpv 核心为 **LGPL-2.1-or-later**（`-Dgpl=false`）；应用本体保持 MIT（运行时动态加载，不静态链接）。
- 依赖库当前**动态链接系统版本**；部分发行版 FFmpeg 为 GPL 构建（如 Arch 官方包）。
  M0 内测可用，**正式发行前**需换 LGPL 构建 FFmpeg 并随包声明（M6 发行合规任务）。
- 随包发行：把 `lib/` 动态库（含依赖）拷到「可执行文件同级 `播放器运行时/`」并附 `licenses/`。
- 实测数据见 `docs/PLAYER_M0_RESULTS.md`（E1 构建、E2 渲染、E3 字幕、E4 硬解）。

