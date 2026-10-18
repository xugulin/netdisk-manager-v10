# 构建 vendor media-stack（LGPL libmpv）——Windows 版（**文档级骨架，M0 未实测**）
#
# 状态：本文件是 Windows 侧的构建方案骨架；本机（Linux）无法验证 MSYS2 产物。
#       Windows 播放路径依赖 Mode A + ANGLE（见 docs/PLAYER_WINDOWS_ANGLE.md），
#       需要 `libmpv-2.dll` 为带 OpenGL render API 的 LGPL 构建。
#
# 方案：MSYS2 MinGW-w64（UCRT64）里用 meson 构建 mpv，链接 MinGW 版 FFmpeg/libass/libplacebo。
#
# 一次性准备（MSYS2 UCRT64 shell）：
#   pacman -S --needed base-devel git meson ninja pkgconf \
#       mingw-w64-ucrt-x86_64-gcc mingw-w64-ucrt-x86_64-ffmpeg \
#       mingw-w64-ucrt-x86_64-libass mingw-w64-ucrt-x86_64-libplacebo \
#       mingw-w64-ucrt-x86_64-libbluray mingw-w64-ucrt-x86_64-luajit
#
# 构建（在仓库根目录，MSYS2 UCRT64 shell 中执行）：
#   MPV_VERSION=0.41.0
#   curl -fL -o mpv.tar.gz "https://github.com/mpv-player/mpv/archive/refs/tags/v${MPV_VERSION}.tar.gz"
#   tar -xzf mpv.tar.gz
#   meson setup build-windows "mpv-${MPV_VERSION}" \
#       --buildtype=release --prefix=/usr \
#       -Dgpl=false -Dlibmpv=true -Dcplayer=false -Dbuild-date=false \
#       -Dtests=false -Dmanpage-build=disabled
#   ninja -C build-windows
#
#   mkdir -p vendor/prebuilt/windows-x86_64/lib
#   cp build-windows/libmpv-2.dll vendor/prebuilt/windows-x86_64/lib/
#
# 运行期依赖（必须一起放进 播放器运行时\ 或与 exe 同级）：
#   libmpv-2.dll
#   libass-9.dll / libavcodec-*.dll / libavformat-*.dll / libavutil-*.dll /
#   libavfilter-*.dll / libswresample-*.dll / libswscale-*.dll / libplacebo-*.dll /
#   libbluray-*.dll / libluajit-*.dll / libwinpthread-1.dll 等
#   （用 `ntldd -R libmpv-2.dll` 或 Dependencies.exe 枚举，逐个补齐）
#
# 需要确认/实测的点（列入 M0/T6 待办）：
#   1. LGPL 构建的 libmpv 在 **ANGLE 提供的 EGL/GLES 上下文**下能否创建 render context
#      （MPV_RENDER_PARAM_OPENGL_INIT_PARAMS 走 ANGLE 的 get_proc_address）；
#   2. 是否能用 d3d11va 硬解（libmpv LGPL 构建不带 GPL 的 d3d11va 互操作？需实测；
#      参考 docs/PLAYER_WINDOWS_ANGLE.md 的验证清单）；
#   3. HDR / 4K60 丢帧、GPU 占用；
#   4. 产物与 LGPL 声明文件（LICENSE.LGPL、Copyright）一起放进 licenses/。
#
# 产物布局（与 Linux 对齐）：
#   vendor/prebuilt/windows-x86_64/lib/libmpv-2.dll
#   vendor/prebuilt/windows-x86_64/licenses/{LICENSE.LGPL,LICENSE.GPL,Copyright}
#   vendor/prebuilt/windows-x86_64/BUILDINFO.txt
#
Write-Host "Windows media-stack 构建为文档级骨架（见本脚本注释）；M0 未实测。" -ForegroundColor Yellow
