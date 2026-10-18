#!/usr/bin/env bash
# 构建 vendor media-stack：**LGPL 构建的 libmpv**（Linux x86_64）。
#
# 两种模式：
#  1) 默认（M0 开发联调）：用**系统 FFmpeg** 构建 libmpv → vendor/prebuilt/linux-x86_64/
#  2) `--lgpl-ffmpeg`（M6 发行合规）：先自建 **LGPL FFmpeg**，再用它的 pkg-config
#     重链 libmpv → vendor/prebuilt/linux-x86_64-lgpl/（配合
#     `scripts/package-player-runtime-linux.sh --self-contained` 打包自包含运行时）
#
# 为什么需要模式 2：发行版 FFmpeg 常为 **GPL(v3) 构建**（如 Arch 官方包），
# 动态链接它随包分发有 GPL 传染风险；LGPL 运行时必须让 FFmpeg 也来自 LGPL 构建。
#
# 用法：
#   ./scripts/build-media-stack-linux.sh                 # 系统 FFmpeg → linux-x86_64
#   ./scripts/build-media-stack-linux.sh --lgpl-ffmpeg   # 自建 LGPL FFmpeg → linux-x86_64-lgpl
#   ./scripts/build-media-stack-linux.sh --lgpl-ffmpeg --ffmpeg-only   # 只构建 FFmpeg
#   ./scripts/build-media-stack-linux.sh --clean         # 清理源码/构建目录后重建
#   MPV_VERSION=0.41.0 FFMPEG_VERSION=8.0 ./scripts/build-media-stack-linux.sh --lgpl-ffmpeg
#   ./scripts/build-media-stack-linux.sh --workdir /tmp/build --prefix /tmp/out
#
# 依赖（Arch）：base-devel ninja meson pkg-config ffmpeg libass libplacebo libbluray
# 产物：
#   <prefix>/lib/libmpv.so.<api>           libmpv（LGPL-2.1+，-Dgpl=false）
#   <prefix>/include/mpv/*.h              客户端头文件
#   <prefix>/licenses/                    mpv (+ FFmpeg LGPL) 许可证
#   <prefix>/BUILDINFO.txt                sha256 / ldd / 构建参数 / 依赖版本
#   vendor/build/ffmpeg-lgpl/             LGPL FFmpeg 安装前缀（--lgpl-ffmpeg）
set -euo pipefail
export LC_ALL=C

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MPV_VERSION="${MPV_VERSION:-0.41.0}"
FFMPEG_VERSION="${FFMPEG_VERSION:-8.0}"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/wp-media-stack-build"
WORKDIR="${WORKDIR:-$CACHE}"
JOBS="${JOBS:-$(nproc 2>/dev/null || echo 4)}"
FFMPEG_JOBS="${FFMPEG_JOBS:-$JOBS}"
JOBS="${JOBS:-$(nproc 2>/dev/null || echo 4)}"
LGPL_FFMPEG=0
FFMPEG_ONLY=0
CLEAN=0
PREFIX=""

while [ $# -gt 0 ]; do
  case "$1" in
    --lgpl-ffmpeg) LGPL_FFMPEG=1 ;;
    --ffmpeg-only) FFMPEG_ONLY=1 ;;
    --clean) CLEAN=1 ;;
    --workdir) WORKDIR="$2"; shift ;;
    --prefix) PREFIX="$2"; shift ;;
    --jobs) JOBS="$2"; shift ;;
    --ffmpeg-jobs) FFMPEG_JOBS="$2"; shift ;;
    -h|--help) sed -n '2,40p' "$0"; exit 0 ;;
    *) echo "未知参数：$1" >&2; exit 2 ;;
  esac
  shift
done

FFMPEG_PREFIX="$ROOT/vendor/build/ffmpeg-lgpl"
if [ -z "$PREFIX" ]; then
  if [ "$LGPL_FFMPEG" -eq 1 ]; then
    PREFIX="$ROOT/vendor/prebuilt/linux-x86_64-lgpl"
  else
    PREFIX="$ROOT/vendor/prebuilt/linux-x86_64"
  fi
fi

say() { printf '\n==> %s\n' "$*"; }
die() { printf '\n[错误] %s\n' "$*" >&2; exit 1; }
need_cmd() { command -v "$1" >/dev/null 2>&1 || die "缺少命令：$1（安装依赖后再试）"; }

