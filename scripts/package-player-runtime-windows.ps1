# 播放器运行时打包（Windows x86_64）—— 结构级生成 / 校验
#
# 状态：**结构级脚本**。本机开发环境为 Linux（无 Windows/pwsh 可执行验证），因此本脚本
#       做「目录结构 + DLL 清单 + ANGLE + 许可证 + 加载顺序」的生成与校验，
#       **不下载 DLL、不构建 libmpv-2.dll**（构建见 scripts/build-media-stack-windows.ps1）。
#
# 使用（Windows PowerShell 5.1+ / PowerShell 7，在仓库根目录）：
#   # ① 只校验现有 播放器运行时\（默认；缺必需 DLL 时退出码 1）
#   powershell -ExecutionPolicy Bypass -File .\scripts\package-player-runtime-windows.ps1
#
#   # ② 生成目录骨架 + 许可证 + 清单 + 加载顺序说明（开发机无 DLL 也可跑，缺 DLL 只警告）
#   powershell -ExecutionPolicy Bypass -File .\scripts\package-player-runtime-windows.ps1 -Generate
#
#   # ③ 指定目录 / 严格模式（缺任何必需 DLL 即失败）
#   powershell -ExecutionPolicy Bypass -File .\scripts\package-player-runtime-windows.ps1 -Check -Strict
#   powershell -ExecutionPolicy Bypass -File .\scripts\package-player-runtime-windows.ps1 -Generate `
#       -RuntimeDir "出\网盘管理-win64\播放器运行时" -PrebuiltDir "vendor\prebuilt\windows-x86_64"
#
# 产物结构（与 build-windows.ps1 --Portable 的 出\网盘管理-win64\ 对齐）：
#   网盘管理.exe
#   播放器运行时\
#     libmpv-2.dll          # 我们 vendored 的 LGPL libmpv（Mode A OpenGL render API）
#     libass-*.dll  libavcodec-*.dll  libavformat-*.dll  libavutil-*.dll
#     libavfilter-*.dll  libswresample-*.dll  libswscale-*.dll  libavdevice-*.dll
#     libplacebo-*.dll  libbluray-*.dll  libluajit-*.dll  libwinpthread-1.dll …
#     libEGL.dll  libGLESv2.dll   # ANGLE（BSD-3-Clause；d3dcompiler_47.dll 可选）
#     LICENSES\…                  # 第三方许可证（assets\licenses 同步）
#     BUILDINFO.txt  DEPENDENCIES.txt  LOAD-ORDER.txt  README.txt
#
# DLL 来源：优先从 -PrebuiltDir（默认 vendor\prebuilt\windows-x86_64）复制：
#   <PrebuiltDir>\lib\*.dll 或 <PrebuiltDir>\*.dll；ANGLE 可放 <PrebuiltDir>\angle\。
#   本脚本不会联网下载：依赖 DLL 由 scripts/build-media-stack-windows.ps1（MSYS2 UCRT64 构建）
#   或发行方准备。
#
#Requires -Version 5.1
param(
  [string]$RuntimeDir = "播放器运行时",
  [string]$PrebuiltDir = "vendor\prebuilt\windows-x86_64",
  [switch]$Generate,
  [switch]$Check,
  [switch]$Strict,
  [switch]$Help
)

$ErrorActionPreference = "Stop"

# ---------- 工具 ----------
function Write-Ok($msg)   { Write-Host "  [OK]   $msg" -ForegroundColor Green }
function Write-Warn2($msg){ Write-Host "  [WARN] $msg" -ForegroundColor Yellow }
function Write-Err2($msg) { Write-Host "  [ERR]  $msg" -ForegroundColor Red }
function Write-Say($msg)  { Write-Host "==> $msg" -ForegroundColor Cyan }
function Write-Utf8($path, $content) {
  $dir = Split-Path -Parent $path
  if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
  [System.IO.File]::WriteAllText($path, $content, (New-Object System.Text.UTF8Encoding($false)))
}
function Get-Root {
  # 脚本位于 <root>\scripts\ 下；$PSScriptRoot 比 $MyInvocation 更可靠
  return (Split-Path -Parent $PSScriptRoot)
}

# ---------- 帮助 ----------
if ($Help) {
  $lines = Get-Content $PSCommandPath | Select-Object -First 45
  $lines | ForEach-Object { $_ -replace '^# ?', '' }
  exit 0
}

$Root = Get-Root
Set-Location $Root
if (-not $Check -and -not $Generate) { $Check = $true }

Write-Say "Windows 播放器运行时（结构级）"
Write-Host "    仓库:        $Root"
Write-Host "    运行时目录:  $RuntimeDir"
Write-Host "    prebuilt:    $PrebuiltDir"
Write-Host "    模式:        $(if ($Generate) { 'Generate' } else { 'Check' })$(if ($Strict) { ' +Strict' })"

