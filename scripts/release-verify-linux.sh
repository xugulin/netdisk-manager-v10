#!/usr/bin/env bash
# 发布验收（Linux x86_64）—— 随包 libmpv 运行时验证；可进 CI。
#
# 做三件事：
#   1) 跑 `scripts/package-player-runtime-linux.sh --verify-only`（打包产物自身校验）；
#   2) 独立复核 播放器运行时/ 的结构 / 许可证 / sha256 / SONAME（不依赖打包脚本的结论）；
#   3) **真实加载 + headless 短播放**：`WP_MPV_LIB=...播放器运行时/lib/libmpv.so.2`（hwdec=no 快速 open）
#      启动 `mpv-probe` 播一段自生成的测试视频，用 strace/LD_DEBUG 证明实际打开的
#      libmpv 是随包库（而不是系统库），并断言版本输出与事件。
#
# 用法：
#   ./scripts/release-verify-linux.sh                    # 全量验收（需已生成 播放器运行时/）
#   ./scripts/release-verify-linux.sh --runtime-dir DIR  # 指定便携目录内的 播放器运行时/
#   ./scripts/release-verify-linux.sh --skip-package-verify   # 跳过第 1 步（CI 分阶段时）
#   ./scripts/release-verify-linux.sh --skip-play             # 只做静态校验（无 ffmpeg 环境）
#   ./scripts/release-verify-linux.sh --keep                  # 保留临时文件
#   ./scripts/release-verify-linux.sh -h
#
# 输出：逐步 PASS/FAIL，证据目录 `出/验收日志/release-verify-<时间戳>/`（gitignore），
#       含 package-verify.log / strace.log / play.json / summary.txt；退出码 0=全 PASS。
#
# 依赖：bash、python3、sha256sum、readelf、file、cargo（定位 mpv-probe）、
#       ffmpeg（生成测试视频，可 --skip-play 跳过）、strace（可选，缺省用 LD_DEBUG 兜底）。
set -uo pipefail
export LC_ALL=C

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME="$ROOT/播放器运行时"
SKIP_PKG=0
SKIP_PLAY=0
KEEP=0
OUT_DIR="${WP_RELEASE_VERIFY_OUT:-$ROOT/出/验收日志/release-verify-$(date +%Y%m%d-%H%M%S)}"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/wp-release-verify.XXXXXX")"

PASS=0
FAIL=0
declare -a RESULTS=()

usage() {
  sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --runtime-dir) RUNTIME="${2:?--runtime-dir 需要目录}"; shift ;;
    --skip-package-verify) SKIP_PKG=1 ;;
    --skip-play) SKIP_PLAY=1 ;;
    --keep) KEEP=1 ;;
    -h|--help) usage 0 ;;
    *) echo "未知参数：$1（-h 看用法）" >&2; exit 2 ;;
  esac
  shift
done

cleanup() { [ "$KEEP" = 1 ] || rm -rf "$TMP"; }
trap cleanup EXIT

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '  \033[1;32m✓\033[0m %s\n' "$*"; PASS=$((PASS + 1)); RESULTS+=("PASS  $*"); }
warn() { printf '  \033[1;33m!\033[0m %s\n' "$*"; }
bad()  { printf '  \033[1;31m✗\033[0m %s\n' "$*"; FAIL=$((FAIL + 1)); RESULTS+=("FAIL  $*"); }
die()  { bad "$*"; summary; exit 1; }

summary() {
  mkdir -p "$OUT_DIR"
  {
    echo "# 发布验收结果（Linux）"
    echo "time: $(date '+%Y-%m-%d %H:%M:%S %z')"
    echo "runtime: $RUNTIME"
    echo "pass: $PASS  fail: $FAIL"
    echo ""
    printf '%s\n' "${RESULTS[@]}"
  } > "$OUT_DIR/summary.txt"
  echo ""
  say "结果：PASS=$PASS FAIL=$FAIL"
  echo "    证据：$OUT_DIR/"
  echo "    明细：$OUT_DIR/summary.txt"
}

need() { command -v "$1" >/dev/null 2>&1; }

# ── 0) 预检 ────────────────────────────────────────────────
say "发布验收开始"
echo "    仓库：$ROOT"
echo "    运行时：$RUNTIME"
mkdir -p "$OUT_DIR"
[ -d "$RUNTIME" ] || die "运行时目录不存在：$RUNTIME（先跑 scripts/package-player-runtime-linux.sh）"
for c in sha256sum readelf file python3; do
  need "$c" || die "缺少命令：$c"
