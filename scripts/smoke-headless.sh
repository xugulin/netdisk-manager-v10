#!/usr/bin/env bash
# 无头冒烟验收：Xvfb 里启动 GUI、截关键页面/对话框图、断言进程正常退出。
# 用途：CI / 本机没有显示服务器时验证「GUI 能起来、中文能渲染、页面/对话框不崩」。
#
# 依赖：xvfb-run（Debian/Ubuntu: apt install xvfb；Arch: pacman -S xorg-server-xvfb）
# 输出：出/验收截图/*.png
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
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

echo
echo "==> 结果：通过 $PASS 项，失败 $FAIL 项（截图目录：$OUT）"
if [ "$FAIL" -ne 0 ]; then exit 1; fi
