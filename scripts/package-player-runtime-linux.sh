#!/usr/bin/env bash
# 播放器运行时打包（Linux x86_64）—— 从 vendor/prebuilt 产出发行用 `播放器运行时/`。
#
# 产物结构（默认输出 <仓库>/播放器运行时/，已在 .gitignore）：
#   播放器运行时/
#     lib/libmpv.so.2.5.0 (+ libmpv.so.2 / libmpv.so 符号链接)
#     lib/libmpv.so.* → 来自 vendor/prebuilt/linux-x86_64/lib/
#     include/mpv/*.h            # 仅 --with-headers（开发用途，发行包可略）
#     LICENSES/                  # assets/licenses/* + 本构建许可证（LGPL/GPL/Copyright）
#     BUILDINFO.txt              # 本次打包信息 + 校验结果 + 源构建信息
#     DEPENDENCIES.txt           # 动态依赖清单：soname → 系统路径 → 所属发行版包
#     README.txt                 # 运行/替换/依赖说明
#     源码获取说明.txt            # LGPL 源码获取方式（发行合规必需）
#
# 两种模式：
#   system-deps（默认）：libmpv 动态链接系统 FFmpeg/libass/libplacebo/libmujs…
#                       发行包**不随附**这些库；生成系统依赖包清单供用户安装。
#   self-contained     ：把依赖 .so 一并放进 lib/ 并设 rpath $ORIGIN。依赖来源优先级：
#                         ① --deps-dir；② prebuilt/lib 内已有依赖；③ **自动从 libmpv 的
#                         ldd 收集全部非 glibc 库**（M6 默认路径）。自动模式会做合规校验：
#                         libav* 必须来自自建 LGPL FFmpeg（vendor/build/ffmpeg-lgpl）、
#                         打包集合不得含 GPL-only 组件（x264/x265/rubberband/dvd*）；
#                         GPU/驱动栈（GL/EGL/GBM/DRM/VA/CUDA）与 glibc 保持系统提供。
#                       prebuilt 默认优先 vendor/prebuilt/linux-x86_64-lgpl（LGPL 版）。
#
# 用法：
#   ./scripts/package-player-runtime-linux.sh                    # 系统依赖模式打包
#   ./scripts/package-player-runtime-linux.sh --verify-only      # 只校验现有 播放器运行时/
#   ./scripts/package-player-runtime-linux.sh --self-contained   # 自包含模式（需依赖库）
#   ./scripts/package-player-runtime-linux.sh --out /tmp/rt      # 指定输出目录
#   ./scripts/package-player-runtime-linux.sh --with-headers     # 附带头文件（开发用）
#   ./scripts/package-player-runtime-linux.sh --prebuilt <dir>   # 指定 prebuilt 目录
#   ./scripts/package-player-runtime-linux.sh --deps-dir <dir>   # 自包含模式的依赖 .so 来源
#
# 校验：sha256（与源 BUILDINFO 比对）、readelf -d（SONAME/NEEDED）、file（ELF 架构）、
#       ldd（缺失依赖）、符号链接、许可证完整性。任何一项失败 → 退出码 1。
#
# 与 scripts/build-linux.sh 的对接：`build-linux.sh --portable` 会把本脚本生成的
# `播放器运行时/` 整目录拷进 `出/网盘管理-linux-x64/播放器运行时/`；也可用 --out 直接
# 输出到便携目录内的 播放器运行时/。
set -euo pipefail
export LC_ALL=C

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREBUILT="$ROOT/vendor/prebuilt/linux-x86_64"
OUT="$ROOT/播放器运行时"
PREBUILT_EXPLICIT=0
ASSETS_LICENSES="$ROOT/assets/licenses"
MODE="system"
VERIFY_ONLY=0
WITH_HEADERS=0
DEPS_DIR=""
RUNTIME_NAME="播放器运行时"
SCRIPT_NAME="$(basename "${BASH_SOURCE[0]}")"

# 发行包必须随附的许可证文件（assets/licenses 内）
REQUIRED_LICENSES=(
  LGPL-2.1.txt
  GPL-2.0.txt
  mpv-Copyright.txt
  libass-ISC.txt
  FFmpeg-LGPL-2.1.txt
  libplacebo-LGPL-2.1.txt
  libbluray-LGPL-2.1.txt
  libaacs-LGPL-2.1.txt
  libbdplus-LGPL-2.1.txt
  ANGLE-BSD-3-Clause.txt
)

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '  \033[1;32m✓\033[0m %s\n' "$*"; }
warn() { printf '  \033[1;33m!\033[0m %s\n' "$*" >&2; }
fail() { printf '  \033[1;31m✗\033[0m %s\n' "$*" >&2; }
die()  { fail "$*"; exit 1; }

