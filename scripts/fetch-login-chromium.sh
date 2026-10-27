#!/usr/bin/env bash
# 下载并精简 Chromium —— 只用于登录窗口的「登录内核」。
#
# 用法：
#   ./scripts/fetch-login-chromium.sh              # 用仓库固定 revision（scripts/login-kernel.rev）
#   ./scripts/fetch-login-chromium.sh latest       # 显式取官方最新（会把最新写进 VERSION.txt）
#   ./scripts/fetch-login-chromium.sh 1728157      # 固定某个 revision（可复现构建）
#
# 产出：<仓库>/登录内核/（chrome + 必需资源，约 250–350 MB）
# 说明：Chromium 本体很大且无法安全裁剪，这已是「能开窗口登录」的最小集合；
#       语言包只保留 zh-CN/en-US，去掉调试符号/宽屏视频插件等。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# 版本解析优先级：命令行参数 > scripts/login-kernel.rev（可复现）> 官方最新
if [ "${1:-}" = "latest" ]; then
  REV="$(curl -fsSL https://storage.googleapis.com/chromium-browser-snapshots/Linux_x64/LAST_CHANGE)"
  echo "==> 取官方最新 Chromium revision：$REV"
elif [ $# -ge 1 ] && [ -n "${1:-}" ]; then
  REV="$1"
elif [ -f "$ROOT/scripts/login-kernel.rev" ]; then
  REV="$(tr -d '[:space:]' < "$ROOT/scripts/login-kernel.rev")"
  echo "==> 使用固定 Chromium revision：$REV（scripts/login-kernel.rev）"
else
  REV="$(curl -fsSL https://storage.googleapis.com/chromium-browser-snapshots/Linux_x64/LAST_CHANGE)"
fi
[ -n "$REV" ] || { echo "❌ 取 Chromium 版本号失败（网络？）" >&2; exit 1; }

URL="https://storage.googleapis.com/chromium-browser-snapshots/Linux_x64/$REV/chrome-linux.zip"
ZIP="$ROOT/.cache/chrome-linux-$REV.zip"
mkdir -p "$ROOT/.cache"

SHA_FILE="$ZIP.sha256"
if [ -s "$ZIP" ] && [ -f "$SHA_FILE" ] && sha256sum -c "$SHA_FILE" >/dev/null 2>&1; then
  echo "==> 复用已下载的 $(basename "$ZIP")（sha256 校验通过）"
else
  echo "==> 下载 Chromium 快照 $REV（约 250 MB）"
  curl -L --retry 5 -C - --progress-bar -o "$ZIP" "$URL"
  sha256sum "$ZIP" > "$SHA_FILE"
  echo "==> 已记录校验和：$(cat "$SHA_FILE")"
fi
[ -s "$ZIP" ] || { echo "❌ 下载失败" >&2; exit 1; }

echo "==> 解包并精简"
TMP="$ROOT/.cache/chrome-unpack-$REV"
rm -rf "$TMP"
mkdir -p "$TMP"
unzip -q "$ZIP" -d "$TMP"
SRC="$TMP/chrome-linux"
[ -x "$SRC/chrome" ] || { echo "❌ 解包后没找到 chrome 可执行文件" >&2; exit 1; }

OUT="$ROOT/登录内核"
rm -rf "$OUT"
mkdir -p "$OUT" "$OUT/locales"

# 只保留登录窗口运行必需的文件
for f in chrome chrome_sandbox chrome_crashpad_handler icudtl.dat v8_context_snapshot.bin \
         libEGL.so libGLESv2.so libvk_swiftshader.so libvulkan.so.1; do
  [ -e "$SRC/$f" ] && cp -a "$SRC/$f" "$OUT/" || true
done
cp -a "$SRC"/*.pak "$OUT/" 2>/dev/null || true
for l in zh-CN en-US; do
  [ -f "$SRC/locales/$l.pak" ] && cp -a "$SRC/locales/$l.pak" "$OUT/locales/" || true
done
chmod +x "$OUT/chrome" 2>/dev/null || true
chmod u+s "$OUT/chrome_sandbox" 2>/dev/null || true

{
  printf 'chromium-snapshot %s\n' "$REV"
  printf 'source %s\n' "$URL"
  printf 'zip-sha256 %s\n' "$(cut -d' ' -f1 "$SHA_FILE" 2>/dev/null)"
} > "$OUT/VERSION.txt"
cat > "$OUT/许可证说明.txt" <<'LIC'
本目录是 Chromium 官方快照的精简副本，仅用于本程序的登录窗口。
Chromium 以 BSD 3-Clause 等开源许可发布，源码与许可全文见 https://www.chromium.org/ .
本程序不修改 Chromium，只裁剪了语言包与调试资源。
LIC
rm -rf "$TMP"

echo "==> 完成：$OUT（$(du -sh "$OUT" | cut -f1)）"
"$OUT/chrome" --version 2>/dev/null || echo "（chrome --version 未能运行，可能是缺少系统 GL/GTK 运行库）"
