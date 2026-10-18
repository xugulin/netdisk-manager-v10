# 获取并校验 Windows ANGLE 运行时（libEGL.dll / libGLESv2.dll / d3dcompiler_47.dll）
#
# 背景（见 docs/PLAYER_WINDOWS_ANGLE.md）：
#   Windows 必须走 Mode A（进程内 GL 渲染）+ EGL/GLES（ANGLE）。mpv 官方 Windows 包
#   不再自带 ANGLE；本项目从「ANGLE prebuilt（Chrome stable 同源构建）」取 BSD-3 的
#   ANGLE DLL，**不提交二进制**（.gitignore 已忽略 vendor/prebuilt/）。
#
# 来源与固定（pin）：
#   仓库  https://github.com/dsafdsaf132/angle-prebuilt
#   发布  angle-stable-chrome-155.0.8059.80-cccf60c（2026-10-14）
#   文件  angle-cccf60c-win32-x64.zip（win32 x64，内含 angle/out/Release/*.dll）
#   归档 sha256 / 各 DLL sha256 见下方常量；校验失败脚本会打印实测值并提示如何更新 pin。
#
# 许可证：ANGLE = BSD-3-Clause（zip 内 angle/LICENSE，随附到 ANGLE-LICENSE.txt；
#         发行包 LICENSES/ANGLE-BSD-3-Clause.txt 由 assets/licenses/ 提供）。
#         ⚠️ 该 zip 由 Chrome stable 构建链产出，仅提取 ANGLE 的 BSD-3 DLL；
#         不提取/不复用 mpv、Chrome 等 GPL/其它许可组件。
#
# 用法（在 Windows PowerShell 5.1+/PowerShell 7，仓库根目录）：
#   powershell -ExecutionPolicy Bypass -File .\scripts\fetch-angle.ps1
#   powershell -ExecutionPolicy Bypass -File .\scripts\fetch-angle.ps1 -PrintOnly
#   powershell -ExecutionPolicy Bypass -File .\scripts\fetch-angle.ps1 -Archive .\angle-cccf60c-win32-x64.zip
#   powershell -ExecutionPolicy Bypass -File .\scripts\fetch-angle.ps1 -OutDir vendor\prebuilt\windows-x86_64\angle -Force
#
# 产物：
#   <OutDir>\libEGL.dll  <OutDir>\libGLESv2.dll  [<OutDir>\d3dcompiler_47.dll]
#   <OutDir>\ANGLE-LICENSE.txt    ANGLE BSD-3 许可证
#   <OutDir>\ANGLE-SOURCE.txt     来源/校验/构建元数据（供合规与复现）
#
#Requires -Version 5.1
param(
  [string]$OutDir = "vendor\prebuilt\windows-x86_64\angle",
  [string]$Archive = "",
  [switch]$WithHeaders,
  [switch]$PrintOnly,
  [switch]$Force,
  [switch]$Help
)

$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

# ── 固定来源（更新时：改成新 release URL，并把下面的 sha256 换成实测值）──
$AngleRepo       = "https://github.com/dsafdsaf132/angle-prebuilt"
$AngleRelease    = "angle-stable-chrome-155.0.8059.80-cccf60c"
$AngleChromeVer  = "155.0.8059.80"
$AngleCommit     = "cccf60cde32c911b017a315b82235df61da5057b"
$AngleArchive    = "angle-cccf60c-win32-x64.zip"
$AngleSourceUrl  = "$AngleRepo/releases/download/$AngleRelease/$AngleArchive"
$AngleArchiveSha = "2f13aee2f06ae3ae9c82d2095c38bd5ac2ea5981ecfa972c7ca46fb18e20ccbf"
# 必需 DLL：name → @{ sha256; size; optional }
$AngleDlls = [ordered]@{
  "libEGL.dll"           = @{ sha256 = "9b26ba4761aa448d83ccc134115423a361c78806604a3e24c261f30022e7a258"; size = 502272;   optional = $false }
  "libGLESv2.dll"        = @{ sha256 = "bbe20e1e13ed221bef84eecf3259eac741fa3aacf5495406f130c68ca1431001"; size = 7844864;  optional = $false }
  "d3dcompiler_47.dll"   = @{ sha256 = "d28d341306db9067c779d2cfe5865c3840e3a863efa47a2e0b0d4a1b906f6158"; size = 4753864;  optional = $true  }
}

