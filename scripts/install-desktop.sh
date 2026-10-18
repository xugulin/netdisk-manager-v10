#!/usr/bin/env bash
# 网盘管理 — Linux 桌面集成安装脚本
#
# 用法：
#   ./scripts/install-desktop.sh                    # 安装到当前用户（~/.local）
#   ./scripts/install-desktop.sh --prefix /usr/local  # 安装到系统目录（需 sudo）
#   ./scripts/install-desktop.sh --uninstall        # 卸载
#   ./scripts/install-desktop.sh /path/to/网盘管理   # 指定二进制
#
# 安装内容：
#   二进制 → <prefix>/bin/网盘管理
#   图标   → <datadir>/icons/hicolor/{16,24,32,48,64,128,256,512}|scalable/apps/netdisk-manager.*
#   桌面项 → <datadir>/applications/netdisk-manager.desktop
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="网盘管理"
ICON_NAME="netdisk-manager"
PREFIX=""
UNINSTALL=0
BIN_ARG=""

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX="${2:?--prefix 需要目录参数}"; shift 2 ;;
    --prefix=*) PREFIX="${1#--prefix=}"; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) BIN_ARG="$1"; shift ;;
  esac
done

if [ -n "$PREFIX" ]; then
  BIN_DIR="$PREFIX/bin"
  DATA_DIR="$PREFIX/share"
else
  BIN_DIR="$HOME/.local/bin"
  DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
fi
APPS_DIR="$DATA_DIR/applications"
ICONS_DIR="$DATA_DIR/icons/hicolor"

refresh_caches() {
  if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$ICONS_DIR" >/dev/null 2>&1 || true
  fi
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPS_DIR" >/dev/null 2>&1 || true
  fi
}

if [ "$UNINSTALL" = 1 ]; then
  rm -f "$BIN_DIR/$APP_NAME" "$APPS_DIR/$ICON_NAME.desktop"
  find "$ICONS_DIR" -type f -name "$ICON_NAME.*" -delete 2>/dev/null || true
  refresh_caches
  echo "✅ 已卸载网盘管理桌面集成（$BIN_DIR / $APPS_DIR / $ICONS_DIR）"
  exit 0
fi

# ── 找二进制 ──
if [ -z "$BIN_ARG" ]; then
  for cand in "$ROOT/出/网盘管理-linux-x64/网盘管理" "$ROOT/../网盘管理-linux-x64/网盘管理"; do
    [ -x "$cand" ] && BIN_ARG="$cand" && break
  done
fi
if [ -z "$BIN_ARG" ] || [ ! -x "$BIN_ARG" ]; then
  echo "❌ 没找到可执行文件，请显式传入：./scripts/install-desktop.sh <路径/网盘管理>" >&2
  exit 1
fi

echo "→ 安装二进制：$BIN_ARG → $BIN_DIR/$APP_NAME"
install -d "$BIN_DIR"
install -m 755 "$BIN_ARG" "$BIN_DIR/$APP_NAME"

echo "→ 安装图标 → $ICONS_DIR"
for size in 16 24 32 48 64 128 256 512; do
  src="$ROOT/assets/icons/$ICON_NAME-$size.png"
  [ -f "$src" ] || continue
  install -Dm644 "$src" "$ICONS_DIR/${size}x${size}/apps/$ICON_NAME.png"
done
[ -f "$ROOT/assets/icons/$ICON_NAME.svg" ] && \
  install -Dm644 "$ROOT/assets/icons/$ICON_NAME.svg" "$ICONS_DIR/scalable/apps/$ICON_NAME.svg"

echo "→ 安装桌面项：$APPS_DIR/$ICON_NAME.desktop"
install -d "$APPS_DIR"
sed "s|^Exec=.*|Exec=$BIN_DIR/$APP_NAME %U|" "$ROOT/assets/$ICON_NAME.desktop" \
  > "$APPS_DIR/$ICON_NAME.desktop"
chmod 644 "$APPS_DIR/$ICON_NAME.desktop"

refresh_caches
echo
echo "✅ 安装完成！"
echo "   · 应用菜单里搜索「网盘管理」即可启动"
echo "   · 命令行：$BIN_DIR/$APP_NAME"
echo "   · 卸载：./scripts/install-desktop.sh --uninstall${PREFIX:+ --prefix $PREFIX}"
if ! printf '%s' ":$PATH:" | grep -q ":$BIN_DIR:"; then
  echo "   ⚠ $BIN_DIR 不在 PATH 中，可把下面一行加进 ~/.bashrc："
  echo "     export PATH=\"$BIN_DIR:\$PATH\""
fi
