#!/usr/bin/env bash
# 无头冒烟验收：Xvfb 里启动 GUI、截关键页面/对话框图、断言进程正常退出。
# 用途：CI / 本机没有显示服务器时验证「GUI 能起来、中文能渲染、页面/对话框不崩」。
#
# 依赖：xvfb-run（Debian/Ubuntu: apt install xvfb；Arch: pacman -S xorg-server-xvfb）
# 输出：出/验收截图/*.png
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# ── 品牌资产完整性检查（图标 / desktop / 安装脚本）
for f in assets/icons/netdisk-manager.svg \
         assets/icons/netdisk-manager-small.svg \
         assets/icons/netdisk-manager-256.png \
         assets/icons/netdisk-manager.ico \
         assets/netdisk-manager.desktop \
         scripts/install-desktop.sh; do
  if [ ! -f "$ROOT/$f" ]; then
    echo "❌ 缺少品牌资产：$f" >&2
    exit 1
  fi
done

OUT="$ROOT/出/验收截图"
mkdir -p "$OUT"
rm -f "$OUT"/*.png

BIN="$(python3 - "$ROOT" <<'PY'
import json, subprocess, sys, os
root = sys.argv[1]
meta = json.loads(subprocess.check_output(["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=root))
for mode in ("release", "debug"):
    p = os.path.join(meta["target_directory"], mode, "网盘管理")
    if os.path.exists(p):
        print(p)
        break
PY
)"
if [ -z "$BIN" ]; then
  echo "❌ 找不到可执行文件，先运行 ./scripts/build-linux.sh" >&2
  exit 1
fi
echo "==> 可执行文件：$BIN"

export WP_DATA_DIR="$OUT/数据"
rm -rf "$WP_DATA_DIR"
export LIBGL_ALWAYS_SOFTWARE=1

PASS=0
FAIL=0

# run_case <文件名> <额外参数...>
run_case() {
  local name="$1"; shift
  echo "==> 截图：$name（$*）"
  if xvfb-run -a -s "-screen 0 1440x900x24" \
      "$BIN" "$@" --screenshot "$OUT/$name.png" --frames 24 \
      > "$OUT/$name.log" 2>&1; then
    if [ -f "$OUT/$name.png" ]; then
      echo "    ✅ $(ls -lh "$OUT/$name.png" | awk '{print $5}')"
      PASS=$((PASS+1))
      return
    fi
    echo "    ❌ 截图缺失" >&2
  else
    echo "    ❌ 进程退出异常（见 $OUT/$name.log）" >&2
    tail -5 "$OUT/$name.log" >&2 || true
  fi
  FAIL=$((FAIL+1))
}

# ── 主页面
run_case "01-网盘页"        --drive demo
run_case "02-传输页"        --drive demo --page transfers
run_case "03-设置页"        --drive demo --page settings
run_case "04-敏感词页"      --drive demo --page guard
run_case "05-日志页"        --drive demo --page logs
run_case "06-关于页"        --drive demo --page about
run_case "07-本机浏览"      --drive local
run_case "08-文件列表"      --drive demo --path "/影视/科幻"
# ── 对话框
run_case "09-登录框"        --drive demo --dialog login
run_case "10-新建文件夹"    --drive demo --dialog newfolder
run_case "11-重命名"        --drive demo --dialog rename
run_case "12-删除确认"      --drive demo --dialog delete
run_case "13-移动到"        --drive demo --dialog move
run_case "14-分享"          --drive demo --dialog share
run_case "15-跨盘传输"      --drive demo --dialog crosscopy
run_case "16-文件详情"      --drive demo --dialog details
run_case "17-本地文件选择"  --drive demo --dialog filepicker
# ── 工具页（演示网盘内置演示数据）
run_case "18-工具-回收站"    --drive demo --page tools --tools-tab recycle
run_case "19-工具-云任务"    --drive demo --page tools --tools-tab cloud
run_case "20-工具-上传记录"  --drive demo --page tools --tools-tab records
run_case "21-工具-分享管理"  --drive demo --page tools --tools-tab shares
run_case "22-工具-分类浏览"  --drive demo --page tools --tools-tab categories
run_case "23-网盘-缩略图"    --drive demo --path /影视 --frames 60
run_case "24-播放器"          --drive demo --page player --frames 40

# ── 25：真实 libmpv 播放（可选；有 ffmpeg + libmpv 才跑，保证 smoke 在纯 CI 环境不假失败）
if command -v ffmpeg >/dev/null 2>&1 && { ldconfig -p 2>/dev/null | grep -q libmpv || [ -e "$ROOT/播放器运行时/libmpv.so.2" ]; }; then
  TEST_VIDEO="$(mktemp --suffix=.mp4 /tmp/wp-smoke-video-XXXXXX)"
  if ffmpeg -hide_banner -loglevel error \
      -f lavfi -i "testsrc2=size=1280x720:rate=30" -f lavfi -i "sine=frequency=440" \
      -t 6 -c:v libx264 -pix_fmt yuv420p -preset ultrafast -c:a aac -shortest -y "$TEST_VIDEO" >/dev/null 2>&1; then
    echo "==> 截图：25-真实播放（libmpv 真实帧，media=$(basename "$TEST_VIDEO")）"
    if WP_PLAYER_HWDEC=no WP_SCREENSHOT_FRAME=40 LIBGL_ALWAYS_SOFTWARE=1 \
        xvfb-run -a -s "-screen 0 1440x900x24 +extension GLX" \
        "$BIN" --media "$TEST_VIDEO" --screenshot "$OUT/25-真实播放.png" \
        --frames 160 --wait-ms 12000 > "$OUT/25-真实播放.log" 2>&1 \
        && [ -f "$OUT/25-真实播放.png" ]; then
      echo "    ✅ $(ls -lh "$OUT/25-真实播放.png" | awk '{print $5}')"
      PASS=$((PASS+1))
    else
      echo "    ❌ 真实播放截图失败（见 $OUT/25-真实播放.log）" >&2
      tail -5 "$OUT/25-真实播放.log" >&2 || true
      FAIL=$((FAIL+1))
    fi
  else
    echo "    ⏭ 跳过 25-真实播放：ffmpeg 生成测试片失败" >&2
  fi
  rm -f "$TEST_VIDEO"
else
  echo "==> 跳过 25-真实播放（无 ffmpeg 或 libmpv 不可用）"
fi

# ── 可选：真机网盘播放（真实账号+外网；默认跳过，WP_SMOKE_REAL_NET=1 才跑）
# 注意：需要 WP_DATA_DIR 指向真机会话目录；llvmpipe 下首帧慢，wait-media-ms 给足。
if [ "${WP_SMOKE_REAL_NET:-0}" = "1" ] && [ -d "$ROOT/出/真机联调数据" ]; then
  REAL_DATA="$ROOT/出/真机联调数据"
  real_play_case() { # <名字> <drive> <path> <seek-ms> <wait-ms>
    local name="$1" drive="$2" path="$3" seek="$4" wait="$5"
    echo "==> 截图：$name（真实网盘：$drive $path）"
    if WP_DATA_DIR="$REAL_DATA" WP_PLAYER_HWDEC=no WP_PLAYER_AO_NULL=1 \
        WP_SCREENSHOT_FRAME=60 LIBGL_ALWAYS_SOFTWARE=1 \
        xvfb-run -a -s "-screen 0 1440x900x24 +extension GLX" \
        "$BIN" --play-remote "$drive" "$path" --seek-ms "$seek" \
        --screenshot "$OUT/$name.png" --frames 120 --wait-media-ms "$wait" \
        > "$OUT/$name.log" 2>&1 && [ -f "$OUT/$name.png" ]; then
      echo "    ✅ $(ls -lh "$OUT/$name.png" | awk '{print $5}')"
      PASS=$((PASS+1))
    else
      echo "    ❌ 真实网盘播放失败（见 $OUT/$name.log）" >&2
      FAIL=$((FAIL+1))
    fi
  }
  real_play_case "26-网盘播放-光鸭mkv" guangya '/来自：分享/东京复仇者/东京复仇者.S01E25.2021.1080p.mkv' 120000 45000
  real_play_case "27-网盘播放-夸克HLS" quark '/video_20260903_171534.mp4' 60000 45000
  real_play_case "28-网盘播放-百度不可播提示" baidu '/来自：V2408A/DCIM/Camera/video_20250714_182340.mp4' 0 20000
else
  echo "==> 跳过 26/27/28（真机网盘用例；WP_SMOKE_REAL_NET=1 且存在 出/真机联调数据 才跑）"
fi

echo
echo "==> 结果：通过 $PASS 项，失败 $FAIL 项（截图目录：$OUT）"
if [ "$FAIL" -ne 0 ]; then exit 1; fi
