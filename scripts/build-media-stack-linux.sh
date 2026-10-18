#!/usr/bin/env bash
# 构建 vendor media-stack：**LGPL 构建的 libmpv**（Linux x86_64）。
#
# 目的（M0-E1）：证明「源码可控」可落地——固定 mpv 版本、`-Dgpl=false` 的 LGPL
# 构建，产物 + 许可证 + 构建信息一起放进 `vendor/prebuilt/linux-x86_64/`。
#
# 用法：
#   ./scripts/build-media-stack-linux.sh                # 默认 mpv 0.41.0，构建到 vendor/prebuilt
#   ./scripts/build-media-stack-linux.sh --clean        # 清掉工作目录后重建
#   MPV_VERSION=0.41.0 ./scripts/build-media-stack-linux.sh
#   ./scripts/build-media-stack-linux.sh --workdir /tmp/mpv-build   # 指定构建目录
#
# 依赖（Arch）：base-devel ninja meson pkg-config ffmpeg libass libplacebo libbluray
#   本机已验证：mpv 0.41.0 + 系统 FFmpeg 8 + libass 0.17.5 + libplacebo 7.360.1 + libbluray 1.5.1
#
# ⚠️ 许可证说明：
#   - 本脚本用 `-Dgpl=false` 构建 mpv 核心 → LGPL-2.1+（见 vendor/prebuilt/.../licenses/）。
#   - 依赖（FFmpeg/libass/libplacebo/libbluray 等）**动态链接系统库**。若发行版 FFmpeg 是
#     GPL 构建（如 Arch），正式对外发行应换成 LGPL 构建的 FFmpeg 并随包提供；
#     本机 M0 阶段先验证「源码可复现构建 + 可播放」，发行合规在 M6 处理。
#
# 产物：
#   vendor/prebuilt/linux-x86_64/lib/libmpv.so.2.5.0 (+ .so.2 / .so 符号链接)
#   vendor/prebuilt/linux-x86_64/include/mpv/*.h
#   vendor/prebuilt/linux-x86_64/lib/mpv.pc
#   vendor/prebuilt/linux-x86_64/licenses/{LICENSE.LGPL,LICENSE.GPL,Copyright}
#   vendor/prebuilt/linux-x86_64/BUILDINFO.txt
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MPV_VERSION="${MPV_VERSION:-0.41.0}"
# 默认构建目录放到用户缓存（不进仓库；也避免污染 git status）
WORKDIR="${WORKDIR:-${XDG_CACHE_HOME:-$HOME/.cache}/wp-media-stack-build}"
PREFIX="${PREFIX:-$ROOT/vendor/prebuilt/linux-x86_64}"
JOBS="$(nproc 2>/dev/null || echo 4)"
CLEAN=0

while [ $# -gt 0 ]; do
  case "$1" in
    --clean) CLEAN=1 ;;
    --workdir) WORKDIR="$2"; shift ;;
    --prefix) PREFIX="$2"; shift ;;
    --jobs) JOBS="$2"; shift ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    *) echo "未知参数：$1" >&2; exit 2 ;;
  esac
  shift
done

say() { printf '\n==> %s\n' "$*"; }
die() { printf '\n[错误] %s\n' "$*" >&2; exit 1; }

# ---------- 0. 依赖检查 ----------
say "检查构建依赖"
missing=()
command -v meson >/dev/null || missing+=("meson")
command -v ninja >/dev/null || missing+=("ninja")
command -v pkg-config >/dev/null || missing+=("pkg-config")
command -v cc >/dev/null || missing+=("gcc")
for pc in libavformat libavcodec libavutil libavfilter libswresample libswscale \
          libass libplacebo libbluray; do
  pkg-config --exists "$pc" || missing+=("$pc-dev")