function Write-Say($m)  { Write-Host "==> $m" -ForegroundColor Cyan }
function Write-Ok($m)   { Write-Host "  [OK]   $m" -ForegroundColor Green }
function Write-Warn2($m){ Write-Host "  [WARN] $m" -ForegroundColor Yellow }
function Write-Err2($m) { Write-Host "  [ERR]  $m" -ForegroundColor Red }
function Get-Root {
  # 脚本位于 <root>\scripts\ 下
  return (Split-Path -Parent $PSScriptRoot)
}
function Get-Sha256($path) {
  return (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant()
}

# 校验 PE 文件为 x86-64（machine=0x8664, PE32+ magic=0x20b）
function Test-PeX64($path) {
  $fs = $null; $br = $null
  try {
    $fs = [System.IO.File]::OpenRead($path)
    $br = New-Object System.IO.BinaryReader($fs)
    if ($br.ReadUInt16() -ne 0x5A4D) { return @{ ok = $false; detail = "非 MZ 文件" } }   # 'MZ'
    $fs.Seek(0x3C, [System.IO.SeekOrigin]::Begin) | Out-Null
    $peOff = $br.ReadInt32()
    if ($peOff -le 0 -or $peOff -ge $fs.Length - 24) { return @{ ok = $false; detail = "PE 头偏移非法" } }
    $fs.Seek($peOff, [System.IO.SeekOrigin]::Begin) | Out-Null
    if ($br.ReadUInt32() -ne 0x00004550) { return @{ ok = $false; detail = "缺少 PE 签名" } } # 'PE\0\0'
    $machine = $br.ReadUInt16()
    $fs.Seek($peOff + 24, [System.IO.SeekOrigin]::Begin) | Out-Null
    $magic = $br.ReadUInt16()
    $ok = ($machine -eq 0x8664) -and ($magic -eq 0x20B)
    $detail = ("machine=0x{0:X4} magic=0x{1:X4}" -f $machine, $magic)
    return @{ ok = $ok; detail = $detail }
  } catch {
    return @{ ok = $false; detail = "PE 解析失败：$($_.Exception.Message)" }
  } finally {
    if ($br) { $br.Dispose() }
    if ($fs) { $fs.Dispose() }
  }
}

function Extract-ZipEntries($zipPath, $destDir, $nameSuffixes) {
  Add-Type -AssemblyName System.IO.Compression.FileSystem
  $zip = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
  try {
    $wanted = @()
    foreach ($entry in $zip.Entries) {
      foreach ($suffix in $nameSuffixes) {
        if ($entry.FullName.Replace('\', '/').EndsWith($suffix, [System.StringComparison]::OrdinalIgnoreCase)) {
          $wanted += $entry
        }
      }
    }
    foreach ($entry in $wanted) {
      $leaf = Split-Path -Leaf $entry.FullName.Replace('\', '/')
      $target = Join-Path $destDir $leaf
      [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $target, $true)
    }
    return $wanted.Count
  } finally {
    $zip.Dispose()
  }
}

# ── 帮助 ──────────────────────────────────────────────────
if ($Help) {
  Get-Content $PSCommandPath | Select-Object -First 48 | ForEach-Object { $_ -replace '^# ?', '' }
  exit 0
}

$Root = Get-Root
Set-Location $Root

Write-Say "Windows ANGLE 运行时获取/校验"
Write-Host "    来源：$AngleRepo"
Write-Host "    发布：$AngleRelease（Chrome $AngleChromeVer，ANGLE commit ${AngleCommit}）"
Write-Host "    归档：$AngleArchive"
Write-Host "    归档 sha256：$AngleArchiveSha"
Write-Host "    目标：$OutDir"

if ($PrintOnly) {
  Write-Say "PrintOnly：只打印步骤，不下载"
  Write-Host "  1) 下载 $AngleSourceUrl"
  Write-Host "  2) 校验归档 sha256 = $AngleArchiveSha"
  Write-Host "  3) 从归档提取（angle/out/Release/）："
  foreach ($k in $AngleDlls.Keys) {
    $d = $AngleDlls[$k]
    $tag = if ($d.optional) { "可选" } else { "必需" }
    Write-Host ("       {0}  [{1}]  sha256={2}" -f $k, $tag, $d.sha256)
  }
  Write-Host "  4) PE 架构校验（x86-64）+ 复制到 $OutDir"
  Write-Host "  5) 写 ANGLE-SOURCE.txt / ANGLE-LICENSE.txt"
  Write-Host ""
  Write-Host "  手动下载（无法联网时）：下载上述 URL 的 zip，然后："
  Write-Host "    powershell -ExecutionPolicy Bypass -File .\scripts\fetch-angle.ps1 -Archive .\$AngleArchive"
  exit 0
}

# ── 准备目标目录 ───────────────────────────────────────────
if ((Test-Path $OutDir) -and -not $Force) {
  $existing = @(Get-ChildItem $OutDir -Filter *.dll -File -ErrorAction SilentlyContinue)
  if ($existing.Count -gt 0) {
    Write-Warn2 "目标已存在 DLL（$($existing.Count) 个）：$OutDir（加 -Force 覆盖重取）"
    exit 0
  }
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("wp-angle-" + [guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory -Force -Path $Tmp | Out-Null

try {
  # ── 1) 取得归档（下载或使用离线文件）────────────────────
  if ($Archive -ne "") {
    $zipPath = (Resolve-Path -LiteralPath $Archive).Path
    Write-Say "使用离线归档：$zipPath"
  } else {
    $zipPath = Join-Path $Tmp $AngleArchive
    Write-Say "下载归档 ..."
    Write-Host "    $AngleSourceUrl"
    try {
      Invoke-WebRequest -UseBasicParsing -Uri $AngleSourceUrl -OutFile $zipPath -TimeoutSec 300
    } catch {
      Write-Err2 "下载失败：$($_.Exception.Message)"
      Write-Warn2 "手动方案："
      Write-Host "      1) 浏览器打开 $AngleSourceUrl 下载 $AngleArchive"
      Write-Host "      2) powershell -ExecutionPolicy Bypass -File .\scripts\fetch-angle.ps1 -Archive <zip路径>"
      Write-Host "      3) 或参考 docs/PLAYER_WINDOWS_ANGLE.md §3 的其他来源（Qt/Chromium ANGLE）"
      exit 1
    }
  }

  # ── 2) 校验归档 sha256 ──────────────────────────────────
  $actualSha = Get-Sha256 $zipPath
  if ($actualSha -ne $AngleArchiveSha) {
    Write-Err2 "归档 sha256 不匹配！"
    Write-Host "      期望：$AngleArchiveSha"
    Write-Host "      实际：$actualSha"
    Write-Host "  若上游发布更新（版本变化）：更新本脚本顶部 \`$AngleRelease / \`$AngleArchive / \`$AngleArchiveSha / \`$AngleDlls"
    exit 2
  }
  Write-Ok "归档 sha256 校验通过（$($actualSha.Substring(0, 16))…）"

  # ── 3) 提取 ANGLE DLL ───────────────────────────────────
  $extractDir = Join-Path $Tmp "extract"
  New-Item -ItemType Directory -Force -Path $extractDir | Out-Null
  $suffixes = @()
  foreach ($k in $AngleDlls.Keys) { $suffixes += "/$k" }
  $n = Extract-ZipEntries $zipPath $extractDir $suffixes
  if ($n -lt 2) {
    Write-Err2 "归档内未找到 ANGLE DLL（匹配到 $n 个）；预期角度路径 angle/out/Release/*.dll"
    exit 3
  }
  Write-Ok "已从归档提取 $n 个 DLL"

  # ── 4) 逐个校验 sha256 / PE 架构 / 复制 ──────────────────
  $copied = @()
  foreach ($k in $AngleDlls.Keys) {
    $d = $AngleDlls[$k]
    $src = Join-Path $extractDir $k
    if (-not (Test-Path $src)) {
      if ($d.optional) { Write-Warn2 "可选 DLL 缺失：$k（跳过）"; continue }
      Write-Err2 "必需 DLL 缺失：$k"
      exit 4
    }
    $sha = Get-Sha256 $src
    if ($sha -ne $d.sha256) {
      Write-Err2 "$k sha256 不匹配（期望 $($d.sha256.Substring(0, 16))… 实际 $($sha.Substring(0, 16))…）"
      exit 5
    }
    $size = (Get-Item $src).Length
    if ($size -ne $d.size) {
      Write-Warn2 "$k 大小与 pin 不一致（期望 $($d.size)，实际 $size）；继续但请复核 pin"
    }
    $pe = Test-PeX64 $src
    if (-not $pe.ok) {
      Write-Err2 "$k 不是 x86-64 PE：$($pe.detail)"
      exit 6
    }
    Copy-Item $src (Join-Path $OutDir $k) -Force
    $copied += $k
    Write-Ok "$k  sha256/PE 校验通过（$($pe.detail)，$([math]::Round($size / 1MB, 2)) MB）"
  }

  # ── 5) 许可证 + 来源说明 ────────────────────────────────
  $licDest = Join-Path $OutDir "ANGLE-LICENSE.txt"
  $licEntry = $null
  Add-Type -AssemblyName System.IO.Compression.FileSystem
  $zip2 = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
  try {
    foreach ($entry in $zip2.Entries) {
      if ($entry.FullName.Replace('\', '/') -match "(^|/)LICENSE$") { $licEntry = $entry; break }
    }
    if ($licEntry) {
      [System.IO.Compression.ZipFileExtensions]::ExtractToFile($licEntry, $licDest, $true)
      Write-Ok "已写出 ANGLE-LICENSE.txt（BSD-3-Clause）"
    } else {
      Write-Warn2 "归档内未找到 LICENSE；使用仓库 assets\licenses\ANGLE-BSD-3-Clause.txt 作为替代"
      Copy-Item (Join-Path $Root "assets\licenses\ANGLE-BSD-3-Clause.txt") $licDest -Force
    }
  } finally {
    $zip2.Dispose()
  }

  if ($WithHeaders) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip3 = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
    try {
      foreach ($entry in $zip3.Entries) {
        $p = $entry.FullName.Replace('\', '/')
        if ($p -match "^angle/include/(.+)$") {
          $rel = $Matches[1]
          $dest = Join-Path (Join-Path $OutDir "include") $rel
          New-Item -ItemType Directory -Force -Path (Split-Path -Parent $dest) | Out-Null
          [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $dest, $true)
        }
      }
    } finally {
      $zip3.Dispose()
    }
    Write-Ok "已附带 ANGLE include/ 头文件（-WithHeaders）"
  }

  $sourceInfo = @"
ANGLE 运行时来源与校验（Windows x86_64）
=========================================
生成时间：$(Get-Date -Format 'yyyy-MM-ddTHH:mm:sszzz')
来源仓库：$AngleRepo
发布版本：$AngleRelease
Chrome 版本：$AngleChromeVer
ANGLE commit：$AngleCommit
下载地址：$AngleSourceUrl
归档文件：$AngleArchive
归档 sha256：$AngleArchiveSha
提取自归档内：angle/out/Release/{ $($AngleDlls.Keys -join ', ') }
已复制 DLL：
$($copied | ForEach-Object { "  - $_  sha256=" + $AngleDlls[$_].sha256 } | Out-String)
ANGLE 许可证：BSD-3-Clause（见 ANGLE-LICENSE.txt；发行包 LICENSES/ANGLE-BSD-3-Clause.txt）
说明：仅提取 ANGLE 的 BSD-3 许可 DLL；该预编译包由 Chrome stable 构建链产出，
      本项目**不**复用其中的 mpv/Chrome 等其他组件，也不把本归档提交进仓库。
复现：powershell -ExecutionPolicy Bypass -File scripts\fetch-angle.ps1
"@
  $utf8 = New-Object System.Text.UTF8Encoding($false)
  [System.IO.File]::WriteAllText((Join-Path $OutDir "ANGLE-SOURCE.txt"), $sourceInfo, $utf8)
  Write-Ok "已写出 ANGLE-SOURCE.txt"

  Write-Say "完成：$OutDir"
  Get-ChildItem $OutDir -File | ForEach-Object {
    Write-Host ("    {0}  {1:N0} B" -f $_.Name, $_.Length)
  }
  Write-Host ""
  Write-Host "下一步："
  Write-Host "  powershell -ExecutionPolicy Bypass -File .\scripts\package-player-runtime-windows.ps1 -Generate -Check -Strict"
  Write-Host "  powershell -ExecutionPolicy Bypass -File .\scripts\windows-verify.ps1"
  exit 0
} finally {
  Remove-Item -LiteralPath $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}