usage() {
  sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --system-deps) MODE="system" ;;
    --self-contained) MODE="self-contained" ;;
    --verify-only) VERIFY_ONLY=1 ;;
    --with-headers) WITH_HEADERS=1 ;;
    --prebuilt) PREBUILT="${2:?--prebuilt 需要目录}"; PREBUILT_EXPLICIT=1; shift ;;
    --out) OUT="${2:?--out 需要目录}"; shift ;;
    --deps-dir) DEPS_DIR="${2:?--deps-dir 需要目录}"; shift ;;
    -h|--help) usage 0 ;;
    *) echo "未知参数：$1（-h 看用法）" >&2; exit 2 ;;
  esac
  shift
done

# M6 合规：self-contained 模式优先用「自建 LGPL FFmpeg 重链」的 prebuilt；
# system-deps 模式必须用与系统库 ABI 匹配的构建（默认 M0 prebuilt），否则 ldd 会缺 libav*.62。
if [ "$PREBUILT_EXPLICIT" = 0 ] && [ "$MODE" = "self-contained" ] \
   && [ -f "$ROOT/vendor/prebuilt/linux-x86_64-lgpl/lib/libmpv.so.2.5.0" ]; then
  PREBUILT="$ROOT/vendor/prebuilt/linux-x86_64-lgpl"
fi
# 输出目录安全校验：拒绝空/根/仓库根
case "$OUT" in
  ""|"/"|"$ROOT"|"$ROOT/") die "拒绝的输出目录：$OUT" ;;
esac
[ "$VERIFY_ONLY" = 1 ] || [ "$MODE" = "self-contained" ] || [ "$MODE" = "system" ] || die "未知模式 $MODE"

# ── 依赖命令 ───────────────────────────────────────────────
have() { command -v "$1" >/dev/null 2>&1; }
have sha256sum || die "缺少 sha256sum（coreutils）"
have readelf   || die "缺少 readelf（binutils）"
have file      || die "缺少 file"
have ldd       || die "缺少 ldd（glibc）"

# ── 工具函数 ───────────────────────────────────────────────
sha256_of() { sha256sum "$1" | awk '{print $1}'; }

# 从源 BUILDINFO 取字段（第二个冒号后的值，trim）
buildinfo_field() {
  local key="$1" file="$2"
  grep -m1 "^${key}:" "$file" 2>/dev/null | sed -E "s/^${key}:[[:space:]]*//" | tr -d '\r' || true
}

# 查询某个系统路径属于哪个发行版包（Arch/Debian 双后端；查不到输出空）
package_of_path() {
  local path="$1" out=""
  if have pacman; then
    out="$(pacman -Qo "$path" 2>/dev/null | sed -E 's/.* is owned by ([^ ]+) (.*)/\1 \2/' || true)"
  elif have dpkg; then
    out="$(dpkg -S "$path" 2>/dev/null | head -1 | cut -d: -f1 || true)"
  fi
  printf '%s' "$out"
}

# 系统基础库（glibc/GCC 运行时，发行版必装，不算「额外依赖包」）
is_base_dep() {
  case "$1" in
    linux-vdso.so.*|ld-linux*.so.*|libc.so.*|libm.so.*|libmvec.so.*|libpthread.so.*|librt.so.*|libdl.so.*|libresolv.so.*|libutil.so.*|libnsl.so.*|libanl.so.*|libBrokenLocale.so.*|libthread_db.so.*)
      return 0 ;;
    libstdc++.so.*|libgcc_s.so.*)
      return 0 ;;
  esac
  return 1
}

# ldd 输出 → 依赖三元组列表（soname / path / status）
collect_deps() {
  local lib="$1"
  ldd "$lib" 2>/dev/null | awk '
    /=> not found/            { print $1 "\t-|<not found>" ; next }
    /=> \//                   { print $1 "\t" $3 ; next }
    /^\//                     { n=split($1,a,"/"); print a[n] "\t" $1 ; next }
  ' | sed 's/[[:space:]]*$//'
}

# ── 取 prebuilt / 源构建信息 ────────────────────────────────
if [ "$VERIFY_ONLY" = 0 ]; then
  [ -d "$PREBUILT" ] || die "prebuilt 目录不存在：$PREBUILT（先跑 scripts/build-media-stack-linux.sh）"
  [ -f "$PREBUILT/BUILDINFO.txt" ] || die "缺少 $PREBUILT/BUILDINFO.txt"
  SRC_LIB="$PREBUILT/lib/libmpv.so.2.5.0"
  [ -f "$SRC_LIB" ] || die "缺少 $SRC_LIB"
fi

say "打包模式：$MODE -> $OUT"
if [ "$VERIFY_ONLY" = 0 ]; then
  echo "    源 prebuilt：$PREBUILT"
fi

