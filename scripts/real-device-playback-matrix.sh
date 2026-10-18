#!/usr/bin/env bash
# 三盘真机播放矩阵（百度 / 夸克 / 光鸭）—— wp-cli 取链 → mpv 播放 + seek → 证据落地。
#
# 只读：仅调用 direct_link / probe-range 与 HTTPS GET/播放，不对网盘做任何写操作。
# 默认 dry-run（只打印将执行的命令，不联网）；加 --run 才真正执行。
#
# 证据目录默认：出/真机联调数据/日志/播放矩阵/<日期-时刻>/
#   <case>.probe-range.json         probe-range 结果（含 head/first/mid Range 状态、header 名单；不含 Cookie 值）
#   <case>.link.sanitized.json      脱敏后的 DirectLink（url 去掉签名 query、header 值只留长度）
#   <case>.mpv-play.log             播放（前 N 帧）mpv 输出
#   <case>.mpv-seek.log             seek 后播放 mpv 输出
#   <case>.hls-check.json           HLS 专有：m3u8 状态 / 首个分片 Range 状态
#   <case>.result.txt               单用例汇总（命令/结果/PASS-FAIL）
#   matrix.tsv                      全用例汇总表
#
# 不落真实 Cookie：原始 link JSON 只放在 mktemp 私有临时目录（trap 清理）；
# 证据目录里的 URL 去掉 query 签名，header 值只记录长度。
#
# 用法：
#   scripts/real-device-playback-matrix.sh                # dry-run（默认）
#   scripts/real-device-playback-matrix.sh --run          # 真跑全部用例
#   scripts/real-device-playback-matrix.sh --run --only quark-large-hls
#   scripts/real-device-playback-matrix.sh --list         # 列出用例
#   scripts/real-device-playback-matrix.sh --run --out /tmp/matrix
#
# 依赖：bash、python3、mpv、xvfb-run；wp-cli 二进制（target/debug/wp-cli）或 cargo。
# 环境变量：WP_MATRIX_FRAMES（默认 5）、WP_CLI_BIN（覆盖 wp-cli 路径）。
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DATA_DIR="$ROOT/出/真机联调数据"
OUT_DIR=""
RUN=0
ONLY=""
LIST=0
FRAMES="${WP_MATRIX_FRAMES:-5}"

# 用例：name|drive|remote_path|expect(plain|hls)|seek_seconds|policy(strict|tolerate)
#   tolerate：允许该用例失败（记录为 WARN，不计入失败数）——例如百度 >50MB 文件 CDN 侧 403（账号限制）
# 注：均来自 2026-10-19 真机会话实测可访问的素材（见 docs/PLAYER_REAL_DEVICE_MATRIX.md）。
CASES=(
  "baidu-small|baidu|/来自：V2408A/DCIM/1074058976_1734538851561.mp4|plain|2|strict"
  "baidu-large|baidu|/来自：V2408A/DCIM/Camera/video_20250722_184451.mp4|plain|15|strict"
  "baidu-over50-403|baidu|/来自：V2408A/DCIM/Camera/video_20250714_182340.mp4|plain|30|tolerate"
  "quark-small|quark|/模拟器游戏/pc端全能模拟器/29元完美版免安装，自带34T游戏库/29米一键安装整合包版本介绍.mp4|plain|2|strict"
  "quark-large-hls|quark|/video_20260903_171534.mp4|hls|60|strict"
  "guangya-mkv|guangya|/来自：分享/东京复仇者/东京复仇者.S01E25.2021.1080p.mkv|plain|120|strict"
)

usage() {
  sed -n '2,32p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --run) RUN=1 ;;
    --dry-run) RUN=0 ;;
    --list) LIST=1 ;;
    --only) ONLY="${2:?--only 需要用例名}"; shift ;;
    --out) OUT_DIR="${2:?--out 需要目录}"; shift ;;
    --data-dir) DATA_DIR="${2:?--data-dir 需要目录}"; shift ;;
    --frames) FRAMES="${2:?--frames 需要数字}"; shift ;;
    -h|--help) usage 0 ;;
    *) echo "未知参数：$1（-h 看用法）" >&2; exit 2 ;;
  esac
  shift
done