# ---------------- 0. 依赖检查 ----------------
say "检查构建依赖"
for c in meson ninja pkg-config cc curl tar; do need_cmd "$c"; done
if [ "$LGPL_FFMPEG" -eq 1 ] && [ ! -d "$FFMPEG_PREFIX/lib/pkgconfig" ]; then
  need_cmd make
  for c in gcc g++ ld; do need_cmd "$c"; done
fi
# 系统开发库（LGPL 依赖，动态链接；发行合规由打包脚本统一校验）
for pc in libass libplacebo libbluray; do
  pkg-config --exists "$pc" || die "缺少依赖：$pc-dev（Arch: sudo pacman -S libass libplacebo libbluray）"
done

# ---------------- 1. LGPL FFmpeg（可选，M6 合规模式） ----------------
build_ffmpeg() {
  say "构建 LGPL FFmpeg $FFMPEG_VERSION（--disable-gpl/version3/nonfree → 纯 LGPL）"
  local src="$CACHE/ffmpeg-$FFMPEG_VERSION"
  local tar="$CACHE/ffmpeg-$FFMPEG_VERSION.tar.xz"
  local log="$CACHE/ffmpeg-build.log"
  mkdir -p "$CACHE" "$FFMPEG_PREFIX"
  if [ "$CLEAN" -eq 1 ]; then rm -rf "$src" "$FFMPEG_PREFIX"; fi
  if [ ! -s "$tar" ]; then
    say "下载 FFmpeg $FFMPEG_VERSION 源码（ffmpeg.org → gh-proxy 回退）"
    curl -fL --retry 2 --connect-timeout 20 -o "$tar.part" \
      "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz" || \
      curl -fL --retry 2 --connect-timeout 20 -o "$tar.part" \
      "https://gh-proxy.com/https://github.com/FFmpeg/FFmpeg/archive/refs/tags/n$FFMPEG_VERSION.tar.gz"
    mv "$tar.part" "$tar"
  fi
  if [ ! -d "$src" ]; then
    say "解压 FFmpeg 源码"
    tar -xf "$tar" -C "$CACHE"
    mv "$CACHE/ffmpeg-$FFMPEG_VERSION" "$src" 2>/dev/null || true
    # gh-proxy tarball 解出来可能是 FFmpeg-n8.0
    if [ ! -d "$src" ]; then mv "$CACHE/FFmpeg-n$FFMPEG_VERSION" "$src" 2>/dev/null || true; fi
  fi
  [ -d "$src" ] || die "FFmpeg 源码目录不存在：$src"
  ( cd "$src" && ./configure \
      --prefix="$FFMPEG_PREFIX" \
      --disable-gpl --disable-version3 --disable-nonfree \
      --disable-doc --disable-programs \
      --enable-shared --disable-static --enable-pic --disable-debug \
      --enable-vaapi ) > "$log" 2>&1 || { tail -40 "$log"; die "FFmpeg configure 失败（日志：$log）"; }
  # configure 会打印 License: LGPL version 2.1 or later；不是就失败
  grep -q 'License: LGPL version 2.1 or later' "$log" || \
    { grep -iE 'license|gpl' "$log" | head -5; die "FFmpeg 不是 LGPL 构建（见 $log）"; }
  say "FFmpeg configure 通过（License: LGPL 2.1+），开始编译 make -j$FFMPEG_JOBS（预计 10–20 分钟）"
  ( cd "$src" && make -j "$FFMPEG_JOBS" ) > "$log.make" 2>&1 || { tail -30 "$log.make"; die "FFmpeg make 失败（日志：$log.make）"; }
  ( cd "$src" && make install ) > "$log.install" 2>&1 || die "FFmpeg install 失败"
  ls "$FFMPEG_PREFIX/lib"/libavcodec.so.* >/dev/null || die "FFmpeg 产物缺失"
  say "LGPL FFmpeg 安装完成：$FFMPEG_PREFIX"
}

if [ "$LGPL_FFMPEG" -eq 1 ]; then
  build_ffmpeg
  if [ "$FFMPEG_ONLY" -eq 1 ]; then
    say "只构建 FFmpeg（--ffmpeg-only），结束"
    exit 0
  fi
