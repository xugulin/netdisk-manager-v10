#!/usr/bin/env bash
# 端到端验收：内置登录窗口 → CDP 读取 Cookie → 事件回调 → 适配器导入。
#
# 不需要真账号：用 acceptance 钩子向真内核注入测试 Cookie，只验证「链路能不能走通」。
# 真账号的登录成功/写权限需要人工点一次（见 docs/内置登录-真机验证清单.md）。
#
# 用法：./scripts/verify-login-import.sh
# 退出码：0=链路通；非 0=失败（打印原因）
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

[ -x "登录内核/chrome" ] || { echo "❌ 缺 登录内核/，先跑 ./scripts/fetch-login-chromium.sh" >&2; exit 1; }

echo "==> 构建 acceptance 版"
cargo build -p wp-gui --features acceptance >/dev/null 2>&1 || { echo "❌ 构建失败" >&2; exit 1; }
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug/网盘管理"
[ -x "$BIN" ] || { echo "❌ 找不到 $BIN" >&2; exit 1; }

DATA="$(mktemp -d)"
LOG="$(mktemp)"
cleanup() {
  python3 - "$ROOT" <<'PY' 2>/dev/null
import os, signal, sys
root = sys.argv[1] + '/登录内核/'
for pid in os.listdir('/proc'):
    if not pid.isdigit(): continue
    try: cmd = open(f'/proc/{pid}/cmdline','rb').read().decode('utf-8','ignore').replace('\0',' ')
    except Exception: continue
    if cmd.startswith(root + 'chrome'):
        try: os.kill(int(pid), signal.SIGTERM)
        except Exception: pass
PY
  rm -rf "$DATA" "$LOG"
}
trap cleanup EXIT INT TERM

echo "==> 无头启动 GUI，自动打开内置登录窗口并注入测试 Cookie"
timeout 90 xvfb-run -a env \
  GDK_BACKEND=x11 LIBGL_ALWAYS_SOFTWARE=1 \
  WP_DATA_DIR="$DATA" \
  WP_DEMO_LOGIN_TAB=browser WP_DEMO_BROWSER_LOGIN=1 WP_LOGIN_TEST_COOKIES=1 \
  "$BIN" --dialog login --drive baidu --frames 200 >"$LOG" 2>&1
GUI_RC=$?

APP_LOG="$(ls "$DATA"/日志/*.log 2>/dev/null | tail -1)"
if [ -z "$APP_LOG" ]; then
  echo "❌ 没找到应用日志（GUI 可能没起来，rc=$GUI_RC）" >&2
  tail -20 "$LOG" >&2
  exit 1
fi

inj="$(grep -c "已向内置登录窗口注入测试 Cookie" "$APP_LOG" || true)"
got="$(grep -c "内置登录窗口：取到" "$APP_LOG" || true)"
echo "==> 注入标记：$inj 条；取到会话标记：$got 条"
if [ "$inj" -lt 1 ]; then
  echo "❌ 没有注入标记（验收钩子没生效）" >&2
  grep -E "内置登录|登录内核|错误" "$APP_LOG" | tail -10 >&2
  exit 1
fi
if [ "$got" -lt 1 ]; then
  echo "❌ 没有「取到会话」标记：Cookie 没被读回或事件没到达应用" >&2
  grep -E "内置登录|CDP|错误" "$APP_LOG" | tail -10 >&2
  exit 1
fi

echo "✅ 链路通：内置窗口启动 → 测试 Cookie 注入 → CDP 读回 → 应用收到会话并开始导入"
echo "   （真账号的最终服务端校验/写权限需人工验证，见 docs/内置登录-真机验证清单.md）"