if [ "$LIST" = 1 ]; then
  echo "用例："
  for c in "${CASES[@]}"; do IFS='|' read -r n d p e s pol <<<"$c"; printf '  %-18s %-8s seek=%-4s expect=%-5s policy=%-8s %s\n' "$n" "$d" "$s" "$e" "${pol:-strict}" "$p"; done
  exit 0
fi

if [ ! -d "$DATA_DIR" ]; then echo "数据目录不存在：$DATA_DIR" >&2; exit 2; fi
STAMP="$(date +%Y%m%d-%H%M%S)"
[ -n "$OUT_DIR" ] || OUT_DIR="$DATA_DIR/日志/播放矩阵/$STAMP"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/wp-matrix.XXXXXX")"
umask 077
trap 'rm -rf "$TMP"' EXIT

# ── wp-cli 定位（目标目录可能被 CARGO_TARGET_DIR 重定向）────────
resolve_wp_cli() {
  if [ -n "${WP_CLI_BIN:-}" ]; then echo "$WP_CLI_BIN"; return; fi
  local td="" c=""
  if [ -x "$ROOT/target/debug/wp-cli" ]; then echo "$ROOT/target/debug/wp-cli"; return; fi
  if [ -n "${CARGO_TARGET_DIR:-}" ]; then td="$CARGO_TARGET_DIR"
  elif c="$(cd "$ROOT" && cargo metadata --no-deps --format-version 1 2>/dev/null)"; then
    td="$(printf '%s' "$c" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("target_directory",""))' 2>/dev/null)"
  fi
  if [ -n "$td" ] && [ -x "$td/debug/wp-cli" ]; then echo "$td/debug/wp-cli"; return; fi
  echo ""
}
WP_BIN="$(resolve_wp_cli)"
if [ "$RUN" = 1 ] && [ -z "$WP_BIN" ]; then
  echo "[matrix] 未找到 wp-cli 二进制，cargo build -p wp-cli ..."
  ( cd "$ROOT" && cargo build -q -p wp-cli ) || { echo "构建 wp-cli 失败" >&2; exit 2; }
  WP_BIN="$(resolve_wp_cli)"
fi
if [ -n "$WP_BIN" ]; then
  WP=( "$WP_BIN" --data-dir "$DATA_DIR" )
else
  WP=( cargo run -q -p wp-cli -- --data-dir "$DATA_DIR" )
fi

# ── 内嵌 python 助手（避免依赖 jq）────────────────────────────
cat > "$TMP/parse_link.py" <<'PY'
import json, sys
from urllib.parse import urlsplit, urlunsplit

raw_path, out_dir = sys.argv[1], sys.argv[2]
raw = open(raw_path, encoding="utf-8").read()
start = raw.find("{")
if start < 0:
    print("ERR 不是 JSON"); sys.exit(3)
d = json.loads(raw[start:])
url = d.get("url") or ""
headers = [(str(k), str(v)) for k, v in (d.get("headers") or [])]

host = urlsplit(url).netloc if url else ""
sanitized_url = ""
if url:
    p = urlsplit(url)
    path = p.path if len(p.path) <= 80 else p.path[:77] + "..."
    sanitized_url = urlunsplit((p.scheme, p.netloc, path, "", ""))

sensitive = {"cookie", "authorization", "proxy-authorization", "token", "x-token", "set-cookie"}
san_headers = []
ua = ""
hdr_lines = []
for k, v in headers:
    lk = k.lower()
    san_headers.append({"name": k, "value_redacted": True, "value_len": len(v)} if lk in sensitive else {"name": k, "value": v})
    if lk == "user-agent":
        ua = v
    else:
        hdr_lines.append(k + "\t" + v)
open(out_dir + "/url", "w", encoding="utf-8").write(url + "\n")
open(out_dir + "/ua", "w", encoding="utf-8").write(ua + "\n")
open(out_dir + "/hdr.tsv", "w", encoding="utf-8").write("\n".join(hdr_lines) + ("\n" if hdr_lines else ""))
json.dump({"url": sanitized_url, "url_host": host, "is_hls": bool(d.get("is_hls")),
           "expire_at": d.get("expire_at"), "size": d.get("size"),
           "file_name": d.get("file_name"), "headers": san_headers},
          open(out_dir + "/link.sanitized.json", "w", encoding="utf-8"),
          ensure_ascii=False, indent=1)
