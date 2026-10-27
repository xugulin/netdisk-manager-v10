#!/usr/bin/env bash
# 验证内置登录内核可用：版本 → 真实启动 → CDP 握手。
#
# 用法：./scripts/verify-login-kernel.sh
# 退出码：0=通过；非 0=失败（打印原因）
# 依赖：curl；无图形环境时自动用 xvfb-run。
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
K="$ROOT/登录内核"

fail() { echo "❌ $*" >&2; exit 1; }

[ -x "$K/chrome" ] || fail "找不到可执行文件 $K/chrome（先跑 ./scripts/fetch-login-chromium.sh）"

VER="$("$K/chrome" --version 2>/dev/null)" || fail "chrome --version 执行失败（缺系统运行库？）"
echo "==> 版本：$VER"

PORT="$(python3 - <<'PY'
import socket
s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()
PY
)"
PROFILE="$(mktemp -d)"
LOGF="$(mktemp)"
cleanup() {
  [ -n "${PID:-}" ] && kill "$PID" 2>/dev/null
  wait 2>/dev/null
  rm -rf "$PROFILE" "$LOGF"
}
trap cleanup EXIT INT TERM

RUN=()
if [ -z "${DISPLAY:-}" ] && command -v xvfb-run >/dev/null 2>&1; then
  RUN=(xvfb-run -a)
fi

echo "==> 启动（调试端口 $PORT）"
"${RUN[@]}" "$K/chrome" \
  --user-data-dir="$PROFILE" \
  --remote-debugging-port="$PORT" \
  --no-first-run --no-default-browser-check \
  about:blank >"$LOGF" 2>&1 &
PID=$!

JSON=""
for _ in $(seq 1 60); do
  if JSON="$(curl -fsS "http://127.0.0.1:$PORT/json/version" 2>/dev/null)"; then
    break
  fi
  if ! kill -0 "$PID" 2>/dev/null; then
    echo "---- chrome 日志 ----" >&2
    tail -20 "$LOGF" >&2
    fail "chrome 启动即退出（见上方日志）"
  fi
  sleep 0.25
done
[ -n "$JSON" ] || { tail -20 "$LOGF" >&2; fail "15 秒内 CDP 端口没有响应"; }

BROWSER="$(printf '%s' "$JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("Browser","?"))' 2>/dev/null || echo "?")"
echo "==> CDP：$BROWSER"
case "$BROWSER" in
  Chrome/*|Chromium/*) ;;
  *) fail "CDP 返回的 Browser 字段异常：$BROWSER" ;;
esac
echo "✅ 登录内核可用：$VER"