done

BUILDINFO="$RUNTIME/BUILDINFO.txt"
LIB_LINK="$RUNTIME/lib/libmpv.so.2"
LIB="$(readlink -f "$LIB_LINK" 2>/dev/null || true)"
[ -n "$LIB" ] || LIB="$RUNTIME/lib/libmpv.so.2.5.0"
MODE="$(grep -m1 '^package_mode:' "$BUILDINFO" 2>/dev/null | sed -E 's/^package_mode:[[:space:]]*//' || true)"
MODE="${MODE:-unknown}"
echo "    打包模式：$MODE"
[ "$MODE" = "system" ] && echo "    （系统依赖模式：目标机需已安装 DEPENDENCIES.txt 中的包）"

# strace / LD_DEBUG 日志里非 ASCII 路径会被转义成 \ooo（每字节一个八进制转义）；
# 解码后再按绝对路径 grep，避免中文目录名导致「找不到随包库」的假失败。
decode_escaped_paths() { # $1=raw log  $2=decoded output
  python3 - "$1" "$2" <<'PYEOF'
import re, sys
raw = open(sys.argv[1], "rb").read()

def repl(m):
    try:
        return bytes(int(x, 8) for x in re.findall(rb"\\([0-7]{3})", m.group(0))).decode("utf-8").encode("utf-8")
    except (UnicodeDecodeError, ValueError):
        return m.group(0)

out = re.sub(rb"(?:\\[0-7]{3})+", repl, raw)
open(sys.argv[2], "wb").write(out)
PYEOF
}

# ── 1) 打包脚本自身校验 ─────────────────────────────────────
say "[1/4] 打包产物校验（package --verify-only）"
if [ "$SKIP_PKG" = 1 ]; then
  warn "已跳过（--skip-package-verify）"
elif [ -x "$ROOT/scripts/package-player-runtime-linux.sh" ]; then
  if "$ROOT/scripts/package-player-runtime-linux.sh" --verify-only --out "$RUNTIME" \
      > "$OUT_DIR/package-verify.log" 2>&1; then
    ok "package-player-runtime-linux.sh --verify-only 通过"
  else
    bad "package 校验失败（见 $OUT_DIR/package-verify.log）"
  fi
else
  bad "未找到 scripts/package-player-runtime-linux.sh"
fi

# ── 2) 独立复核：结构 / 许可证 / sha256 / SONAME ─────────────
say "[2/4] 独立复核结构 / 许可证 / sha256 / SONAME"
[ -f "$LIB" ] && ok "lib/libmpv.so.2.5.0 存在" || bad "缺 lib/libmpv.so.2.5.0"
if [ -L "$RUNTIME/lib/libmpv.so.2" ] && [ "$(readlink -f "$RUNTIME/lib/libmpv.so.2")" = "$(readlink -f "$LIB")" ]; then
  ok "符号链接 libmpv.so.2 → libmpv.so.2.5.0"
else
  bad "libmpv.so.2 符号链接缺失或指向错误"
fi

EXPECTED_SHA="$(grep -m1 '^libmpv_sha256:' "$BUILDINFO" 2>/dev/null | sed -E 's/^libmpv_sha256:[[:space:]]*//' | tr -d '\r' || true)"
if [ -n "$EXPECTED_SHA" ] && [ -f "$LIB" ]; then
  ACTUAL_SHA="$(sha256sum "$LIB" | awk '{print $1}')"
  if [ "$ACTUAL_SHA" = "$EXPECTED_SHA" ]; then
    ok "sha256 与 BUILDINFO 一致（${ACTUAL_SHA:0:16}…）"
  else
    bad "sha256 不一致（BUILDINFO=${EXPECTED_SHA:0:16}… actual=${ACTUAL_SHA:0:16}…）"
  fi
else
  bad "BUILDINFO 缺 libmpv_sha256 或库文件缺失"
fi

if [ -f "$LIB" ]; then
  SONAME="$(readelf -d "$LIB" 2>/dev/null | sed -n 's/.*(SONAME).*\[\(.*\)\]/\1/p' | head -1)"
  [ "$SONAME" = "libmpv.so.2" ] && ok "readelf -d SONAME = libmpv.so.2" || bad "SONAME 异常：$SONAME"
fi