has_cookie = any(k.lower() == "cookie" for k, _ in headers)
print("is_hls=%s size=%s expire_at=%s url_host=%s n_headers=%d has_cookie=%s"
      % (d.get("is_hls"), d.get("size"), d.get("expire_at"), host, len(headers), has_cookie))
PY

cat > "$TMP/range_summary.py" <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1], encoding="utf-8"))
except Exception as e:
    print("probe-range 解析失败：%s" % e); sys.exit(3)
def g(k, f):
    return (d.get(k) or {}).get(f)
print("url_host=%s size=%s is_hls=%s header_names=%s head=%s first=%s mid=%s"
      % (d.get("url_host"), d.get("size"), d.get("is_hls"),
         ",".join(d.get("header_names") or []),
         g("head", "status"), g("first", "status"), g("mid", "status")))
ok = bool(g("first", "ok")) and str(g("first", "status")) in ("206", "200")
sys.exit(0 if ok else 4)
PY

cat > "$TMP/hls_check.py" <<'PY'
import json, sys, urllib.request, urllib.error
from urllib.parse import urljoin

url, hdr_tsv, ua = sys.argv[1], sys.argv[2], sys.argv[3]
headers = {}
for line in open(hdr_tsv, encoding="utf-8"):
    line = line.rstrip("\n")
    if "\t" in line:
        k, v = line.split("\t", 1)
        headers[k] = v
if ua:
    headers["User-Agent"] = ua

def fetch(u, extra=None):
    h = dict(headers)
    if extra:
        h.update(extra)
    req = urllib.request.Request(u, headers=h)
    try:
        r = urllib.request.urlopen(req, timeout=60)
        return r.status, r.read(), dict(r.headers)
    except urllib.error.HTTPError as e:
        return e.code, b"", dict(e.headers)

st, body, rh = fetch(url)
out = {"m3u8_url_host": url.split("/")[2], "m3u8_status": st, "m3u8_bytes": len(body),
       "content_type": rh.get("Content-Type"), "segments": 0, "seg_status": None,
       "content_range": None, "seg_url_host": None}
if st == 200:
    lines = [l.strip() for l in body.decode("utf-8", "replace").splitlines()
             if l.strip() and not l.startswith("#")]
    out["segments"] = len(lines)
    if lines:
        seg = urljoin(url, lines[0])
        out["seg_url_host"] = seg.split("/")[2]
        sst, _, sh = fetch(seg, {"Range": "bytes=0-1023"})
        out["seg_status"] = sst
        out["content_range"] = sh.get("Content-Range")
json.dump(out, sys.stdout, ensure_ascii=False, indent=1)
print()
ok = out["m3u8_status"] == 200 and out["seg_status"] in (200, 206)
sys.exit(0 if ok else 4)
PY

echo "[matrix] data-dir = $DATA_DIR"
echo "[matrix] 证据目录 = $OUT_DIR"
echo "[matrix] 模式 = $([ "$RUN" = 1 ] && echo RUN || echo DRY-RUN)  帧数 = $FRAMES"
mkdir -p "$OUT_DIR"
TSV="$OUT_DIR/matrix.tsv"
if [ "$RUN" = 1 ] && [ ! -s "$TSV" ]; then
  printf 'case\tdrive\tfile\tsize\texpect\tis_hls\tplay\tseek\trange_or_hls\tresult\tnote\n' > "$TSV"
fi