done
if [ ${#missing[@]} -gt 0 ]; then
  echo "缺少依赖：${missing[*]}"
  if command -v pacman >/dev/null && sudo -n true 2>/dev/null; then
    say "尝试用 pacman 安装：meson ninja pkgconf ffmpeg libass libplacebo libbluray"
    sudo pacman -S --needed --noconfirm meson ninja pkgconf ffmpeg libass libplacebo libbluray
  else
    die "请先安装：${missing[*]}（Arch: sudo pacman -S meson ninja pkgconf ffmpeg libass libplacebo libbluray）"
  fi
fi

# ---------- 1. 下载源码 ----------
mkdir -p "$WORKDIR"
TARBALL="$WORKDIR/mpv-v${MPV_VERSION}.tar.gz"
SRC_DIR="$WORKDIR/mpv-$MPV_VERSION"
BUILD_DIR="$WORKDIR/build"
STAGE_DIR="$WORKDIR/stage"

if [ ! -s "$TARBALL" ]; then
  say "下载 mpv v${MPV_VERSION} 源码"
  URL_DIRECT="https://github.com/mpv-player/mpv/archive/refs/tags/v${MPV_VERSION}.tar.gz"
  URL_PROXY="https://gh-proxy.com/${URL_DIRECT}"
  # 基座下载（加速源竞速/兜底 curl）若存在则优先
  DL_BIN=""
  for cand in "$(command -v 基座下载 2>/dev/null || true)" "/home/xgl/共享基座/命令/基座下载"; do
    if [ -n "$cand" ] && [ -x "$cand" ]; then DL_BIN="$cand"; break; fi
  done
  ok=0
  if [ -n "$DL_BIN" ]; then
    if "$DL_BIN" "$URL_PROXY" "$TARBALL" >/dev/null 2>&1 && [ -s "$TARBALL" ]; then ok=1; fi
  fi
  if [ "$ok" -eq 0 ]; then
    curl -fL --retry 2 --connect-timeout 15 -o "$TARBALL" "$URL_PROXY" || \
      curl -fL --retry 2 --connect-timeout 15 -o "$TARBALL" "$URL_DIRECT" || \
      die "下载 mpv 源码失败：$URL_DIRECT（可手动放到 $TARBALL）"
  fi
fi
[ -s "$TARBALL" ] || die "源码包为空：$TARBALL"

if [ "$CLEAN" -eq 1 ]; then
  rm -rf "$SRC_DIR" "$BUILD_DIR" "$STAGE_DIR"
fi
if [ ! -d "$SRC_DIR" ]; then
  say "解压源码"
  tar -xzf "$TARBALL" -C "$WORKDIR"
fi
[ -d "$SRC_DIR" ] || die "源码目录不存在：$SRC_DIR"

# ---------- 2. meson 配置（LGPL 构建）----------
say "meson 配置（LGPL：-Dgpl=false -Dlibmpv=true -Dcplayer=false）"
if [ -d "$BUILD_DIR" ]; then
  meson setup --reconfigure "$BUILD_DIR" "$SRC_DIR" >/dev/null
else
  meson setup "$BUILD_DIR" "$SRC_DIR" \
    --buildtype=release \
    --prefix=/usr \
    -Dgpl=false \
    -Dlibmpv=true \
    -Dcplayer=false \
    -Dbuild-date=false \
    -Dtests=false \
    -Dmanpage-build=disabled
fi

# ---------- 3. 编译 ----------
say "编译 libmpv（ninja -j$JOBS）"
ninja -C "$BUILD_DIR" -j "$JOBS"

[ -e "$BUILD_DIR/libmpv.so.2" ] || die "构建产物缺失：$BUILD_DIR/libmpv.so.2"

# ---------- 4. 安装到 staging + 拷贝到 vendor ----------
say "安装到 staging：$STAGE_DIR"
rm -rf "$STAGE_DIR"
DESTDIR="$STAGE_DIR" ninja -C "$BUILD_DIR" install >/dev/null

STAGED_LIB="$(find "$STAGE_DIR" -name 'libmpv.so.*.*' -type f | head -1)"
[ -n "$STAGED_LIB" ] || die "staging 中未找到 libmpv.so.*"

say "写入 $PREFIX"
mkdir -p "$PREFIX/lib" "$PREFIX/include" "$PREFIX/licenses"
cp -f "$STAGED_LIB" "$PREFIX/lib/"
BASE_NAME="$(basename "$STAGED_LIB")"
ln -sfn "$BASE_NAME" "$PREFIX/lib/libmpv.so.2"
ln -sfn "libmpv.so.2" "$PREFIX/lib/libmpv.so"
rm -rf "$PREFIX/include/mpv"
cp -a "$(dirname "$(find "$STAGE_DIR" -name client.h -path '*/mpv/*' | head -1)")" "$PREFIX/include/"
cp -f "$(find "$STAGE_DIR" -name 'mpv.pc' | head -1)" "$PREFIX/lib/"
cp -f "$SRC_DIR/LICENSE.LGPL" "$SRC_DIR/LICENSE.GPL" "$SRC_DIR/Copyright" "$PREFIX/licenses/"

# ---------- 5. 构建信息 + 自检 ----------
SO_PATH="$PREFIX/lib/$BASE_NAME"
say "生成 BUILDINFO.txt 并自检"
{
  echo "media-stack (LGPL libmpv) build info"
  echo "built_at:      $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  echo "mpv_version:   v${MPV_VERSION}"
  echo "builder:       $(uname -srm)"
  echo "build_type:    release, -Dgpl=false -Dlibmpv=true -Dcplayer=false"
  echo "soname:        $(objdump -p "$SO_PATH" 2>/dev/null | awk '/SONAME/{print $2}')"
  echo "size_bytes:    $(stat -c%s "$SO_PATH")"
  echo "sha256:        $(sha256sum "$SO_PATH" | awk '{print $1}')"
  echo
  echo "linked_deps:"
  ldd "$SO_PATH" 2>/dev/null | sed 's/^/  /' || true
  echo
  echo "meson summary (节选):"
  grep -E '^    (libmpv|opengl|vulkan|x11|wayland|drm|cocoa|d3d11|javascript|lua)' "$BUILD_DIR/meson-logs/meson-log.txt" 2>/dev/null | head -20 | sed 's/^/  /' || true
} > "$PREFIX/BUILDINFO.txt"

# client API 版本自检（用 python ctypes，避免额外编译）
API="$(
python3 - "$SO_PATH" <<'PY'
import ctypes, sys
lib = ctypes.CDLL(sys.argv[1])
lib.mpv_client_api_version.restype = ctypes.c_ulong
v = lib.mpv_client_api_version()
print(f"{(v >> 16) & 0xffff}.{v & 0xffff}")
PY
)"
echo "    client API：$API"
echo "    sha256：$(sha256sum "$SO_PATH" | awk '{print $1}')"

say "完成 ✅"
cat <<EOF

产物：
  $PREFIX/lib/$BASE_NAME
  $PREFIX/include/mpv/*.h
  $PREFIX/BUILDINFO.txt

使用方式（二选一）：
  1) 开发联调：WP_MPV_LIB=$PREFIX/lib/libmpv.so.2 cargo run -p wp-gui
  2) 随包发行：把 lib/ 下文件拷到「可执行文件同级的 播放器运行时/」目录
     （wp-media-mpv 的加载顺序：显式路径 > WP_MPV_LIB > 系统库 > 播放器运行时/）

⚠️ 许可证：mpv 核心为 LGPL-2.1+（-Dgpl=false）；依赖库动态链接系统版本。
   正式发行前需将 GPL 构建的 FFmpeg 替换为 LGPL 构建并随包声明（见本脚本头部注释）。
EOF