# 许可证（与打包脚本的必需清单保持一致）
REQ_LIC=(
  LGPL-2.1.txt GPL-2.0.txt mpv-Copyright.txt libass-ISC.txt
  FFmpeg-LGPL-2.1.txt libplacebo-LGPL-2.1.txt libbluray-LGPL-2.1.txt
  libaacs-LGPL-2.1.txt libbdplus-LGPL-2.1.txt ANGLE-BSD-3-Clause.txt
)
MISS_LIC=""
for lic in "${REQ_LIC[@]}"; do
  [ -f "$RUNTIME/LICENSES/$lic" ] || MISS_LIC="$MISS_LIC $lic"
done
[ -z "$MISS_LIC" ] && ok "LICENSES 完整（${#REQ_LIC[@]} 份）" || bad "LICENSES 缺失：$MISS_LIC"

for f in BUILDINFO.txt DEPENDENCIES.txt README.txt 源码获取说明.txt; do
  [ -f "$RUNTIME/$f" ] && ok "文件存在：$f" || bad "缺文件：$f"
done

# 依赖清单可读性（system 模式必须非空；self-contained 模式允许为空但应有说明）
DEP_LINES="$(grep -cE '^[a-zA-Z0-9_.+-]+\.so' "$RUNTIME/DEPENDENCIES.txt" 2>/dev/null || true)"
if [ "$MODE" = "system" ]; then
  [ "${DEP_LINES:-0}" -gt 0 ] && ok "DEPENDENCIES.txt 记录了 $DEP_LINES 项系统依赖" || bad "系统依赖模式但 DEPENDENCIES.txt 无依赖记录"
fi

# ── 3) 定位/构建 mpv-probe ─────────────────────────────────
say "[3/4] 定位 mpv-probe（headless 播放探针）"
PROBE=""
TARGET_DIR=""
if METADATA="$(cd "$ROOT" && cargo metadata --no-deps --format-version 1 2>/dev/null)"; then
  TARGET_DIR="$(printf '%s' "$METADATA" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("target_directory",""))' 2>/dev/null)"
fi
[ -n "$TARGET_DIR" ] || TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
PROBE="$TARGET_DIR/debug/mpv-probe"
# 必须确保 mpv-probe 与当前源码一致（增量构建很快）：否则会用上一版加载顺序做验收（假 PASS）。
say "    构建 mpv-probe（确保与当前源码一致）..."
if (cd "$ROOT" && cargo build -q -p wp-media-mpv --bin mpv-probe > "$OUT_DIR/build-probe.log" 2>&1); then
  [ -x "$PROBE" ] || PROBE="$(find "$TARGET_DIR/debug" -maxdepth 1 -name 'mpv-probe' -type f 2>/dev/null | head -1)"
  ok "mpv-probe 构建完成"
else
  bad "mpv-probe 构建失败（见 $OUT_DIR/build-probe.log）"
fi
if [ -x "$PROBE" ]; then
  ok "mpv-probe：$PROBE"
else
  bad "找不到/构建失败 mpv-probe（$PROBE；见 $OUT_DIR/build-probe.log）"
  summary
  exit 1
fi

# ── 4) 真实加载 + headless 短播放 ──────────────────────────
say "[4/4] 随包 libmpv 真实加载 + headless 短播放"
if [ "$SKIP_PLAY" = 1 ]; then
  warn "已跳过（--skip-play）"