# ── verify：对运行时目录做全量校验 ─────────────────────────
VERIFY_ERRORS=0
verify_runtime() {
  local dir="$1" expected_sha="$2" label="${3:-运行时}"
  VERIFY_ERRORS=0
  local libfile="$dir/lib/libmpv.so.2.5.0"
  [ -f "$libfile" ] || { fail "$label：缺少 lib/libmpv.so.2.5.0"; VERIFY_ERRORS=$((VERIFY_ERRORS+1)); return; }

  # file / ELF 架构
  if file -b "$libfile" | grep -q "ELF 64-bit LSB shared object, x86-64"; then
    ok "$label：ELF 64-bit x86-64"
  else
    fail "$label：不是 ELF 64-bit x86-64：$(file -b "$libfile" | cut -c1-80)"; VERIFY_ERRORS=$((VERIFY_ERRORS+1))
  fi

  # sha256
  if [ -n "$expected_sha" ]; then
    local actual; actual="$(sha256_of "$libfile")"
    if [ "$actual" = "$expected_sha" ]; then
      ok "$label：sha256 与源 BUILDINFO 一致（${actual:0:16}…）"
    else
      fail "$label：sha256 不一致（期望 ${expected_sha:0:16}… 实际 ${actual:0:16}…）"; VERIFY_ERRORS=$((VERIFY_ERRORS+1))
    fi
  fi

  # readelf SONAME
  local soname; soname="$(readelf -d "$libfile" 2>/dev/null | sed -n 's/.*(SONAME).*\[\(.*\)\]/\1/p' | head -1)"
  if [ "$soname" = "libmpv.so.2" ]; then
    ok "$label：SONAME = libmpv.so.2"
  else
    fail "$label：SONAME 异常（$soname）"; VERIFY_ERRORS=$((VERIFY_ERRORS+1))
  fi

  # 符号链接
  if [ -L "$dir/lib/libmpv.so.2" ] && [ "$(readlink -f "$dir/lib/libmpv.so.2")" = "$(readlink -f "$libfile")" ]; then
    ok "$label：符号链接 libmpv.so.2 → libmpv.so.2.5.0"
  else
    fail "$label：libmpv.so.2 符号链接缺失或指向错误"; VERIFY_ERRORS=$((VERIFY_ERRORS+1))
  fi
  if [ -L "$dir/lib/libmpv.so" ]; then
    ok "$label：符号链接 libmpv.so → libmpv.so.2"
  else
    warn "$label：缺少 libmpv.so 符号链接（仅影响编译期链接，运行不受影响）"
  fi

  # ldd 缺失依赖
  local missing
  missing="$(collect_deps "$libfile" | awk -F'\t' '$2 ~ /not found/ {print $1}')"
  if [ -z "$missing" ]; then
    ok "$label：ldd 依赖全部可解析"
  else
    if [ "$MODE" = "self-contained" ]; then
      # 自包含模式：用打包的 lib/ 校验
      missing="$(LD_LIBRARY_PATH="$dir/lib" ldd "$libfile" 2>/dev/null | awk '/=> not found/ {print $1}')"
      if [ -z "$missing" ]; then
        ok "$label：自包含依赖全部可解析（LD_LIBRARY_PATH=lib/）"
      else
        fail "$label：自包含缺失依赖：$(echo "$missing" | tr '\n' ' ')"; VERIFY_ERRORS=$((VERIFY_ERRORS+1))
      fi
    else
      fail "$label：ldd 有缺失依赖：$(echo "$missing" | tr '\n' ' ')"; VERIFY_ERRORS=$((VERIFY_ERRORS+1))
    fi
  fi

  # 许可证完整性
  local miss_lic=""
  for lic in "${REQUIRED_LICENSES[@]}"; do
    [ -f "$dir/LICENSES/$lic" ] || miss_lic="$miss_lic $lic"
  done
  if [ -z "$miss_lic" ]; then
    ok "$label：LICENSES 完整（${#REQUIRED_LICENSES[@]} 份）"
  else
    fail "$label：LICENSES 缺失：$miss_lic"; VERIFY_ERRORS=$((VERIFY_ERRORS+1))
  fi

  # 合规说明文件
  [ -f "$dir/源码获取说明.txt" ] && ok "$label：源码获取说明.txt 存在" \
    || { fail "$label：缺 源码获取说明.txt（LGPL 合规必需）"; VERIFY_ERRORS=$((VERIFY_ERRORS+1)); }
  [ -f "$dir/DEPENDENCIES.txt" ] && ok "$label：DEPENDENCIES.txt 存在" \
    || warn "$label：缺 DEPENDENCIES.txt（系统依赖模式建议保留）"
}

# ── verify-only：只校验 ────────────────────────────────────
if [ "$VERIFY_ONLY" = 1 ]; then
  [ -d "$OUT" ] || die "运行时目录不存在：$OUT（先跑 $SCRIPT_NAME）"
  SRC_BI="$OUT/BUILDINFO.txt"
  EXPECTED=""
  [ -f "$SRC_BI" ] && EXPECTED="$(buildinfo_field "libmpv_sha256" "$SRC_BI")"
  [ -n "$EXPECTED" ] || warn "BUILDINFO.txt 缺少 libmpv_sha256 字段，跳过 sha256 比对"
  say "校验运行时：$OUT"
  verify_runtime "$OUT" "$EXPECTED" "播放器运行时"
  echo ""
  if [ "$VERIFY_ERRORS" = 0 ]; then
    say "校验通过（0 错误）"
    exit 0
  fi
  die "校验失败：$VERIFY_ERRORS 项"
fi