# ---------- 清单定义 ----------
# 必需 DLL（缺失 → 播放器无法加载 libmpv 或无法走 ANGLE）
$RequiredDlls = @(
  "libmpv-2.dll",
  "libEGL.dll",
  "libGLESv2.dll"
)
# 必需依赖（通配符；libmpv 的运行时依赖，缺一不可）——按 scripts/build-media-stack-windows.ps1 的说明
$RequiredPatterns = @(
  "libavcodec-*.dll",
  "libavformat-*.dll",
  "libavutil-*.dll",
  "libavfilter-*.dll",
  "libswresample-*.dll",
  "libswscale-*.dll",
  "libavdevice-*.dll",
  "libass-*.dll",
  "libplacebo-*.dll",
  "libwinpthread-1.dll"
)
# 可选依赖（构建配置/解码器/网络差异，缺失记录但不阻断）
$OptionalPatterns = @(
  "libbluray-*.dll",
  "libluajit-*.dll",
  "libuchardet-*.dll",
  "libarchive-*.dll",
  "zlib1.dll",
  "libssl-*.dll",
  "libcrypto-*.dll",
  "libsixel-*.dll",
  "d3dcompiler_47.dll",
  "vcruntime140*.dll",
  "msvcp140*.dll"
)
# 必需许可证（与自己 assets\licenses 对齐；Windows 额外要求 ANGLE）
$RequiredLicenses = @(
  "LGPL-2.1.txt", "GPL-2.0.txt", "mpv-Copyright.txt",
  "libass-ISC.txt", "FFmpeg-LGPL-2.1.txt", "libplacebo-LGPL-2.1.txt",
  "libbluray-LGPL-2.1.txt", "libaacs-LGPL-2.1.txt", "libbdplus-LGPL-2.1.txt",
  "ANGLE-BSD-3-Clause.txt"
)

# ---------- 生成骨架 ----------
function New-RuntimeSkeleton {
  Write-Say "生成运行时目录骨架"
  New-Item -ItemType Directory -Force -Path $RuntimeDir | Out-Null
  New-Item -ItemType Directory -Force -Path (Join-Path $RuntimeDir "LICENSES") | Out-Null

  $licSrc = Join-Path $Root "assets\licenses"
  if (Test-Path $licSrc) {
    Get-ChildItem $licSrc -File | ForEach-Object {
      Copy-Item $_.FullName (Join-Path $RuntimeDir "LICENSES") -Force
    }
    Write-Ok "LICENSES 已从 assets\licenses 同步"
  } else {
    Write-Warn2 "未找到 assets\licenses，LICENSES 需要发行方补齐"
  }

  # 从 prebuilt 复制 DLL（若存在）
  $copied = 0
  if (Test-Path $PrebuiltDir) {
    $sources = @()
    $sources += Get-ChildItem $PrebuiltDir -Filter *.dll -File -ErrorAction SilentlyContinue
    $libDir = Join-Path $PrebuiltDir "lib"
    if (Test-Path $libDir) { $sources += Get-ChildItem $libDir -Filter *.dll -File -ErrorAction SilentlyContinue }
    $angleDir = Join-Path $PrebuiltDir "angle"
    if (Test-Path $angleDir) { $sources += Get-ChildItem $angleDir -Filter *.dll -File -ErrorAction SilentlyContinue }
    foreach ($f in $sources) {
      Copy-Item $f.FullName (Join-Path $RuntimeDir $f.Name) -Force
      $copied++
    }
    if ($copied -gt 0) { Write-Ok "从 prebuilt 复制 $copied 个 DLL" } else { Write-Warn2 "prebuilt 内没有 DLL（先跑 build-media-stack-windows.ps1）" }
  } else {
    Write-Warn2 "prebuilt 目录不存在：$PrebuiltDir（结构级生成继续，DLL 需发行方补齐）"
  }

  # LOAD-ORDER.txt：DLL 搜索顺序与加载方式
  $loadOrder = @"
Windows 播放器运行时 · DLL 加载顺序说明
=======================================
生成：$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')

1) 应用如何找 libmpv：
   - 运行时动态加载（wp-media-mpv）：优先 可执行文件同级\播放器运行时\libmpv-2.dll，
     其次 可执行文件同级\libmpv-2.dll、播放器运行时\lib\libmpv-2.dll；
     也支持环境变量 WP_MPV_LIB 显式指定（调试/自定义构建）。
   - 因此本目录应与「网盘管理.exe」同级：网盘管理.exe 与 播放器运行时\ 并列。