fi

# ---------------- 2. mpv 源码 ----------------
mkdir -p "$WORKDIR"
TARBALL="$WORKDIR/mpv-v${MPV_VERSION}.tar.gz"
SRC_DIR="$WORKDIR/mpv-$MPV_VERSION"
if [ ! -s "$TARBALL" ]; then
  say "下载 mpv v${MPV_VERSION} 源码"
  URL_DIRECT="https://github.com/mpv-player/mpv/archive/refs/tags/v${MPV_VERSION}.tar.gz"
  URL_PROXY="https://gh-proxy.com/${URL_DIRECT}"
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
if [ "$CLEAN" -eq 1 ]; then rm -rf "$SRC_DIR" "$WORKDIR/mpv-build-lgpl" "$WORKDIR/mpv-build"; fi
if [ ! -d "$SRC_DIR" ]; then
  say "解压 mpv 源码"
  tar -xzf "$TARBALL" -C "$WORKDIR"
fi
[ -d "$SRC_DIR" ] || die "源码目录不存在：$SRC_DIR"

# ---------------- 3. meson 配置 + 编译 ----------------
if [ "$LGPL_FFMPEG" -eq 1 ]; then
  BUILD_DIR="${WORKDIR}/mpv-build-lgpl"
  MESON_EXTRA=()
else
  BUILD_DIR="${WORKDIR}/mpv-build"
  MESON_EXTRA=()
