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

echo "==> 构建（$MODE）"
if [ "$MODE" = "release" ]; then
  cargo build --release -p wp-gui -p wp-cli
else
  cargo build -p wp-gui -p wp-cli
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
  rm -rf "$OUT"
  mkdir -p "$OUT"
  cp "$BIN" "$OUT/网盘管理"
  [ -x "$CLI_BIN" ] && cp "$CLI_BIN" "$OUT/wp-cli"
  cp -r docs "$OUT/docs" 2>/dev/null || true
  cp README.md "$OUT/README.md" 2>/dev/null || true
  # 图标 / desktop 文件 / 桌面集成安装脚本
  if [ -d assets/icons ]; then
    mkdir -p "$OUT/图标"
    cp assets/icons/netdisk-manager*.png assets/icons/netdisk-manager.svg \
       assets/icons/netdisk-manager.ico assets/icons/preview.png assets/icons/hero.png \
       "$OUT/图标/" 2>/dev/null || true
    cp assets/netdisk-manager.desktop "$OUT/图标/" 2>/dev/null || true
  fi
  cp scripts/install-desktop.sh "$OUT/安装桌面图标.sh" 2>/dev/null && chmod +x "$OUT/安装桌面图标.sh" || true
  # 绿色版标记：数据写在可执行文件旁（U 盘/解压即用）
  : > "$OUT/portable"
  echo "==> 绿色版目录：$OUT"
  echo "    运行：$OUT/网盘管理 --demo"
fi