FAILED=0
SELECTED=0
for case_def in "${CASES[@]}"; do
  IFS='|' read -r NAME DRIVE PATH_REMOTE EXPECT SEEK POLICY <<<"$case_def"
  [ -n "$POLICY" ] || POLICY="strict"
  [ -n "$ONLY" ] && [ "$ONLY" != "$NAME" ] && continue
  SELECTED=$((SELECTED + 1))
  CASE_TMP="$TMP/$NAME"; mkdir -p "$CASE_TMP"
  BPATH="$OUT_DIR/$NAME"
  echo ""
  echo "════════ $NAME（$DRIVE）════════"
  echo "  文件: $PATH_REMOTE"
  echo "  预期: $EXPECT 链路 / seek=${SEEK}s / policy=$POLICY"

  if [ "$RUN" = 0 ]; then
    echo "  [dry-run] $WP_BIN --data-dir \"$DATA_DIR\" --drive $DRIVE link \"$PATH_REMOTE\" > <raw-link.json>"
    echo "  [dry-run] $WP_BIN --data-dir \"$DATA_DIR\" --drive $DRIVE probe-range \"$PATH_REMOTE\" > $BPATH.probe-range.json"
    echo "  [dry-run] 解析 DirectLink → (is_hls, url, headers, UA)"
    if [ "$EXPECT" = "hls" ]; then
      echo "  [dry-run] curl -H 'Cookie: <from-link>' -H 'Referer: <from-link>' \"<m3u8 url>\"   # 期望 200"
      echo "  [dry-run] curl -H ... -r 0-1023 \"<首个 .ts>\"                                  # 期望 206"
    fi
    echo "  [dry-run] xvfb-run -a mpv --no-config --vo=null --ao=null --frames=$FRAMES --user-agent=<UA> \\"
    echo "              --http-header-fields='Cookie: <from-link>' --http-header-fields-add='Referer: <from-link>' \"<url>\""
    echo "  [dry-run] 同上 + --start=$SEEK   # seek 验证"
    continue
  fi

  # 1) 取直链
  RAW="$CASE_TMP/link.raw.json"
  if ! "${WP[@]}" --drive "$DRIVE" link "$PATH_REMOTE" >"$RAW" 2>"$CASE_TMP/link.err"; then
    REASON="取直链失败：$(head -c 300 "$CASE_TMP/link.err" | tr '\n' ' ')"
    echo "  ✗ $REASON"
    FAILED=$((FAILED + 1))
    printf '%s\t%s\t%s\t-\t%s\t-\tFAIL\tFAIL\t-\tFAIL\t%s\n' "$NAME" "$DRIVE" "$PATH_REMOTE" "$EXPECT" "$REASON" >> "$TSV"
    { echo "case: $NAME"; echo "result: FAIL"; echo "reason: $REASON"; } > "$BPATH.result.txt"
    continue
  fi
  SUMMARY="$(python3 "$TMP/parse_link.py" "$RAW" "$CASE_TMP" 2>&1)"; PARSE_RC=$?
  cp "$CASE_TMP/link.sanitized.json" "$BPATH.link.sanitized.json" 2>/dev/null
  if [ "$PARSE_RC" != 0 ]; then
    echo "  ✗ DirectLink 解析失败：$SUMMARY"
    FAILED=$((FAILED + 1)); continue
  fi
  URL="$(head -1 "$CASE_TMP/url")"; UA="$(head -1 "$CASE_TMP/ua")"
  IS_HLS="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["is_hls"])' "$CASE_TMP/link.sanitized.json")"
  SIZE="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["size"])' "$CASE_TMP/link.sanitized.json")"
  echo "  link: $SUMMARY"

  # 2) probe-range 证据（plain/HLS 都跑；HLS 时 head/first 可能不适用，仅作记录）
  RANGE_SUMMARY="-"
  if "${WP[@]}" --drive "$DRIVE" probe-range "$PATH_REMOTE" >"$BPATH.probe-range.json" 2>"$CASE_TMP/range.err"; then
    RANGE_SUMMARY="$(python3 "$TMP/range_summary.py" "$BPATH.probe-range.json" 2>&1)"
    echo "  range: $RANGE_SUMMARY"
  else
    RANGE_SUMMARY="probe-range 失败：$(head -c 200 "$CASE_TMP/range.err" | tr '\n' ' ')"
    echo "  range: $RANGE_SUMMARY"
  fi

  # 3) HLS 专有检查
  HLS_SUMMARY="-"; HLS_RC=0
  if [ "$IS_HLS" = "True" ] || [ "$EXPECT" = "hls" ]; then
    HLS_OUT="$(python3 "$TMP/hls_check.py" "$URL" "$CASE_TMP/hdr.tsv" "$UA" 2>&1)"; HLS_RC=$?
    printf '%s\n' "$HLS_OUT" > "$BPATH.hls-check.json"
    HLS_SUMMARY="$(printf '%s' "$HLS_OUT" | python3 -c 'import json,sys
try:
    d=json.load(sys.stdin)
    print("m3u8=%s bytes=%s segs=%s seg=%s cr=%s" % (d["m3u8_status"],d["m3u8_bytes"],d["segments"],d["seg_status"],d["content_range"]))