# ── 清理并创建目录 ─────────────────────────────────────────
if [ -d "$OUT" ]; then
  say "清理旧产物：$OUT"
  rm -rf "$OUT"
fi
mkdir -p "$OUT/lib" "$OUT/LICENSES"
[ "$WITH_HEADERS" = 1 ] && mkdir -p "$OUT/include/mpv"

# ── 拷贝 libmpv + 符号链接 ─────────────────────────────────
say "拷贝 libmpv"
cp "$SRC_LIB" "$OUT/lib/libmpv.so.2.5.0"
ln -s libmpv.so.2.5.0 "$OUT/lib/libmpv.so.2"
ln -s libmpv.so.2 "$OUT/lib/libmpv.so"
ok "lib/libmpv.so.2.5.0（$(du -h "$OUT/lib/libmpv.so.2.5.0" | cut -f1)）+ 符号链接"

if [ "$WITH_HEADERS" = 1 ]; then
  [ -d "$PREBUILT/include/mpv" ] || warn "prebuilt 无 include/mpv，跳过头文件"
  [ -d "$PREBUILT/include/mpv" ] && cp "$PREBUILT"/include/mpv/*.h "$OUT/include/mpv/" 2>/dev/null || true
  [ -f "$PREBUILT/lib/mpv.pc" ] && cp "$PREBUILT/lib/mpv.pc" "$OUT/lib/mpv.pc" || true
  ok "已附带 include/mpv + mpv.pc（开发用途）"
fi

# ── 许可证 ─────────────────────────────────────────────────
say "整理许可证"
if [ -d "$ASSETS_LICENSES" ]; then
  cp -f "$ASSETS_LICENSES"/*.txt "$OUT/LICENSES/" 2>/dev/null || true
  cp -f "$ASSETS_LICENSES/README.md" "$OUT/LICENSES/" 2>/dev/null || true
  ok "LICENSES/ 来自 assets/licenses（$(ls "$OUT/LICENSES" | wc -l) 个文件）"
else
  warn "未找到 assets/licenses/，退回复制 prebuilt/licenses"
  cp -f "$PREBUILT/licenses/"* "$OUT/LICENSES/" 2>/dev/null || true
fi

# ── 自包含模式：依赖库 + rpath ─────────────────────────────
EXTRA_LIBS=0
if [ "$MODE" = "self-contained" ]; then
  say "自包含模式：收集依赖库"
  SRC_DEPS=()
  if [ -n "$DEPS_DIR" ]; then
    [ -d "$DEPS_DIR" ] || die "--deps-dir 不存在：$DEPS_DIR"
    while IFS= read -r f; do SRC_DEPS+=("$f"); done < <(find "$DEPS_DIR" -maxdepth 1 -name '*.so*' -type f 2>/dev/null)
  fi
  while IFS= read -r f; do
    case "$(basename "$f")" in libmpv*) continue ;; esac
    SRC_DEPS+=("$f")
  done < <(find "$PREBUILT/lib" -maxdepth 1 -name '*.so*' -type f 2>/dev/null)

  # 自包含依赖自动收集：从 libmpv 的 ldd 结果里挑非 glibc / 非 GPU 驱动栈的库
  #（ldd 已展开传递闭包；GPU/驱动栈必须匹配用户硬件，保持系统提供）。
  AUTO_COLLECT=0
  SKIPPED_SYSTEM=()
  DEP_ORIGIN_FILE="$(mktemp)"
  is_selfcontained_skip() {
    case "$1" in
      linux-vdso*|ld-linux*|libc.so*|libm.so*|libmvec.so*|libpthread.so*|librt.so*|libdl.so*|libresolv.so*|libutil.so*|libnsl.so*|libanl.so*|libthread_db.so*|libgcc_s.so*|libstdc++.so*)
        return 0 ;;
      libGL.so*|libGLX*|libGLdispatch*|libOpenGL*|libEGL*|libgbm*|libdrm*|libva.so*|libva-*|libvdpau*|libcuda*|libnvidia*|libnvcuvid*)
        return 0 ;;
    esac
    return 1
  }
  DEP_SEARCH_PATH="$ROOT/vendor/build/ffmpeg-lgpl/lib:$PREBUILT/lib"
  if [ "${#SRC_DEPS[@]}" = 0 ]; then
    AUTO_COLLECT=1
    say "未提供 --deps-dir / prebuilt 依赖 → 自动从 ldd 收集（跳过 glibc 与 GPU 驱动栈）"
    say "依赖搜索路径：$DEP_SEARCH_PATH"
    while IFS=$'\t' read -r soname dpath; do
      [ -n "${soname:-}" ] || continue
      case "${dpath:-}" in
        *"<not found>"*|"") die "自动收集失败：依赖 $soname 未解析（libmpv 运行期依赖不完整）" ;;
      esac
      base="$(basename "$dpath")"
      if is_selfcontained_skip "$base"; then SKIPPED_SYSTEM+=("$base"); continue; fi
      real="$(readlink -f "$dpath")"
      [ -f "$real" ] || die "依赖实体不存在：$dpath"
      SRC_DEPS+=("$real")
      printf '%s\t%s\n' "$(basename "$real")" "$dpath" >> "$DEP_ORIGIN_FILE"
    done < <(LD_LIBRARY_PATH="$DEP_SEARCH_PATH" collect_deps "$SRC_LIB")
    if [ "${#SKIPPED_SYSTEM[@]}" -gt 0 ]; then
      ok "系统提供（不打包）：$(printf '%s\n' "${SKIPPED_SYSTEM[@]}" | sort -u | tr '\n' ' ')"
    fi
  fi
  declare -A _copied=()
  for f in "${SRC_DEPS[@]}"; do
    [ -f "$f" ] || continue
    base="$(basename "$f")"
    [ -n "${_copied[$base]:-}" ] && continue
    _copied[$base]=1
    cp -f "$f" "$OUT/lib/$base" || die "拷贝依赖失败：$f"
    # 按 SONAME 建符号链接（NEEDED 用 SONAME 查找；真实文件名可能带完整版本号）
    soname="$(readelf -d "$f" 2>/dev/null | sed -n 's/.*(SONAME).*\[\(.*\)\]/\1/p' | head -1)"
    if [ -n "$soname" ] && [ "$soname" != "$base" ] && [ ! -e "$OUT/lib/$soname" ]; then
      ln -sfn "$base" "$OUT/lib/$soname"
    fi
    EXTRA_LIBS=$((EXTRA_LIBS+1))
  done
  ok "已拷入 $EXTRA_LIBS 个依赖 .so（自含）"

  # —— 绝对路径 NEEDED 修正（如系统 libmujs.so 无 SONAME 时 mpv NEEDED 会写成 /usr/lib/x.so）——
  # 自包含要求改为 basename + 随包文件，否则运行期永远从系统绝对路径加载。
  SC_ABS_FIXED=0
  for f in "$OUT/lib"/*.so*; do
    [ -f "$f" ] || continue
    while IFS= read -r abs; do
      [ -n "$abs" ] || continue
      base="$(basename "$abs")"
      if [ ! -e "$OUT/lib/$base" ] && [ -f "$abs" ]; then
        cp -f "$abs" "$OUT/lib/$base" || true
        EXTRA_LIBS=$((EXTRA_LIBS+1))
      fi
      if [ -e "$OUT/lib/$base" ] && have patchelf; then
        patchelf --replace-needed "$abs" "$base" "$f" 2>/dev/null && SC_ABS_FIXED=$((SC_ABS_FIXED+1))
      fi
    done < <(readelf -d "$f" 2>/dev/null | sed -n 's/.*(NEEDED).*\[\(\/.*\)\]/\1/p')
  done
  if [ "$SC_ABS_FIXED" -gt 0 ]; then ok "已修正 $SC_ABS_FIXED 个绝对路径 NEEDED → 随包 basename"; fi

  # —— 合规校验（自动收集模式）：FFmpeg 必须来自自建 LGPL 目录；禁 GPL-only 组件 ——
  if [ "$AUTO_COLLECT" = 1 ]; then
    say "自包含合规校验"
    SC_BAD=0
    SC_FFMPEG_SEEN=0
    declare -A _av_checked=()
    for f in "$OUT/lib"/libav*.so.* "$OUT/lib"/libsw*.so.*; do
      [ -f "$f" ] || continue
      # 符号链接与实体文件会重复出现：按「实体文件名」去重并查 origin
      real="$(readlink -f "$f")"
      base="$(basename "$real")"
      [ -n "${_av_checked[$base]:-}" ] && continue
      _av_checked[$base]=1
      origin="$(awk -F'\t' -v b="$base" '$1==b{print $2; exit}' "$DEP_ORIGIN_FILE" 2>/dev/null)"
      origin="${origin:-未知}"
      case "$origin" in
        "$ROOT/vendor/build/ffmpeg-lgpl/"*) SC_FFMPEG_SEEN=$((SC_FFMPEG_SEEN+1)) ;;
        *) fail "FFmpeg 库非自建 LGPL 构建：$base ← $origin"; SC_BAD=1 ;;
      esac
    done
    if [ "$SC_FFMPEG_SEEN" -gt 0 ]; then ok "FFmpeg 库 $SC_FFMPEG_SEEN 个来自自建 LGPL：vendor/build/ffmpeg-lgpl"; fi
    FORBIDDEN_RE='libx264|libx265|libx262|libfdk-aac|librubberband|libdvdread|libdvdnav|libdvdcss'
    SC_FORBIDDEN="$(printf '%s\n' "${SRC_DEPS[@]:-}" | grep -E "$FORBIDDEN_RE" || true)"
    if [ -n "$SC_FORBIDDEN" ]; then
      fail "打包集合含 GPL-only/nonfree 组件："; printf '%s\n' "$SC_FORBIDDEN" | sed 's/^/      /' >&2; SC_BAD=1
    else
      ok "无 GPL-only 组件（x264/x265/fdk-aac/rubberband/dvd*）"
    fi
    rm -f "$DEP_ORIGIN_FILE"
    [ "$SC_BAD" = 0 ] || die "自包含合规校验失败（见上）；请用 scripts/build-media-stack-linux.sh --lgpl-ffmpeg 重链 libmpv"
  fi

  if have patchelf; then
    while IFS= read -r f; do
      patchelf --set-rpath '$ORIGIN' "$f" 2>/dev/null || warn "patchelf 设置 rpath 失败：$f"
    done < <(find "$OUT/lib" -maxdepth 1 -name '*.so*' -type f)
    ok "已为 lib/ 下库设置 rpath \$ORIGIN"
  else
    warn "未安装 patchelf：无法设置 rpath \$ORIGIN（自包含发行必需；Arch: pacman -S patchelf）"
    warn "可手动：patchelf --set-rpath '\$ORIGIN' $OUT/lib/libmpv.so.2.5.0"
  fi
fi

# ── 依赖清单 DEPENDENCIES.txt ──────────────────────────────
say "生成依赖清单 DEPENDENCIES.txt"
DEP_FILE="$OUT/DEPENDENCIES.txt"
PKG_LIST=()
TOTAL_DEPS=0
MISSING_DEPS=0
{
  echo "# 播放器运行时动态依赖清单（由 $SCRIPT_NAME 生成）"
  echo "# 模式：$MODE"
  echo "# 生成时间：$(date '+%Y-%m-%d %H:%M:%S %z')"
  echo "#"
  echo "# 说明："
  echo "#   system-deps 模式：libmpv 动态链接下列系统库，发行包不随附，需目标机器已安装对应发行版包。"
  echo "#   self-contained 模式：依赖已随包放入 lib/，本表仅作记录（运行不依赖系统同款库）。"
  echo "#"
  echo "# soname	系统路径	所属发行版包	类别"
  if [ "$MODE" = "self-contained" ] && [ "$EXTRA_LIBS" -gt 0 ]; then
    DEPS_TEXT="$(LD_LIBRARY_PATH="$OUT/lib" ldd "$OUT/lib/libmpv.so.2.5.0" 2>/dev/null || true)"
  else
    DEPS_TEXT="$(ldd "$OUT/lib/libmpv.so.2.5.0" 2>/dev/null || true)"
  fi
  printf '%s\n' "$DEPS_TEXT" | while IFS= read -r line; do
    [ -z "$line" ] && continue
    case "$line" in
      *"=> not found"*)
        soname="$(printf '%s' "$line" | awk '{print $1}')"
        echo "$soname	-	<缺失>	❌ 未解析"
        ;;
      *"=> /"*)
        soname="$(printf '%s' "$line" | awk '{print $1}')"
        path="$(printf '%s' "$line" | awk '{print $3}')"
        pkg="$(package_of_path "$path")"
        if is_base_dep "$soname"; then
          echo "$soname	$path	${pkg:-<基础>}	系统基础"
        elif [ -n "$pkg" ]; then
          echo "$soname	$path	$pkg	需要安装"
        else
          echo "$soname	$path	-	第三方（未能映射到包）"
        fi
        ;;
      /*)
        soname="$(basename "$line")"
        echo "$soname	$line	$(package_of_path "$line")	系统基础/加载器"
        ;;
    esac
  done
  echo ""
  echo "# ── 需要安装的发行版包（去重；Arch 可用）────────────────────────────"
  DEPS_FOR_PKGS="$(ldd "$OUT/lib/libmpv.so.2.5.0" 2>/dev/null || true)"
  printf '%s\n' "$DEPS_FOR_PKGS" | awk -F'=> ' '/=> \//{print $2}' | awk '{print $1}' | while read -r p; do
    [ -z "$p" ] && continue
    s="$(printf '%s' "$p" | xargs -r basename)"
    if ! is_base_dep "$s"; then
      package_of_path "$p" | awk '{print $1}'
    fi
  done | sort -u | sed 's/^/  - /'
  echo ""
  echo "# ── 生成依赖清单后的自检（脚本 verify 会重跑）──────────────────────"
  echo "#   Arch:  sudo pacman -S --needed <上面列出的包>"
  echo "#   Debian/Ubuntu: sudo apt install <对应 -dev/运行库包>（soname 映射不同）"
} > "$DEP_FILE"

TOTAL_DEPS="$(grep -cE '^[a-zA-Z0-9_.+-]+\.so' "$DEP_FILE" 2>/dev/null || true)"
MISSING_DEPS="$(grep -c '❌ 未解析' "$DEP_FILE" 2>/dev/null || true)"
PKG_LIST=($(sed -n '/# ── 需要安装的发行版包/,/# ── 生成依赖清单/p' "$DEP_FILE" | grep -E '^  - ' | sed 's/^  - //' | sort -u))
ok "依赖 $TOTAL_DEPS 项，未解析 $MISSING_DEPS 项，需安装发行版包 ${#PKG_LIST[@]} 个"
[ "${#PKG_LIST[@]}" -gt 0 ] && echo "    包清单：${PKG_LIST[*]}"

# ── 源码获取说明（LGPL 合规）──────────────────────────────
say "生成源码获取说明"
MPV_VERSION="$(buildinfo_field "mpv_version" "$PREBUILT/BUILDINFO.txt")"
BUILT_AT="$(buildinfo_field "built_at" "$PREBUILT/BUILDINFO.txt")"
DEP_VERSIONS=""
if have pacman; then
  for p in ffmpeg libass libplacebo libbluray libaacs libbdplus; do
    v="$(pacman -Q "$p" 2>/dev/null | awk '{print $2}' || true)"
    [ -n "$v" ] && DEP_VERSIONS="${DEP_VERSIONS}  $p $v"$'\n'
  done
fi
cat > "$OUT/源码获取说明.txt" <<EOF
网盘管理 · 播放器运行时 · 对应源码获取说明（LGPL 合规）
========================================================
生成时间：$(date '+%Y-%m-%d %H:%M:%S %z')
生成脚本：scripts/$SCRIPT_NAME

一、libmpv（LGPL-2.1+）
  版本：${MPV_VERSION:-unknown}
  构建时间：${BUILT_AT:-unknown}
  构建选项：-Dgpl=false -Dlibmpv=true -Dcplayer=false（release）
  上游源码：https://github.com/mpv-player/mpv/archive/refs/tags/v${MPV_VERSION:-0.41.0}.tar.gz
  本项目构建脚本：scripts/build-media-stack-linux.sh
  对 mpv 的修改：无（使用上游源码直接构建；本项目对 glutin-winit 的补丁不进入 libmpv）
  构建产物校验：见 BUILDINFO.txt（sha256 / SONAME / ldd）

二、动态链接依赖（system-deps 模式下不随包分发）
  FFmpeg / libass / libplacebo / libbluray / libmujs 等由目标机器的发行版提供；
  各库源码可从对应上游项目获取（FFmpeg: https://ffmpeg.org/download.html 等）。
  本机（Arch）构建时依赖版本：
${DEP_VERSIONS:-  （本机未安装 pacman，版本未知）}
  ⚠️ 正式对外发行（尤其自包含模式）必须使用 LGPL 构建的 FFmpeg 等依赖，或随包提供
     对应源码/书面提供方式；详见 docs/PLAYER_PACKAGING.md §4/§5。

三、LGPL 书面提供方式
  本项目按 LGPL-2.1 §6 提供对应源码获取方式：以上游官方源码地址 + 本项目构建脚本为准；
  若发行方对发行包做书面承诺，可在发行页面/README 中注明「可提供对应源码，有效期 3 年」。
  第三方许可证全文见 LICENSES/。

四、可替换性
  libmpv 以动态库形式随包（lib/libmpv.so.2.5.0），用户可自行替换为兼容 ABI（libmpv.so.2）
  的构建；应用通过运行时动态加载（WP_MPV_LIB 环境变量或 播放器运行时/lib/ 路径），
  无需重新编译本程序。详见 README.txt。
EOF
ok "源码获取说明.txt"

# ── README.txt ─────────────────────────────────────────────
cat > "$OUT/README.txt" <<EOF
网盘管理 · 播放器运行时（Linux x86_64）
======================================
生成：$(date '+%Y-%m-%d %H:%M:%S %z')　模式：$MODE
结构：lib/libmpv.so.2.5.0（+ libmpv.so.2 符号链接）、LICENSES/、BUILDINFO.txt、
      DEPENDENCIES.txt、源码获取说明.txt$( [ "$WITH_HEADERS" = 1 ] && echo '、include/mpv（开发用）')

使用：本目录放在可执行文件「网盘管理」同级即可被自动发现
      （加载顺序：WP_MPV_LIB > 系统 libmpv > 本目录/lib/libmpv.so.2）。
      也可显式指定：WP_MPV_LIB=<本目录>/lib/libmpv.so.2 ./网盘管理

许可证：mpv 核心为 LGPL-2.1+（本项目 -Dgpl=false 构建）；第三方许可证见 LICENSES/；
        源码获取方式见 源码获取说明.txt。

依赖：$([ "$MODE" = "system" ] && echo '本包不随附 FFmpeg/libass/libplacebo 等，需系统已安装；清单见 DEPENDENCIES.txt' || echo '依赖已随包放入 lib/（自包含模式）')
EOF
ok "README.txt"

# ── 校验（sha256/readelf/file/ldd/许可证）───────────────────
EXPECTED_SHA="$(buildinfo_field "sha256" "$PREBUILT/BUILDINFO.txt")"
[ -n "$EXPECTED_SHA" ] || warn "源 BUILDINFO 无 sha256 字段，跳过哈希比对"
SOURCE_LIBMPV_SHA="$EXPECTED_SHA"
if [ "$MODE" = "self-contained" ]; then
  # 自包含模式会对 libmpv 执行 patchelf --set-rpath $ORIGIN（ELF 被改写）→ sha256 变化是预期；
  # 校验基准改为「本运行时 BUILDINFO 记录的打包后 sha」，另记源 sha 供追溯。
  EXPECTED_SHA="$(sha256_of "$OUT/lib/libmpv.so.2.5.0")"
  ok "自包含模式：libmpv 已 patchelf（rpath=\$ORIGIN），按打包后 sha 校验"
fi

# 先写 BUILDINFO（verify 前写入基础内容，校验结果随后追加）
MANIFEST_SHA_LIST="$(cd "$OUT/LICENSES" && sha256sum *.txt 2>/dev/null | sort || true)"
{
  echo "# 播放器运行时 BUILDINFO（Linux x86_64）"
  echo "# 由 scripts/$SCRIPT_NAME 生成"
  echo "generated_at:  $(date '+%Y-%m-%dT%H:%M:%S%z')"
  echo "package_mode:  $MODE"
  echo "runtime_name:  $RUNTIME_NAME"
  echo "source_prebuilt: ${PREBUILT#"$ROOT/"}"
  echo "libmpv_sha256:  $(sha256_of "$OUT/lib/libmpv.so.2.5.0")"
  echo "libmpv_source_sha256: ${SOURCE_LIBMPV_SHA:-unknown}"
  echo "libmpv_soname:  $(readelf -d "$OUT/lib/libmpv.so.2.5.0" | sed -n 's/.*(SONAME).*\[\(.*\)\]/\1/p' | head -1)"
  echo "libmpv_size:    $(stat -c %s "$OUT/lib/libmpv.so.2.5.0")"
  echo "libmpv_file:    $(file -b "$OUT/lib/libmpv.so.2.5.0" | cut -c1-100)"
  echo "headers:        $([ "$WITH_HEADERS" = 1 ] && echo included || echo omitted)"
  echo "dependency_mode: $MODE"
  echo "dependency_count: $TOTAL_DEPS"
  echo "dependency_missing: $MISSING_DEPS"
  echo "self_contained_libs: ${EXTRA_LIBS:-0}"
  echo "gpl_only_components: none"
  if grep -q 'LGPL build' "$PREBUILT/BUILDINFO.txt" 2>/dev/null; then
    echo "ffmpeg_build: LGPL（自建，见 src 行 ffmpeg:）"
  fi
  if [ "${EXTRA_LIBS:-0}" -gt 0 ]; then
    echo ""
    echo "# ── 自包含依赖 sha256 ──"
    ( cd "$OUT/lib" && for f in *.so.*; do [ -f "$f" ] && [ ! -L "$f" ] && sha256sum "$f"; done | sort | sed 's/^/  /' ) || true
    echo ""
    echo "# ── 体积 ──"
    echo "  runtime_total_bytes: $(du -sb "$OUT" | awk '{print $1}')"
    echo "  runtime_total_human: $(du -sh "$OUT" | awk '{print $1}')"
    echo "  lib_dir_human:       $(du -sh "$OUT/lib" | awk '{print $1}')"
  fi
  echo "required_packages: ${PKG_LIST[*]:-}"
  echo ""
  echo "# ── 源构建信息（vendor/prebuilt/linux-x86_64/BUILDINFO.txt 摘录）──"
  grep -E "^(built_at|mpv_version|builder|build_type|soname|size_bytes|sha256):" "$PREBUILT/BUILDINFO.txt" 2>/dev/null | sed 's/^/  src_/' || true
  echo ""
  echo "# ── LICENSES/ 校验和 ──"
  printf '%s\n' "$MANIFEST_SHA_LIST" | sed 's/^/  /'
} > "$OUT/BUILDINFO.txt"

say "校验运行时"
verify_runtime "$OUT" "$EXPECTED_SHA" "播放器运行时"
{
  echo ""
  echo "# ── 校验结果（$(date '+%Y-%m-%d %H:%M:%S')）──"
  if [ "$VERIFY_ERRORS" = 0 ]; then echo "verify: PASS"; else echo "verify: FAIL ($VERIFY_ERRORS errors)"; fi
} >> "$OUT/BUILDINFO.txt"
[ "$VERIFY_ERRORS" = 0 ] || die "运行时校验失败：$VERIFY_ERRORS 项（见上方输出）"

# ── 汇总 ───────────────────────────────────────────────────
echo ""
say "完成：$OUT"
echo "    结构："
find "$OUT" -maxdepth 2 \( -type f -o -type l \) | sed "s|$OUT/|      |" | sort | head -30
echo "    体积：$(du -sh "$OUT" | cut -f1)"
echo ""
echo "下一步："
echo "  * 随应用分发：./scripts/build-linux.sh --portable   # 会自动拷贝 播放器运行时/"
echo "  * 只校验：    ./scripts/$SCRIPT_NAME --verify-only"
if [ "$MODE" = "system" ]; then
  echo "  * 目标机器依赖：见 $OUT/DEPENDENCIES.txt（Arch: sudo pacman -S --needed ${PKG_LIST[*]:-<见清单>}）"
fi
echo "  * 合规材料：docs/PLAYER_PACKAGING.md + $OUT/LICENSES/ + 源码获取说明.txt"
exit 0