fi
say "meson 配置（LGPL：-Dgpl=false -Dlibmpv=true -Dcplayer=false；rubberband=disabled 避免 GPL 依赖）"
export PKG_CONFIG_PATH="$FFMPEG_PREFIX/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
if [ "$LGPL_FFMPEG" -eq 1 ]; then
  # 断言 pkg-config 优先命中自建 FFmpeg
  av_ver="$(pkg-config --modversion libavcodec 2>/dev/null || echo missing)"
  av_dir="$(pkg-config --variable=libdir libavcodec 2>/dev/null || echo missing)"
  case "$av_dir" in
    "$FFMPEG_PREFIX"/*) echo "    libavcodec $av_ver ← $av_dir ✔（自建 LGPL FFmpeg）" ;;
    *) die "pkg-config 未命中自建 FFmpeg（libavcodec=$av_ver @ $av_dir）；检查 PKG_CONFIG_PATH=$PKG_CONFIG_PATH" ;;
  esac
fi
MESON_OPTS=(
  --buildtype=release
  --prefix=/usr
  -Dgpl=false
  -Dlibmpv=true
  -Dcplayer=false
  -Dbuild-date=false
  -Dtests=false
  -Dmanpage-build=disabled
  -Drubberband=disabled
  -Djavascript=disabled
  "${MESON_EXTRA[@]}"
)
if [ -d "$BUILD_DIR" ]; then
  # --reconfigure 也要带 -D：新加的构建选项（如 -Djavascript=disabled）否则不生效
  meson setup --reconfigure "$BUILD_DIR" "$SRC_DIR" "${MESON_OPTS[@]}" >/dev/null
else
  meson setup "$BUILD_DIR" "$SRC_DIR" "${MESON_OPTS[@]}"
fi
say "编译 libmpv（ninja -j$JOBS）"
ninja -C "$BUILD_DIR" -j "$JOBS"
[ -e "$BUILD_DIR/libmpv.so.2" ] || die "构建产物缺失：$BUILD_DIR/libmpv.so.2"

# ---------------- 4. 安装 staging → vendor ----------------
STAGE_DIR="$WORKDIR/stage"
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
if [ "$LGPL_FFMPEG" -eq 1 ]; then
  mkdir -p "$PREFIX/licenses/ffmpeg"
  cp -f "$CACHE/ffmpeg-$FFMPEG_VERSION/COPYING.LGPLv2.1" "$PREFIX/licenses/ffmpeg/" 2>/dev/null || true
  cp -f "$CACHE/ffmpeg-$FFMPEG_VERSION/COPYING.LGPLv3" "$PREFIX/licenses/ffmpeg/" 2>/dev/null || true
  cp -f "$CACHE/ffmpeg-$FFMPEG_VERSION/LICENSE.md" "$PREFIX/licenses/ffmpeg/" 2>/dev/null || true
fi

# ---------------- 5. 构建信息 + 自检 ----------------
SO_PATH="$PREFIX/lib/$BASE_NAME"
say "生成 BUILDINFO.txt 并自检"
{
  echo "media-stack (LGPL libmpv) build info"
  echo "built_at:      $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  echo "mpv_version:   v${MPV_VERSION}"
  if [ "$LGPL_FFMPEG" -eq 1 ]; then
    echo "ffmpeg:        LGPL build v${FFMPEG_VERSION}（自建：$FFMPEG_PREFIX，--disable-gpl --disable-version3 --disable-nonfree）"
  else
    echo "ffmpeg:        系统 FFmpeg（⚠️ 不含发行合规保证；正式发行请用 --lgpl-ffmpeg）"
  fi
  echo "builder:       $(uname -srm)"
  echo "build_type:    release, -Dgpl=false -Dlibmpv=true -Dcplayer=false -Drubberband=disabled"
  echo "soname:        $(objdump -p "$SO_PATH" 2>/dev/null | awk '/SONAME/{print $2}')"
  echo "size_bytes:    $(stat -c%s "$SO_PATH")"
  echo "sha256:        $(sha256sum "$SO_PATH" | awk '{print $1}')"
  echo
  echo "NEEDED(直接依赖 SONAME):"
  readelf -d "$SO_PATH" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/  \1/p'
  echo
  echo "linked_deps(ldd，带自建依赖路径):"
  LD_LIBRARY_PATH="$FFMPEG_PREFIX/lib:$PREFIX/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
    ldd "$SO_PATH" 2>/dev/null | sed 's/^/  /' || true
  if [ "$LGPL_FFMPEG" -eq 1 ]; then
    echo
    echo "ffmpeg_libs(自建目录):"
    ls -l "$FFMPEG_PREFIX/lib"/libav*.so.* "$FFMPEG_PREFIX/lib"/libsw*.so.* 2>/dev/null | sed 's/^/  /'
    echo
    echo "ffmpeg_configure_license:"
    grep -E 'License:' "$CACHE/ffmpeg-build.log" 2>/dev/null | sed 's/^/  /' || true
  fi
} > "$PREFIX/BUILDINFO.txt"

API="$(
LD_LIBRARY_PATH="$FFMPEG_PREFIX/lib:$PREFIX/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
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

# LGPL 模式下额外自检：NEEDED 里 libav* 的 SONAME 必须来自 FFmpeg 8（.62）而非系统 FFmpeg 9（.63）
if [ "$LGPL_FFMPEG" -eq 1 ]; then
  if readelf -d "$SO_PATH" | grep -q 'libavcodec.so.62'; then
    echo "    FFmpeg ABI 自检：libavcodec.so.62（FFmpeg 8，自建 LGPL）✔"
  else
    echo "    ⚠️ FFmpeg ABI 自检：未发现 libavcodec.so.62（可能 pkg-config 未命中自建 FFmpeg）" >&2
  fi
  if LD_LIBRARY_PATH="$FFMPEG_PREFIX/lib:$PREFIX/lib" ldd "$SO_PATH" 2>/dev/null | grep -q '/usr/lib/libavcodec'; then
    echo "    ⚠️ ldd 仍解析到系统 libavcodec（运行期需 LD_LIBRARY_PATH 指向自建 FFmpeg 或打包 rpath）" >&2
  fi
fi

say "完成 ✅"
cat <<EOF

产物：
  $PREFIX/lib/$BASE_NAME
  $PREFIX/include/mpv/*.h
  $PREFIX/BUILDINFO.txt

使用方式：
  1) 开发联调：WP_MPV_LIB=$PREFIX/lib/libmpv.so.2 cargo run -p wp-gui
  2) 发行打包：./scripts/package-player-runtime-linux.sh --self-contained
     （收集全部非 glibc 依赖、patchelf rpath=\$ORIGIN、合规校验）

⚠️ 许可证：mpv 核心 LGPL-2.1+，FFmpeg 亦为 LGPL（--lgpl-ffmpeg 模式）；
   动态链接的非 GPL 依赖（libass ISC / libplacebo LGPL / libbluray LGPL 等）随包声明。
EOF
