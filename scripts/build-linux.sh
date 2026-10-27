#!/usr/bin/env bash
# Linux 构建脚本：产出单文件可执行程序（绿色版）。
#
# 用法：
#   ./scripts/build-linux.sh              # release 构建
#   ./scripts/build-linux.sh --debug      # debug 构建（开发用）
#   ./scripts/build-linux.sh --portable   # 构建后生成绿色版目录 出/网盘管理-linux-x64/
#
# 说明：
# * 本机 CARGO_TARGET_DIR 可能被环境变量重定向；脚本用 `cargo metadata` 解析真实产物路径，
#   不写死 target/release（共享基座经验：产物路径别假设）。
# * 交叉平台无关：构建机只需 rustup + 系统 OpenGL/X11 运行库（运行时需要显卡驱动）。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

MODE="release"
PORTABLE=0
for arg in "$@"; do
  case "$arg" in
    --debug) MODE="debug" ;;
    --portable) PORTABLE=1 ;;
    *) echo "未知参数：$arg" >&2; exit 2 ;;
  esac
done

FEATURES=""

echo "==> 构建（$MODE）"
if [ "$MODE" = "release" ]; then
  cargo build --release -p wp-gui $FEATURES
  cargo build --release -p wp-cli
else
  cargo build -p wp-gui $FEATURES
  cargo build -p wp-cli
fi

# 用 cargo metadata 解析真实产物路径（尊重 CARGO_TARGET_DIR）
BIN="$(python3 - "$ROOT" "$MODE" <<'PY'
import json, subprocess, sys
root, mode = sys.argv[1], sys.argv[2]
meta = json.loads(subprocess.check_output(["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=root))
target_dir = meta["target_directory"]
print(f"{target_dir}/{mode}/网盘管理")
PY
)"
CLI_BIN="$(dirname "$BIN")/wp-cli"
if [ ! -x "$BIN" ]; then
  echo "构建产物不存在：$BIN" >&2
  exit 1
fi
echo "==> 产物：$BIN"
ls -lh "$BIN" "$CLI_BIN" 2>/dev/null || true

if [ "$PORTABLE" = "1" ]; then
  OUT="$ROOT/出/网盘管理-linux-x64"
  # 先打包到临时目录再整体替换：绝不能 rm -rf 掉用户的 数据/（账号/日志/下载，
  # 实测踩过：直接删目录把 2GB 数据连同凭证一起清了，只能从快照恢复）。
  STAGE="$ROOT/出/.网盘管理-linux-x64.打包临时.$$"
  rm -rf "$STAGE"
  mkdir -p "$STAGE"
  # 任何中途失败/退出都清掉临时目录（否则会留 160MB+ 垃圾）
  trap 'rm -rf "$STAGE"' EXIT
  cp "$BIN" "$STAGE/网盘管理"
  [ -x "$CLI_BIN" ] && cp "$CLI_BIN" "$STAGE/wp-cli"
  cp -r docs "$STAGE/docs" 2>/dev/null || true
  cp README.md "$STAGE/README.md" 2>/dev/null || true
  # 图标 / desktop 文件 / 桌面集成安装脚本
  if [ -d assets/icons ]; then
    mkdir -p "$STAGE/图标"
    cp assets/icons/netdisk-manager*.png assets/icons/netdisk-manager.svg \
       assets/icons/netdisk-manager.ico assets/icons/preview.png assets/icons/hero.png \
       "$STAGE/图标/" 2>/dev/null || true
    cp assets/netdisk-manager.desktop "$STAGE/图标/" 2>/dev/null || true
  fi
  cp scripts/install-desktop.sh "$STAGE/安装桌面图标.sh" 2>/dev/null && chmod +x "$STAGE/安装桌面图标.sh" || true
  # 播放器运行时（libmpv 及其依赖；由 scripts/build-media-stack-linux.sh 生成）
  if [ -d "播放器运行时" ]; then
    cp -r "播放器运行时" "$STAGE/播放器运行时"
    echo "==> 已打包播放器运行时（$(du -sh 播放器运行时 | cut -f1)）"
  else
    echo "==> 提示：未找到 播放器运行时/（播放器功能将在启动时提示缺少 libmpv）"
  fi
  # 内置登录内核（只用登录窗口的 Chromium）：绿色版缺内核 = 登录不可用，默认直接报错
  if [ ! -d "登录内核" ] && [ "${WP_ALLOW_NO_KERNEL:-0}" != "1" ]; then
    echo "❌ 缺少 登录内核/：绿色版将无法打开登录窗口。" >&2
    echo "   先执行 ./scripts/fetch-login-chromium.sh 生成；" >&2
    echo "   确实要打「无登录内核」的包时：WP_ALLOW_NO_KERNEL=1 $0 --portable" >&2
    exit 1
  fi
  if [ -d "登录内核" ]; then
    cp -r "登录内核" "$STAGE/登录内核"
    echo "==> 已打包登录内核（$(du -sh 登录内核 | cut -f1)）：$("$STAGE/登录内核/chrome" --version 2>/dev/null || echo '版本未知')"
  else
    echo "==> 提示：未找到 登录内核/，登录窗口不可用；先跑 ./scripts/fetch-login-chromium.sh"
  fi
  # 绿色版标记：数据写在可执行文件旁（U 盘/解压即用）
  : > "$STAGE/portable"
  # 保留用户数据目录（账号/日志/下载/播放器缓存）
  if [ -d "$OUT/数据" ]; then
    mv "$OUT/数据" "$STAGE/数据"
    echo "==> 已保留数据目录（$(du -sh "$STAGE/数据" | cut -f1)）"
  fi
  rm -rf "$OUT"
  mv "$STAGE" "$OUT"
  trap - EXIT
  echo "==> 绿色版目录：$OUT"
  echo "    运行：$OUT/网盘管理 --demo"
fi