except Exception as e:
    print("hls-check 解析失败:",e)' 2>&1)"
    echo "  hls: $HLS_SUMMARY"
  fi

  # 4) mpv 播放（前 N 帧）
  mapfile -t HDR_LINES < "$CASE_TMP/hdr.tsv"
  HDR_ARGS=(); HI=0; DROPPED_HDRS=""
  for line in "${HDR_LINES[@]}"; do
    [ -z "$line" ] && continue
    hname="${line%%$'\t'*}"; hval="${line#*$'\t'}"
    # mpv 的 --http-header-fields 是 String list（逗号分隔）：值里含逗号会被拆碎，
    # 实测会产生畸形头并让上游返回 400（百度）。含逗号的头（多为 Accept-Language
    # 之类非关键头）跳过；关键头（Cookie/Referer/Authorization）含逗号则告警。
    case "$hval" in
      *,*)
        DROPPED_HDRS="${DROPPED_HDRS:+$DROPPED_HDRS, }$hname"
        case "$hname" in Cookie|Referer|Authorization|user-agent|User-Agent)
          echo "  [warn] 关键头 $hname 的值含逗号，mpv CLI 无法原样传递（将由播放器侧代理处理）" ;;
        esac
        continue ;;
    esac
    if [ "$HI" = 0 ]; then HDR_ARGS+=(--http-header-fields="$hname: $hval")
    else HDR_ARGS+=(--http-header-fields-add="$hname: $hval"); fi
    HI=$((HI + 1))
  done
  [ -n "$DROPPED_HDRS" ] && echo "  [matrix] 注入 mpv 时跳过含逗号头：$DROPPED_HDRS（不影响播放）"
  MPV_BASE=(--no-config --vo=null --ao=null --frames="$FRAMES")
  [ -n "$UA" ] && MPV_BASE+=(--user-agent="$UA")
  PLAY_LOG="$BPATH.mpv-play.log"
  timeout 240 xvfb-run -a mpv "${MPV_BASE[@]}" "${HDR_ARGS[@]}" "$URL" >"$PLAY_LOG" 2>&1
  PLAY_RC=$?
  PLAY_VIDEO="$(grep -m1 -F "● Video" "$PLAY_LOG" 2>/dev/null || true)"
  PLAY_AUDIO="$(grep -m1 -F "● Audio" "$PLAY_LOG" 2>/dev/null || true)"
  PLAY_VO="$(grep -m1 -F "VO: [null]" "$PLAY_LOG" 2>/dev/null || true)"
  PLAY_EOF="$(grep -m1 -F "Exiting... (End of file)" "$PLAY_LOG" 2>/dev/null || true)"
  if [ "$PLAY_RC" = 0 ] && [ -n "$PLAY_VIDEO" ]; then PLAY_OK=1; else PLAY_OK=0; fi
  PLAY_AUDIO_TXT=""
  [ -n "$PLAY_AUDIO" ] && PLAY_AUDIO_TXT=" / $PLAY_AUDIO"
  echo "  play: rc=$PLAY_RC ${PLAY_VIDEO:-（无 Video 行）/ -}$PLAY_AUDIO_TXT"
  [ -n "$PLAY_EOF" ] || echo "  play: 未见正常结束行（可能错误）：$(grep -m3 -iE 'error|failed|412|403' "$PLAY_LOG" 2>/dev/null | tr '\n' ' ')"
  echo "  play: $PLAY_VIDEO" >> "$BPATH.result.txt"

  # 5) mpv seek
  SEEK_LOG="$BPATH.mpv-seek.log"
  timeout 240 xvfb-run -a mpv "${MPV_BASE[@]}" "${HDR_ARGS[@]}" --start="$SEEK" "$URL" >"$SEEK_LOG" 2>&1
  SEEK_RC=$?
  SEEK_VIDEO="$(grep -m1 -F "● Video" "$SEEK_LOG" 2>/dev/null || true)"
  SEEK_VO="$(grep -m1 -F "VO: [null]" "$SEEK_LOG" 2>/dev/null || true)"
  if [ "$SEEK_RC" = 0 ] && [ -n "$SEEK_VIDEO" ] && [ -n "$SEEK_VO" ]; then SEEK_OK=1; else SEEK_OK=0; fi
  echo "  seek($SEEK s): rc=$SEEK_RC ${SEEK_VIDEO:-（无 Video 行）/-}"
  [ "$SEEK_OK" = 1 ] || echo "  seek: 失败详情：$(grep -m3 -iE 'error|failed|412|403|end of file' "$SEEK_LOG" 2>/dev/null | tr '\n' ' ')"

  # 6) 结果汇总
  RESULT="PASS"; NOTE=""
  if [ "$PLAY_OK" != 1 ]; then RESULT="FAIL"; NOTE="播放失败 rc=$PLAY_RC"; fi
  if [ "$SEEK_OK" != 1 ]; then RESULT="FAIL"; NOTE="${NOTE:+$NOTE; }seek 失败 rc=$SEEK_RC"; fi
  if [ "$EXPECT" = "plain" ] && [ "$IS_HLS" = "True" ]; then RESULT="FAIL"; NOTE="${NOTE:+$NOTE; }预期普通直链却返回 is_hls=true"; fi
  if [ "$EXPECT" = "hls" ] && [ "$IS_HLS" != "True" ]; then RESULT="FAIL"; NOTE="${NOTE:+$NOTE; }预期 HLS 却是 is_hls=false"; fi
  if [ "$IS_HLS" = "True" ] && [ "$HLS_RC" != 0 ]; then RESULT="FAIL"; NOTE="${NOTE:+$NOTE; }HLS m3u8/分片检查失败"; fi
  if [ "$RESULT" = "FAIL" ] && [ "$POLICY" = "tolerate" ]; then
    RESULT="WARN"
    NOTE="${NOTE:+$NOTE; }预期失败已记录（tolerate）"
  fi
  echo "  => $RESULT ${NOTE:+（$NOTE）}"
  [ "$RESULT" = "FAIL" ] && FAILED=$((FAILED + 1))

  RANGE_OR_HLS="$RANGE_SUMMARY"
  [ "$IS_HLS" = "True" ] && RANGE_OR_HLS="HLS: $HLS_SUMMARY"
  RANGE_OR_HLS="${RANGE_OR_HLS//$'\t'/ }"
  {
    echo "case: $NAME"
    echo "drive: $DRIVE"
    echo "path: $PATH_REMOTE"
    echo "expect: $EXPECT  is_hls: $IS_HLS  size: $SIZE"
    echo "repro:"
    echo "  wp-cli --data-dir \"$DATA_DIR\" --drive $DRIVE link \"$PATH_REMOTE\""
    echo "  mpv --no-config --vo=null --ao=null --frames=$FRAMES --user-agent=<UA> --http-header-fields='Cookie: <from-link>' --http-header-fields-add='Referer: <from-link>' \"<url>\""
    echo "  # seek: 同上 + --start=$SEEK"
    echo "range: $RANGE_SUMMARY"
    echo "hls: $HLS_SUMMARY"
    echo "mpv play rc=$PLAY_RC: $PLAY_VIDEO | $PLAY_AUDIO | $PLAY_VO | $PLAY_EOF"
    echo "mpv seek($SEEK s) rc=$SEEK_RC: $SEEK_VIDEO | $SEEK_VO"
    echo "result: $RESULT  $NOTE"
    echo "evidence: $BPATH.*"
  } > "$BPATH.result.txt"
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$NAME" "$DRIVE" "$PATH_REMOTE" "$SIZE" "$EXPECT" "$IS_HLS" \
    "$([ "$PLAY_OK" = 1 ] && echo OK || echo FAIL)" \
    "$([ "$SEEK_OK" = 1 ] && echo OK || echo FAIL)" \
    "$RANGE_OR_HLS" "$RESULT" "${NOTE//$'\t'/ }" >> "$TSV"
done

if [ "$RUN" = 0 ]; then
  echo ""
  echo "[dry-run] 共 $SELECTED 个用例；加 --run 执行。证据将写入：$OUT_DIR"
  exit 0
fi

echo ""
echo "════════ 矩阵汇总（$OUT_DIR/matrix.tsv）════════"
column -t -s $'\t' "$TSV" 2>/dev/null || cat "$TSV"
WARNED=$(awk -F'\t' 'NR>1 && $10=="WARN"' "$TSV" 2>/dev/null | wc -l)
if [ "$FAILED" = 0 ]; then
  echo "✅ 全部严格用例 PASS（共 $SELECTED 个；tolerate 计 WARN=$WARNED）"
  exit 0
else
  echo "❌ $FAILED 个用例未通过（共 $SELECTED 个；WARN=$WARNED）"
  exit 1
fi