else
  # 4.1 生成 2 秒测试视频（无外部素材依赖）
  VIDEO="$TMP/test-2s.mp4"
  if need ffmpeg; then
    if ! ffmpeg -hide_banner -loglevel error -f lavfi -i "testsrc=size=160x120:rate=10:duration=2" \
        -c:v libx264 -pix_fmt yuv420p -y "$VIDEO" > "$OUT_DIR/ffmpeg.log" 2>&1; then
      ffmpeg -hide_banner -loglevel error -f lavfi -i "testsrc=size=160x120:rate=10:duration=2" \
        -c:v mpeg4 -y "$VIDEO" >> "$OUT_DIR/ffmpeg.log" 2>&1 || true
    fi
  fi
  if [ -s "$VIDEO" ]; then
    ok "测试视频已生成（$(du -h "$VIDEO" | cut -f1)）"
    PLAY_JSON="$OUT_DIR/play.json"
    B_ABS="$(readlink -f "$RUNTIME")"
    LIB_ABS="$B_ABS/lib/libmpv.so.2"
    # 4.2 用 WP_MPV_LIB 显式指向随包库 + LD_LIBRARY_PATH 指向运行时 lib
    : > "$OUT_DIR/strace.log"
    TRACE_MODE=""
    if need strace; then
      TRACE_MODE="strace"
      WP_MPV_LIB="$LIB_ABS" LD_LIBRARY_PATH="$B_ABS/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
        timeout 120 strace -f -e trace=openat -o "$OUT_DIR/strace.log" \
        "$PROBE" "$VIDEO" --seconds 8 --hwdec no --json > "$PLAY_JSON" 2> "$OUT_DIR/play.err"
    else
      TRACE_MODE="LD_DEBUG"
      WP_MPV_LIB="$LIB_ABS" LD_LIBRARY_PATH="$B_ABS/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
        LD_DEBUG=libs timeout 120 "$PROBE" "$VIDEO" --seconds 2 --json \
        > "$PLAY_JSON" 2> "$OUT_DIR/ld-debug.log"
    fi
    PLAY_RC=$?
    if [ "$PLAY_RC" = 0 ]; then
      ok "mpv-probe 退出码 0（headless 短播放完成，strace=$TRACE_MODE）"
    else
      bad "mpv-probe 退出码 $PLAY_RC（见 play.err）"
    fi

    # 4.3 断言 JSON：版本输出 + 事件 + 无打开错误
    PY_CHECK="$(python3 - "$PLAY_JSON" <<'PY'
import json, sys
path = sys.argv[1]
try:
    lines = [l for l in open(path, encoding="utf-8", errors="replace").read().splitlines() if l.strip()]
    d = json.loads(lines[-1])
except Exception as e:
    print(f"JSON_INVALID {e}"); sys.exit(3)
version = str(d.get("mpv_version") or "")
errs = []
if "client API" not in version:
    errs.append(f"mpv_version 不含 client API：{version!r}")
if d.get("open_error"):
    errs.append(f"open_error={d.get('open_error')}")
events = d.get("events") or []
if not any(str(e).startswith("Opened") for e in events):
    errs.append("未观测到 Opened 事件")
print("OK " + (version if version else "?"))
if errs:
    print("ERRORS " + " | ".join(errs)); sys.exit(4)
PY
)"
    if [ "$?" = 0 ]; then
      ok "播放断言：$PY_CHECK"
    else
      bad "播放断言失败：$PY_CHECK"
    fi

    # 4.4 断言实际加载的是随包库（strace/LD_DEBUG 证据；先解码非 ASCII 路径转义）
    LOAD_LOG=""
    if [ -s "$OUT_DIR/strace.log" ]; then
      LOAD_LOG="$OUT_DIR/strace-decoded.log"
      decode_escaped_paths "$OUT_DIR/strace.log" "$LOAD_LOG"
    elif [ -s "$OUT_DIR/ld-debug.log" ]; then
      LOAD_LOG="$OUT_DIR/ld-debug-decoded.log"
      decode_escaped_paths "$OUT_DIR/ld-debug.log" "$LOAD_LOG"
    fi
    if [ -n "$LOAD_LOG" ]; then
      # 4.4a 随包库必须被实际打开（同时匹配符号链接路径与实体 .so.2.5.0）
      if grep -F "$LIB_ABS" "$LOAD_LOG" > "$OUT_DIR/load-evidence.txt" 2>/dev/null \
         || grep -F "$(readlink -f "$LIB_ABS")" "$LOAD_LOG" >> "$OUT_DIR/load-evidence.txt" 2>/dev/null; then
        ok "加载路径证据：实际打开随包库 $LIB_ABS"
      else
        bad "日志中未发现随包库被打开：$LIB_ABS（见 $LOAD_LOG）"
      fi
      # 4.4b 不得打开系统 libmpv
      SYSTEM_LIB_HITS="$(grep -E 'libmpv\.so' "$LOAD_LOG" | grep -F "/usr/lib/libmpv" || true)"
      if [ -z "$SYSTEM_LIB_HITS" ]; then
        ok "未误用系统库（加载日志中无 /usr/lib/libmpv）"
      else
        bad "发现系统库被打开（疑似误用）：$SYSTEM_LIB_HITS"
      fi
      # 4.4c 依赖库应来自系统（system-deps 模式）或随包（self-contained 模式）
      if [ "$MODE" = "self-contained" ]; then
        if grep -E 'libavcodec|libass' "$LOAD_LOG" | grep -qE "$(readlink -f "$RUNTIME")/lib/"; then
          ok "自包含依赖从随包 lib/ 加载"
        else
          bad "自包含模式但依赖库未从随包 lib/ 加载"
        fi
      fi
    else
      warn "无 strace/LD_DEBUG 证据（strace 未安装且 LD_DEBUG 未产出）→ 只验证了退出码与版本"
    fi

    # 4.5 兜底行为（信息性）：不设 WP_MPV_LIB 时应回退系统库
    if [ -e /usr/lib/libmpv.so.2 ]; then
      SYS_JSON="$OUT_DIR/play-system-fallback.json"
      env -u WP_MPV_LIB LD_LIBRARY_PATH="$B_ABS/lib" \
        timeout 120 "$PROBE" "$VIDEO" --seconds 4 --hwdec no --json > "$SYS_JSON" 2>/dev/null || true
      if [ -s "$SYS_JSON" ]; then
        SYS_VER="$(python3 -c 'import json,sys