2) libmpv-2.dll 如何找自己的依赖 DLL（Windows 默认搜索顺序）：
   - 应用目录（exe 所在目录）优先；
   - 然后 System32 等系统目录；
   - 然后 PATH。
   注意：**「播放器运行时\」子目录默认不在搜索路径内**。本应用在启动时会把
   播放器运行时\ 加入进程 DLL 搜索路径（AddDllDirectory + SetDefaultDllDirectories，
   不使用全局 SetDllDirectory/PATH，避免污染），保证 libmpv 依赖能从该目录加载。
   若你自行替换加载逻辑，必须同样在 LoadLibrary 之前注册该目录。

3) ANGLE（libEGL.dll / libGLESv2.dll）：
   - 用于 Windows 内嵌渲染（Mode A，GLES/EGL → D3D11VA 互操作）；
   - 放置在本目录，由应用在创建 GL 上下文前加入搜索路径；
   - ANGLE 为 BSD-3-Clause，许可证见 LICENSES\ANGLE-BSD-3-Clause.txt；
   - 可选 d3dcompiler_47.dll（部分系统需其编译 shader）。

4) 发布前必查：
   - 干净 Windows 10/11（无系统 libmpv/FFmpeg）能启动并播放；
   - 删除/重命名 libmpv-2.dll → 应用给可读错误而非崩溃；
   - 删除 ANGLE DLL → 能回退 WGL（软解/提示），不崩溃；
   - DEPENDENCIES.txt 与实际 DLL 一致；LICENSES\ 完整。
"@
  Write-Utf8 (Join-Path $RuntimeDir "LOAD-ORDER.txt") $loadOrder
  Write-Ok "LOAD-ORDER.txt（加载顺序/发布前检查）"

  # DEPENDENCIES.txt 模板（实际依赖以 ntldd/Dependencies.exe 导出为准）
  $deps = @"
Windows 播放器运行时 · 依赖清单（结构级）
=========================================
生成：$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')
注意：本清单由 scripts/package-player-runtime-windows.ps1 按构建脚本已知依赖生成；
      最终以构建机 ntldd -R libmpv-2.dll（MSYS2）或 Dependencies.exe 的递归结果为准。

必需（缺失则无法播放）：
  libmpv-2.dll            LGPL-2.1+（本仓库 -Dgpl=false 构建的 libmpv）
  libEGL.dll / libGLESv2.dll   ANGLE（BSD-3-Clause）
$($RequiredPatterns | ForEach-Object { "  $_" } | Out-String)
可选（按构建配置/功能裁剪）：
$($OptionalPatterns | ForEach-Object { "  $_" } | Out-String)
来源：
  - libmpv-2.dll + FFmpeg/libass/libplacebo/libbluray/libluajit 等：MSYS2 UCRT64
    构建（scripts/build-media-stack-windows.ps1）后放入 vendor\prebuilt\windows-x86_64\lib\；
  - ANGLE：推荐取 mpv 官方 Windows 发行包同源 ANGLE（版本匹配），或自建（见 docs\PLAYER_WINDOWS_ANGLE.md）。
"@
  Write-Utf8 (Join-Path $RuntimeDir "DEPENDENCIES.txt") $deps
  Write-Ok "DEPENDENCIES.txt（模板）"

  # BUILDINFO.txt
  $manifest = $RequiredDlls + $RequiredPatterns + $OptionalPatterns
  $buildInfo = @"
# 播放器运行时 BUILDINFO（Windows x86_64）
# 由 scripts/package-player-runtime-windows.ps1 -Generate 生成（结构级）
generated_at:     $(Get-Date -Format 'yyyy-MM-ddTHH:mm:sszzz')
package_mode:     windows-structural
runtime_dir:      $RuntimeDir
prebuilt_dir:     $PrebuiltDir
dll_manifest:     $($manifest -join ", ")
licenses_count:   $(if (Test-Path (Join-Path $RuntimeDir 'LICENSES')) { @(Get-ChildItem (Join-Path $RuntimeDir 'LICENSES') -File).Count } else { 0 })
note:             结构级生成；DLL 与最终版本信息由 Windows 构建机补全（见 LOAD-ORDER.txt / DEPENDENCIES.txt）
"@
  Write-Utf8 (Join-Path $RuntimeDir "BUILDINFO.txt") $buildInfo
  Write-Ok "BUILDINFO.txt"

  # README.txt
  $readme = @"
网盘管理 · 播放器运行时（Windows x86_64）
=========================================
结构：libmpv-2.dll、FFmpeg/libass/libplacebo 等依赖 DLL、ANGLE(libEGL/libGLESv2)、
      LICENSES\、BUILDINFO.txt、DEPENDENCIES.txt、LOAD-ORDER.txt
位置：与「网盘管理.exe」同级（网盘管理.exe 与 播放器运行时\ 并列）。
许可证：mpv 核心 LGPL-2.1+（-Dgpl=false）；ANGLE BSD-3-Clause；全文见 LICENSES\。
源码获取：见 LICENSES\README.md 与 docs\PLAYER_PACKAGING.md §4。
"@
  Write-Utf8 (Join-Path $RuntimeDir "README.txt") $readme
  Write-Ok "README.txt"
}

