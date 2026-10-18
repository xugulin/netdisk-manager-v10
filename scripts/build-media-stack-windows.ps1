# 构建 vendor media-stack（LGPL libmpv）—— Windows x86_64
#
# 状态：**结构级脚本**（本机 Linux 无法执行/实测 Windows 构建）。默认只打印步骤；
#       加 `-Run` 时在装好 MSYS2 的 Windows 机器上实际执行（CI 也用它）。
#       完成后产物在 vendor/prebuilt/windows-x86_64/，再跑
#       scripts/package-player-runtime-windows.ps1 -Generate -Check。
#
# 目标（与 Linux 侧对齐，见 scripts/build-media-stack-linux.sh）：
#   1. **从源码构建 LGPL 版 FFmpeg**（关键：MSYS2 的 ffmpeg 包是 GPL 构建，不能随包；
#      自包含/随包发行必须用 `--disable-gpl --disable-nonfree` 的 LGPL 构建）
#   2. 用 meson 构建 **mpv v0.41.0**：`-Dgpl=false -Dlibmpv=true -Dcplayer=false`
#      → 产出 `libmpv-2.dll`（LGPL-2.1+）
#   3. 把 libmpv-2.dll、include/mpv/*.h、许可证、BUILDINFO.txt（含依赖 DLL 清单/hash）
#      放进 vendor/prebuilt/windows-x86_64/
#
# 前置：Windows 10/11 + MSYS2（UCRT64 环境，默认装到 C:\msys64）
#   https://www.msys2.org/  （安装后无需手动初始化，脚本会自动 pacman -Syu）
#
# 用法（Windows PowerShell 5.1+ / PowerShell 7，仓库根目录）：
#   # 只看步骤（默认，不执行）
#   powershell -ExecutionPolicy Bypass -File .\scripts\build-media-stack-windows.ps1
#   # 真正构建（首次约 20–40 分钟；产物进 vendor/prebuilt/windows-x86_64）
#   powershell -ExecutionPolicy Bypass -File .\scripts\build-media-stack-windows.ps1 -Run
#   # 指定 MSYS2 位置 / 并行度 / 版本 / 干净重建
#   powershell -ExecutionPolicy Bypass -File .\scripts\build-media-stack-windows.ps1 -Run `
#       -Msys2Root D:\msys64 -Jobs 8 -MpvVersion 0.41.0 -FfmpegVersion n9.0.2 -Clean
#
# 产物：
#   vendor/prebuilt/windows-x86_64/
#     lib/libmpv-2.dll
#     lib/<FFmpeg/libass/libplacebo/... 依赖 DLL>
#     include/mpv/*.h
#     licenses/{LICENSE.LGPL,COPYING.FFmpeg,mpv-Copyright}
#     BUILDINFO.txt（版本/sha256/依赖 DLL 清单）
#
# 实测状态：**仅 Windows 可实测**；Linux 上只做语法/结构静态检查。
#
#Requires -Version 5.1
param(
  [string]$Msys2Root = "C:\msys64",
  [string]$WorkDir = "",
  [string]$OutDir = "vendor\prebuilt\windows-x86_64",
  [string]$MpvVersion = "0.41.0",
  [string]$FfmpegVersion = "n9.0.2",
  [int]$Jobs = 4,
  [switch]$Run,
  [switch]$Clean,
  [switch]$Help
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root
if (-not $WorkDir) { $WorkDir = Join-Path $Root "出\构建缓存\windows-media-stack" }

function Write-Say($m)  { Write-Host "==> $m" -ForegroundColor Cyan }
function Write-Ok($m)   { Write-Host "  [OK]   $m" -ForegroundColor Green }
function Write-Warn2($m){ Write-Host "  [WARN] $m" -ForegroundColor Yellow }
function Write-Err2($m) { Write-Host "  [ERR]  $m" -ForegroundColor Red }

if ($Help) {
  Get-Content $PSCommandPath | Select-Object -First 56 | ForEach-Object { $_ -replace '^# ?', '' }
  exit 0
}

# Windows 路径 → MSYS2 路径（C:\foo\bar → /c/foo/bar）
function ConvertTo-MsysPath([string]$p) {
  $full = [System.IO.Path]::GetFullPath($p)
  $drive = $full.Substring(0, 1).ToLowerInvariant()
  $rest = $full.Substring(2).Replace('\', '/')
  return "/$drive$rest"
}

$Bash = Join-Path $Msys2Root "usr\bin\bash.exe"
if (-not (Test-Path $Bash)) {
  Write-Err2 "未找到 MSYS2 bash：$Bash"
  Write-Host "    安装 MSYS2：https://www.msys2.org/  然后重试（或 -Msys2Root 指定安装目录）"
  exit 2
}

$WorkM = ConvertTo-MsysPath $WorkDir
$OutM  = ConvertTo-MsysPath (Join-Path $Root $OutDir)
$PrefixM = "$OutM/build-prefix"

# ── 构建步骤（每条都是一段 MSYS2 bash 脚本；默认打印，-Run 执行）──
# 步骤模板使用**单引号 here-string**（PowerShell 不展开 $），占位符在打印/执行前替换，
# 避免 PS/bash 双重转义（$3、$OUT 等）出错。
$Placeholders = [ordered]@{
  "__WORK__"      = $WorkM
  "__PREFIX__"    = $PrefixM
  "__OUT__"       = $OutM
  "__ROOT__"      = (ConvertTo-MsysPath $Root)
  "__MPV_VERSION__"   = $MpvVersion
  "__FFMPEG_VERSION__" = $FfmpegVersion
  "__JOBS__"      = "$Jobs"
}

function Expand-Step([string]$text) {
  $t = $text
  foreach ($k in $Placeholders.Keys) { $t = $t.Replace($k, [string]$Placeholders[$k]) }
  return $t
}

$Steps = @(
  [ordered]@{
    id = "deps"
    title = "安装 MSYS2 UCRT64 构建依赖（只装工具/库，不装 mingw ffmpeg 包——FFmpeg 从源码建 LGPL）"
    run = @'
set -euo pipefail
pacman -S --needed --noconfirm base-devel git curl tar xz p7zip meson ninja pkgconf nasm \
  mingw-w64-ucrt-x86_64-gcc \
  mingw-w64-ucrt-x86_64-python \
  mingw-w64-ucrt-x86_64-meson \
  mingw-w64-ucrt-x86_64-libass \
  mingw-w64-ucrt-x86_64-libplacebo \
  mingw-w64-ucrt-x86_64-libbluray
'@
  },
  [ordered]@{
    id = "ffmpeg-src"
    title = "下载 FFmpeg __FFMPEG_VERSION__ 源码并构建 **LGPL** 版（--disable-gpl --disable-nonfree --disable-version3）"
    run = @'
set -euo pipefail
mkdir -p '__WORK__'
cd '__WORK__'
if [ ! -d 'ffmpeg-src' ]; then
  curl -fL -o ffmpeg.tar.gz 'https://github.com/FFmpeg/FFmpeg/archive/refs/tags/__FFMPEG_VERSION__.tar.gz'
  tar -xzf ffmpeg.tar.gz
  mv 'FFmpeg-__FFMPEG_VERSION__' 'ffmpeg-src' 2>/dev/null || true
  [ -d 'ffmpeg-src' ] || mv FFmpeg-* 'ffmpeg-src'
fi
cd 'ffmpeg-src'
if [ -f '__PREFIX__/ffmpeg/lib/pkgconfig/libavcodec.pc' ]; then
  echo "FFmpeg LGPL 已构建（缓存命中），跳过 configure/make"
else
  ./configure --prefix='__PREFIX__/ffmpeg' \
    --disable-gpl --disable-nonfree --disable-version3 \
    --disable-doc --disable-debug --disable-programs \
    --enable-shared --disable-static \
    --disable-autodetect
  make -j__JOBS__
  make install
fi
'@
  },
  [ordered]@{
    id = "mpv-src"
    title = "下载 mpv v__MPV_VERSION__ 源码并构建 LGPL libmpv（-Dgpl=false -Dlibmpv=true -Dcplayer=false）"
    run = @'
set -euo pipefail
mkdir -p '__WORK__'
cd '__WORK__'
if [ ! -d 'mpv-src' ]; then
  curl -fL -o mpv.tar.gz 'https://github.com/mpv-player/mpv/archive/refs/tags/v__MPV_VERSION__.tar.gz'
  tar -xzf mpv.tar.gz
  mv 'mpv-__MPV_VERSION__' 'mpv-src' 2>/dev/null || true
  [ -d 'mpv-src' ] || mv mpv-* 'mpv-src'
fi
export PKG_CONFIG_PATH='__PREFIX__/ffmpeg/lib/pkgconfig:/ucrt64/lib/pkgconfig'
# MSYS2 的 meson 由 MSYS python 驱动，在 UCRT64/MinGW 环境下会被 meson 拒绝
# （"This python3 seems to be msys/python ... but you are in a MinGW environment"）。
# 解法：用 mingw-w64 python 安装并运行 meson。
export PATH="/ucrt64/bin:$PATH"
# 优先用 MSYS2 的 mingw meson（/ucrt64/bin/meson.exe）：它自带 UCRT64 python，
# 不会触发 meson 对「MSYS python + MinGW 环境」的拒绝，也不需要 pip。
MESON="/ucrt64/bin/meson.exe"
if [ -x "$MESON" ]; then
  echo "使用 mingw meson：$MESON"
else
  echo "未找到 /ucrt64/bin/meson.exe（缺 mingw-w64-ucrt-x86_64-meson），尝试 mingw python + pip 兜底" >&2
  PY=""
  for cand in /ucrt64/bin/python.exe /ucrt64/bin/python; do
    if [ -x "$cand" ]; then PY="$cand"; break; fi
  done
  if [ -z "$PY" ]; then
    echo "错误：未找到 UCRT64 python" >&2
    exit 3
  fi
  "$PY" -m pip --version >/dev/null 2>&1 || {
    echo "mingw python 无 pip，下载 get-pip.py 安装" >&2
    curl -fsSL https://bootstrap.pypa.io/get-pip.py -o get-pip.py
    "$PY" get-pip.py || exit 4
  }
  "$PY" -c 'import mesonbuild' >/dev/null 2>&1 || "$PY" -m pip install --quiet meson
  MESON="$PY -m mesonbuild.mesonmain"
fi
rm -rf mpv-build
if [ "$MESON" = "/ucrt64/bin/meson.exe" ]; then
  "$MESON" setup mpv-build mpv-src \
    --buildtype=release --prefix='__PREFIX__/mpv' \
    -Dgpl=false -Dlibmpv=true -Dcplayer=false -Dbuild-date=false \
    -Dlua=disabled -Djavascript=disabled -Drubberband=disabled \
    -Dtests=false -Dmanpage-build=disabled
else
  $MESON setup mpv-build mpv-src \
    --buildtype=release --prefix='__PREFIX__/mpv' \
    -Dgpl=false -Dlibmpv=true -Dcplayer=false -Dbuild-date=false \
    -Dlua=disabled -Djavascript=disabled -Drubberband=disabled \
    -Dtests=false -Dmanpage-build=disabled
fi
ninja -C mpv-build
ninja -C mpv-build install
'@
  },
  [ordered]@{
    id = "stage"
    title = "收集 libmpv-2.dll + 依赖 DLL + 头文件 + 许可证 + BUILDINFO"
    run = @'
set -euo pipefail
OUT='__OUT__'
mkdir -p "$OUT/lib" "$OUT/include/mpv" "$OUT/licenses"
cp '__PREFIX__/mpv/bin/libmpv-2.dll' "$OUT/lib/"
cp '__PREFIX__/mpv/include/mpv/'*.h "$OUT/include/mpv/" 2>/dev/null || true

# 依赖 DLL：用 MSYS2 ldd 枚举，只收 /ucrt64/bin 下非系统 DLL（系统 API 集不拷）
ldd '__PREFIX__/mpv/bin/libmpv-2.dll' \
  | awk '{print $3}' | grep -E '^/ucrt64/bin/' | sort -u \
  | while read -r dll; do cp -f "$dll" "$OUT/lib/"; done

# 许可证
cp -f '__WORK__/mpv-src/Copyright' "$OUT/licenses/mpv-Copyright" 2>/dev/null || true
cp -f '__WORK__/ffmpeg-src/COPYING.LGPLv2.1' "$OUT/licenses/COPYING.FFmpeg-LGPLv2.1" 2>/dev/null || true
cp -f '__ROOT__/assets/licenses/LGPL-2.1.txt' "$OUT/licenses/LICENSE.LGPL" 2>/dev/null || true

# BUILDINFO
{
  echo 'media-stack (LGPL libmpv) build info — Windows x86_64'
  echo "built_at:       $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo 'builder:        MSYS2 UCRT64 (MinGW-w64)'
  echo 'mpv_version:    v__MPV_VERSION__'
  echo 'ffmpeg_version: __FFMPEG_VERSION__ (LGPL: --disable-gpl --disable-nonfree --disable-version3)'
  echo 'build_type:     release, -Dgpl=false -Dlibmpv=true -Dcplayer=false'
  echo -n 'libmpv_sha256:  '; sha256sum "$OUT/lib/libmpv-2.dll" | awk '{print $1}'
  echo -n 'libmpv_size:    '; stat -c %s "$OUT/lib/libmpv-2.dll"
  echo 'linked_deps:'
  ldd "$OUT/lib/libmpv-2.dll" | sed 's/^/  /'
} > "$OUT/BUILDINFO.txt"
echo '=== staged files ==='
ls -la "$OUT/lib" | head -40
'@
  }
)

Write-Say "Windows media-stack 构建（LGPL libmpv）"
Write-Host "    MSYS2：$Msys2Root（UCRT64）"
Write-Host "    工作目录：$WorkDir"
Write-Host "    产物目录：$(Join-Path $Root $OutDir)"
Write-Host "    mpv：v$MpvVersion　FFmpeg：$FfmpegVersion（LGPL 源码构建）"
if (-not $Run) {
  Write-Warn2 "当前为结构预览（未执行）。加 -Run 在 Windows 上真正构建（首次约 20–40 分钟）。"
}
Write-Host ""

function Invoke-Msys2Bash([string]$script, [string]$label) {
  Write-Say "[$label]"
  $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("wp-msys2-" + [guid]::NewGuid().ToString("N").Substring(0, 8) + ".sh")
  try {
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($tmp, (Expand-Step $script), $utf8)
    $shPath = ConvertTo-MsysPath $tmp
    $env:MSYSTEM = "UCRT64"
    $env:CHERE_INVOKING = "1"
    $cmd = "source /etc/profile && bash '$shPath'"
    # 注意：不能直接 `& $Bash ...` + `return $LASTEXITCODE`。
    # bash 的 stdout 会进入函数输出流，导致调用方拿到「输出行 + 退出码」数组，
    # 进而被误判为失败（首次 CI 实测踩坑）。这里显式把输出写到 Host，只返回退出码。
    & $Bash "-lc" $cmd | Out-Host
    $rc = $LASTEXITCODE
    return $rc
  } finally {
    Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
  }
}

if (-not $Run) {
  Write-Host "── 构建步骤（预览）──"
  foreach ($s in $Steps) {
    Write-Host ""
    Write-Host ("### {0}: {1}" -f $s.id, $s.title) -ForegroundColor Yellow
    Write-Host (Expand-Step $s.run)
  }
  Write-Host ""
  Write-Host "执行：powershell -ExecutionPolicy Bypass -File .\scripts\build-media-stack-windows.ps1 -Run"
  Write-Host "完成后：scripts\fetch-angle.ps1 → scripts\package-player-runtime-windows.ps1 -Generate -Check"
  exit 0
}

# ── -Run：实际执行 ─────────────────────────────────────────
if ($Clean) {
  Write-Say "清理工作目录与产物：$WorkDir 、$(Join-Path $Root $OutDir)"
  Remove-Item -LiteralPath $WorkDir -Recurse -Force -ErrorAction SilentlyContinue
  Remove-Item -LiteralPath (Join-Path $Root $OutDir) -Recurse -Force -ErrorAction SilentlyContinue
}
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $Root $OutDir) | Out-Null

foreach ($s in $Steps) {
  $code = Invoke-Msys2Bash $s.run $s.id
  if ($code -ne 0) {
    Write-Err2 "步骤 $($s.id) 失败（退出码 $code）；修复后重跑（已完成的步骤可跳过）"
    exit 1
  }
  Write-Ok "步骤 $($s.id) 完成"
}

Write-Say "构建完成：$(Join-Path $Root $OutDir)"
Write-Host "下一步："
Write-Host "  powershell -ExecutionPolicy Bypass -File .\scripts\fetch-angle.ps1"
Write-Host "  powershell -ExecutionPolicy Bypass -File .\scripts\package-player-runtime-windows.ps1 -Generate -Check -Strict"
Write-Host "  powershell -ExecutionPolicy Bypass -File .\scripts\windows-verify.ps1 -Sample1080 <本地mp4> -Sample4K <4K样本>"
exit 0