try:
  print(json.load(open(sys.argv[1])).get("mpv_version",""))
except Exception: print("")' "$SYS_JSON" 2>/dev/null)"
        [ -n "$SYS_VER" ] && ok "系统库兜底可用（不设 WP_MPV_LIB 时加载系统 libmpv：$SYS_VER）" \
          || warn "系统库兜底未产出可解析版本（不影响发布验收）"
      fi
    fi

    # 4.6 发布布局模拟（关键）：把 mpv-probe 拷到「应用目录」，让 播放器运行时/ 与它同级，
    #     不设 WP_MPV_LIB → 应用应通过「exe 相对路径的 播放器运行时/lib/libmpv.so.2」找到随包库。
    APP_DIR="$TMP/app"
    mkdir -p "$APP_DIR"
    cp "$PROBE" "$APP_DIR/mpv-probe"
    ln -s "$(readlink -f "$RUNTIME")" "$APP_DIR/播放器运行时"
    LAYOUT_JSON="$OUT_DIR/play-release-layout.json"
    LAYOUT_TRACE="$OUT_DIR/strace-release-layout.log"
    if env -u WP_MPV_LIB timeout 120 strace -f -e trace=openat -o "$LAYOUT_TRACE" \
        "$APP_DIR/mpv-probe" "$VIDEO" --seconds 4 --hwdec no --json > "$LAYOUT_JSON" 2>/dev/null; then
      LAYOUT_DECODED="$OUT_DIR/strace-release-layout-decoded.log"
      decode_escaped_paths "$LAYOUT_TRACE" "$LAYOUT_DECODED"
      EXPECT_LAYOUT_LIB="$APP_DIR/播放器运行时/lib/libmpv.so.2"
      if grep -F "$EXPECT_LAYOUT_LIB" "$LAYOUT_DECODED" > "$OUT_DIR/load-evidence-release-layout.txt" 2>/dev/null; then
        ok "发布布局模拟：未设 WP_MPV_LIB 时按 exe 相对路径加载随包库（$EXPECT_LAYOUT_LIB）"
      else
        bad "发布布局模拟：未按 exe 相对路径加载随包库（期望 $EXPECT_LAYOUT_LIB）"
      fi
      if grep -E 'libmpv\.so' "$LAYOUT_DECODED" | grep -qF "/usr/lib/libmpv"; then
        bad "发布布局模拟：误用系统 libmpv（未走随包库）"
      else
        ok "发布布局模拟：未回退系统库（随包库优先生效）"
      fi
      LAYOUT_VER="$(python3 -c 'import json,sys
try: print(json.load(open(sys.argv[1])).get("mpv_version",""))
except Exception: print("")' "$LAYOUT_JSON" 2>/dev/null)"
      [ -n "$LAYOUT_VER" ] && ok "发布布局模拟：播放成功（$LAYOUT_VER）" || bad "发布布局模拟：播放未产出 mpv 版本"
    else
      warn "发布布局模拟：启动失败（见 $LAYOUT_TRACE）"
    fi
  else
    bad "无法生成测试视频（缺 ffmpeg；可 --skip-play 跳过动态验证）"
  fi
fi

# ── 汇总 ───────────────────────────────────────────────────
summary
if [ "$FAIL" = 0 ]; then
  echo ""
  echo "✅ 发布验收 PASS（$PASS 项检查）"
  exit 0
else
  echo ""
  echo "❌ 发布验收 FAIL（$FAIL 项失败 / $PASS 项通过）"
  exit 1
fi