# ---------- 校验 ----------
$Errors = 0
$Warnings = 0
function Test-OneDll($name) {
  # 运行时根目录优先，其次 lib\ 子目录
  $candidates = @((Join-Path $RuntimeDir $name), (Join-Path $RuntimeDir ("lib\" + $name)))
  foreach ($c in $candidates) { if (Test-Path $c) { return $true } }
  return $false
}
function Test-Pattern($pattern) {
  $candidates = @((Join-Path $RuntimeDir $pattern), (Join-Path $RuntimeDir ("lib\" + $pattern)))
  foreach ($c in $candidates) {
    if (Get-ChildItem $c -File -ErrorAction SilentlyContinue) { return $true }
  }
  return $false
}

function Invoke-Check {
  Write-Say "校验运行时目录：$RuntimeDir"
  if (-not (Test-Path $RuntimeDir)) {
    Write-Err2 "运行时目录不存在：$RuntimeDir（可先 -Generate 生成骨架）"
    $script:Errors++
    return
  }

  # 1) 必需 DLL
  foreach ($dll in $RequiredDlls) {
    if (Test-OneDll $dll) { Write-Ok "必需 DLL：$dll" }
    else { Write-Err2 "缺必需 DLL：$dll"; $script:Errors++ }
  }
  # 2) 必需依赖（通配符）
  foreach ($pat in $RequiredPatterns) {
    if (Test-Pattern $pat) { Write-Ok "依赖：$pat" }
    else { Write-Err2 "缺依赖：$pat"; $script:Errors++ }
  }
  # 3) 可选依赖
  foreach ($pat in $OptionalPatterns) {
    if (Test-Pattern $pat) { Write-Ok "可选：$pat" }
    else { Write-Warn2 "可选缺失：$pat"; $script:Warnings++ }
  }
  # 4) 许可证
  if (Test-Path (Join-Path $RuntimeDir "LICENSES")) {
    $missing = @()
    foreach ($lic in $RequiredLicenses) {
      if (-not (Test-Path (Join-Path $RuntimeDir ("LICENSES\" + $lic)))) { $missing += $lic }
    }
    if ($missing.Count -eq 0) { Write-Ok "LICENSES 完整（$($RequiredLicenses.Count) 份）" }
    else { Write-Err2 ("LICENSES 缺失：" + ($missing -join ", ")); $script:Errors++ }
  } else {
    Write-Err2 "缺 LICENSES 目录"; $script:Errors++
  }
  # 5) 说明文件
  foreach ($f in @("README.txt", "LOAD-ORDER.txt", "DEPENDENCIES.txt", "BUILDINFO.txt")) {
    if (Test-Path (Join-Path $RuntimeDir $f)) { Write-Ok "文件：$f" }
    else { Write-Warn2 "缺文件：$f（建议 -Generate 补全）"; $script:Warnings++ }
  }
  # 6) 与 exe 同级检查（提示性）
  $exeDir = Join-Path $Root "出\网盘管理-win64"
  if (Test-Path $exeDir) {
    $exe = Join-Path $exeDir "网盘管理.exe"
    if (Test-Path $exe) {
      if (Test-Path (Join-Path $exeDir "播放器运行时")) { Write-Ok "便携目录布局：出\网盘管理-win64\播放器运行时 已就位" }
      else { Write-Warn2 "便携目录存在但没有 播放器运行时\（build-windows.ps1 --Portable 后需拷贝本目录）"; $script:Warnings++ }
    }
  }
  # 7) DLL 架构/签名无法在非 Windows 校验，明确提示
  Write-Warn2 "PE 架构/签名/依赖递归需在 Windows 构建机用 ntldd -R 或 Dependencies.exe 复核（本脚本结构级）"
}

# ---------- 主流程 ----------
if ($Generate) {
  New-RuntimeSkeleton
  Write-Say "生成完成：$RuntimeDir"
  if (-not (Test-Path (Join-Path $RuntimeDir "libmpv-2.dll"))) {
    Write-Warn2 "当前没有 libmpv-2.dll（结构级生成）；在 Windows 构建机构建后重跑 -Generate 或手工放入"
  }
  Invoke-Check
} else {
  Invoke-Check
}

Write-Host ""
Write-Host ("校验结果：Errors={0}  Warnings={1}" -f $Errors, $Warnings) -ForegroundColor $(if ($Errors -eq 0) { "Green" } else { "Red" })
if ($Errors -gt 0 -and $Strict) {
  Write-Err2 "Strict 模式：存在错误，退出码 1"
  exit 1
}
if ($Errors -gt 0) {
  Write-Warn2 "存在 $Errors 项错误（非 Strict 模式退出码 0；发布前必须清零）"
}
exit 0
