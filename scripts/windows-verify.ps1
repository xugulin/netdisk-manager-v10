# Windows 播放链路验证（W1–W7 清单自动/半自动）—— 输出 PASS/FAIL JSON
#
# 对照 docs/PLAYER_WINDOWS_ANGLE.md §5 的 W1–W7：
#   W1 GL 上下文 = EGL/ANGLE（半自动：有 -GlRendererLog 则自动判，否则要求人工确认）
#   W2 libmpv render API 出帧（render_probe JSON：frames_rendered>0 且有首帧）
#   W3 硬解 hwdec-current = d3d11va/nvdec/dxva2（mpv-probe JSON）
#   W4 4K HEVC 性能（mpv-probe 丢帧统计；需 -Sample4K，否则 SKIP）
#   W5 HDR（需 HDR 显示器；无 -HdrScreen 则 SKIP，仅记录建议步骤）
#   W6 无 ANGLE 回退（复制运行时→删 ANGLE DLL→再跑 render_probe，断言不崩溃）
#   W7 打包完整性（package-player-runtime-windows.ps1 -Check -Strict + exe/运行时布局）
#
# 用法（在 Windows PowerShell 5.1+/PowerShell 7，仓库根目录）：
#   powershell -ExecutionPolicy Bypass -File .\scripts\windows-verify.ps1
#   powershell -ExecutionPolicy Bypass -File .\scripts\windows-verify.ps1 -Sample1080 D:\test\1080p.mp4 -Sample4K D:\test\4k-hevc.mp4
#   powershell -ExecutionPolicy Bypass -File .\scripts\windows-verify.ps1 -SkipGl          # 无桌面会话/CI
#   powershell -ExecutionPolicy Bypass -File .\scripts\windows-verify.ps1 -GlRendererLog .\gl.log
#
# 产物：出\验收日志\windows-verify-report.json（含 host/checks/summary），退出码 0=无 FAIL。
#
# 说明：
#   * 需要先准备好 播放器运行时\（package-player-runtime-windows.ps1 -Generate）。
#   * W1/W2/W5/W6 需要 GL 上下文：远程/无桌面会话会失败 → 用 -SkipGl 跳过（CI 用）。
#   * 探针（mpv-probe.exe / render_probe.exe）默认用 cargo 构建（-SkipBuild 可跳过构建）。
#
#Requires -Version 5.1
param(
  [string]$RuntimeDir = "播放器运行时",
  [string]$AppExe = "",
  [string]$Sample1080 = "",
  [string]$Sample4K = "",
  [string]$GlRendererLog = "",
  [string]$OutJson = "出\验收日志\windows-verify-report.json",
  [int]$Seconds = 6,
  [switch]$SkipGl,
  [switch]$SkipBuild,
  [switch]$HdrScreen,
  [switch]$Help
)

$ErrorActionPreference = "Stop"
$script:Checks = New-Object System.Collections.ArrayList

function Write-Say($m)  { Write-Host "==> $m" -ForegroundColor Cyan }
function Write-Ok($m)   { Write-Host "  [OK]   $m" -ForegroundColor Green }
function Write-Warn2($m){ Write-Host "  [WARN] $m" -ForegroundColor Yellow }
function Write-Err2($m) { Write-Host "  [ERR]  $m" -ForegroundColor Red }
function Write-Skip($m) { Write-Host "  [SKIP] $m" -ForegroundColor DarkGray }

function Get-Root { return (Split-Path -Parent $PSScriptRoot) }

function Add-Check {
  param([string]$Id, [string]$Title, [string]$Status, [string]$Detail)
  $item = [ordered]@{ id = $Id; title = $Title; status = $Status; detail = $Detail }
  [void]$script:Checks.Add($item)
  switch ($Status) {
    "PASS" { Write-Ok   "$Id $Title —— $Detail" }
    "WARN" { Write-Warn2 "$Id $Title —— $Detail" }
    "SKIP" { Write-Skip "$Id $Title —— $Detail" }
    default { Write-Err2 "$Id $Title —— $Detail" }
  }
}

function Quote-Arg([string]$a) {
  if ($a -match '[\s"]') { return '"' + ($a -replace '"', '\"') + '"' }
  return $a
}

# 运行外部程序（带超时；返回 @{ code; stdout; stderr; timed_out }）
function Invoke-Tool {
  param([string]$Exe, [string[]]$ArgList, [int]$TimeoutSec = 180, [hashtable]$EnvMap = @{})
  $psi = New-Object System.Diagnostics.ProcessStartInfo
  $psi.FileName = $Exe
  $psi.Arguments = (($ArgList | ForEach-Object { Quote-Arg $_ }) -join ' ')
  $psi.UseShellExecute = $false
  $psi.RedirectStandardOutput = $true
  $psi.RedirectStandardError = $true
  $psi.CreateNoWindow = $true
  if ($EnvMap -and $EnvMap.Count -gt 0) {
    foreach ($k in $EnvMap.Keys) { $psi.EnvironmentVariables[$k] = [string]$EnvMap[$k] }
  }
  $p = New-Object System.Diagnostics.Process
  $p.StartInfo = $psi
  [void]$p.Start()
  $outTask = $p.StandardOutput.ReadToEndAsync()
  $errTask = $p.StandardError.ReadToEndAsync()
  $done = $p.WaitForExit($TimeoutSec * 1000)
  if (-not $done) {
    try { $p.Kill() } catch { }
    return @{ code = -999; stdout = ""; stderr = "TIMEOUT > $TimeoutSec s"; timed_out = $true }
  }
  return @{ code = $p.ExitCode; stdout = $outTask.Result; stderr = $errTask.Result; timed_out = $false }
}

# 从工具输出取最后一个 JSON 对象行
function Get-LastJson($text) {
  if (-not $text) { return $null }
  $lines = $text -split "`r?`n"
  for ($i = $lines.Count - 1; $i -ge 0; $i--) {
    $l = $lines[$i].Trim()
    if ($l.StartsWith("{") -and $l.EndsWith("}")) {
      try { return ($l | ConvertFrom-Json) } catch { }
    }
  }
  return $null
}

function Get-TargetDir {
  $Root = Get-Root
  try {
    $meta = & cargo metadata --no-deps --format-version 1 2>$null | ConvertFrom-Json
    if ($meta.target_directory) { return $meta.target_directory }
  } catch { }
  if ($env:CARGO_TARGET_DIR) { return $env:CARGO_TARGET_DIR }
  return (Join-Path $Root "target")
}

function Find-Probe([string]$kind) {
  $Root = Get-Root
  $td = Get-TargetDir
  $rel = if ($kind -eq "mpv-probe") { "debug\mpv-probe.exe" } else { "debug\examples\render_probe.exe" }
  $cand = Join-Path $td $rel
  if (Test-Path $cand) { return $cand }
  if ($script:SkipBuild) { return "" }
  Write-Say "构建探针 $kind（可能需数十秒）..."
  $cargoArgs = if ($kind -eq "mpv-probe") { @("build", "-q", "-p", "wp-media-mpv", "--bin", "mpv-probe") }
               else { @("build", "-q", "-p", "wp-media-mpv", "--example", "render_probe") }
  $r = Invoke-Tool -Exe "cargo" -ArgList $cargoArgs -TimeoutSec 900
  if ($r.code -ne 0) {
    Write-Warn2 "构建 $kind 失败：$($r.stderr)"
    return ""
  }
  if (Test-Path $cand) { return $cand }
  return ""
}

function Test-CrashExit([int]$code) {
  # Windows 常见崩溃码：0xC0000005(AV)/0xC0000409 等 → 有符号 int 为 -1073741819 / -1073740791
  return @(-1073741819, -1073740791, -1073741510) -contains $code
}

function Resolve-Existing([string]$p) {
  if ($p -and (Test-Path -LiteralPath $p)) { return (Resolve-Path -LiteralPath $p).Path }
  return ""
}

# ── 帮助 ──────────────────────────────────────────────────
if ($Help) {
  Get-Content $PSCommandPath | Select-Object -First 34 | ForEach-Object { $_ -replace '^# ?', '' }
  exit 0
}

$Root = Get-Root
Set-Location $Root
$RuntimePath = Resolve-Existing $RuntimeDir
if (-not $RuntimePath -and (Test-Path (Join-Path $Root $RuntimeDir))) {
  $RuntimePath = (Resolve-Path -LiteralPath (Join-Path $Root $RuntimeDir)).Path
}

Write-Say "Windows 播放链路验证（W1–W7）"
Write-Host "    仓库：$Root"
Write-Host "    运行时：$(if ($RuntimePath) { $RuntimePath } else { "$RuntimeDir（不存在）" })"
Write-Host "    输出报告：$OutJson"
if ($SkipGl) { Write-Host "    -SkipGl：跳过 GL 相关项（W1/W2/W5/W6；CI/无桌面会话）" }

# ── 主机信息 ──────────────────────────────────────────────
$hostInfo = [ordered]@{
  computer = $env:COMPUTERNAME
  user = $env:USERNAME
  os = ""
  ps_version = $PSVersionTable.PSVersion.ToString()
  arch = $env:PROCESSOR_ARCHITECTURE
  gpus = @()
}
try {
  $os = Get-CimInstance Win32_OperatingSystem -ErrorAction Stop
  $hostInfo.os = "$($os.Caption) $($os.Version) build $($os.BuildNumber)"
} catch { $hostInfo.os = "$([System.Environment]::OSVersion.VersionString)" }
try {
  $gpus = Get-CimInstance Win32_VideoController -ErrorAction Stop | ForEach-Object {
    [ordered]@{ name = $_.Name; driver = $_.DriverVersion; vram_mb = [math]::Round($_.AdapterRAM / 1MB) }
  }
  $hostInfo.gpus = @($gpus)
} catch { }
Write-Host "    OS：$($hostInfo.os)"
foreach ($g in $hostInfo.gpus) { Write-Host "    GPU：$($g.name)（驱动 $($g.driver)）" }
Write-Host ""

# ════════════════ W7：打包完整性（先跑，很多检查依赖它）════════════════
Write-Say "[W7] 打包完整性"
if (-not $RuntimePath) {
  Add-Check "W7" "打包完整性" "FAIL" "运行时目录不存在：$RuntimeDir（先跑 package-player-runtime-windows.ps1 -Generate）"
} else {
  $pkgScript = Join-Path $Root "scripts\package-player-runtime-windows.ps1"
  $pkgOut = ""
  if (Test-Path $pkgScript) {
    try {
      $pkgOut = (& $pkgScript -RuntimeDir $RuntimePath -Check -Strict 2>&1 | Out-String)
      $pkgCode = $LASTEXITCODE
    } catch {
      $pkgOut = $_.Exception.Message
      $pkgCode = 1
    }
    if ($pkgCode -eq 0) {
      Add-Check "W7a" "打包结构/许可证/DLL 清单" "PASS" "package-player-runtime-windows.ps1 -Check -Strict 通过"
    } else {
      Add-Check "W7a" "打包结构/许可证/DLL 清单" "FAIL" "package -Check 失败（$pkgCode）：$(($pkgOut -split "`r?`n" | Select-Object -Last 3) -join ' / ')"
    }
  } else {
    Add-Check "W7a" "打包结构/许可证/DLL 清单" "FAIL" "缺少 scripts\package-player-runtime-windows.ps1"
  }

  # 运行时关键文件
  $required = @("libmpv-2.dll", "licenses\LGPL-2.1.txt", "licenses\ANGLE-BSD-3-Clause.txt",
                "BUILDINFO.txt", "DEPENDENCIES.txt", "LOAD-ORDER.txt", "README.txt")
  $missing = @()
  foreach ($f in $required) {
    $full = Join-Path $RuntimePath ($f -replace '/', '\')
    if ($f -ieq "licenses\LGPL-2.1.txt" -or $f -like "licenses\*") {
      $full = Join-Path (Join-Path $RuntimePath "LICENSES") (Split-Path -Leaf $f)
    }
    if (-not (Test-Path $full)) { $missing += $f }
  }
  if ($missing.Count -eq 0) {
    Add-Check "W7b" "运行时必需文件" "PASS" "libmpv/LICENSE/BUILDINFO/DEPENDENCIES/LOAD-ORDER/README 齐全"
  } else {
    Add-Check "W7b" "运行时必需文件" "FAIL" ("缺少：" + ($missing -join ", "))
  }

  # exe 与运行时同级布局
  $exePath = Resolve-Existing $AppExe
  if (-not $exePath) {
    $guess = Join-Path $Root "出\网盘管理-win64\网盘管理.exe"
    if (Test-Path $guess) { $exePath = $guess }
  }
  if ($exePath) {
    $exeDir = Split-Path -Parent $exePath
    $rtSibling = Join-Path $exeDir "播放器运行时"
    if ((Test-Path $rtSibling) -and (Test-Path (Join-Path $rtSibling "libmpv-2.dll"))) {
      Add-Check "W7c" "exe 与 播放器运行时 同级布局" "PASS" "$exeDir\播放器运行时\libmpv-2.dll"
    } else {
      Add-Check "W7c" "exe 与 播放器运行时 同级布局" "FAIL" "$exeDir 下缺少 播放器运行时\libmpv-2.dll"
    }
  } else {
    Add-Check "W7c" "exe 与 播放器运行时 同级布局" "SKIP" "未提供 -AppExe 且未找到 出\网盘管理-win64\网盘管理.exe"
  }

  # ANGLE 是否就位（W1/W2/W6 的前置）
  $angleOk = (Test-Path (Join-Path $RuntimePath "libEGL.dll")) -and (Test-Path (Join-Path $RuntimePath "libGLESv2.dll"))
  if ($angleOk) {
    Add-Check "W7d" "ANGLE DLL 就位" "PASS" "libEGL.dll + libGLESv2.dll（先跑 scripts\fetch-angle.ps1）"
  } else {
    Add-Check "W7d" "ANGLE DLL 就位" "FAIL" "缺少 libEGL.dll/libGLESv2.dll（先跑 scripts\fetch-angle.ps1）"
  }
}

# ════════════════ 探针准备 ════════════════
$sample1080Path = Resolve-Existing $Sample1080
$sample4kPath = Resolve-Existing $Sample4K
$mpvProbe = ""
$renderProbe = ""
if (-not $SkipGl) {
  $renderProbe = Find-Probe "render_probe"
  $mpvProbe = Find-Probe "mpv-probe"
} else {
  $mpvProbe = Find-Probe "mpv-probe"
}
if ($sample1080Path) { Write-Host "    1080p 样本：$sample1080Path" } else { Write-Host "    1080p 样本：未提供（W1/W2/W3 将 SKIP）" }
if ($sample4kPath) { Write-Host "    4K 样本：$sample4kPath" } else { Write-Host "    4K 样本：未提供（W4 将 SKIP）" }
Write-Host ""

# ════════════════ W1/W2：GL 上下文 + render API 出帧 ════════════════
Write-Say "[W1/W2] GL 上下文（ANGLE）与 render API 出帧"
$renderOk = $false
$renderJson = $null
if ($SkipGl) {
  Add-Check "W1" "GL 上下文为 EGL/ANGLE" "SKIP" "-SkipGl（无桌面会话/CI）"
  Add-Check "W2" "libmpv render API 出帧" "SKIP" "-SkipGl（无桌面会话/CI）"
} elseif (-not $renderProbe) {
  Add-Check "W1" "GL 上下文为 EGL/ANGLE" "FAIL" "render_probe.exe 不可用（构建失败或 -SkipBuild）"
  Add-Check "W2" "libmpv render API 出帧" "FAIL" "render_probe.exe 不可用"
} elseif (-not $sample1080Path) {
  Add-Check "W1" "GL 上下文为 EGL/ANGLE" "SKIP" "未提供 -Sample1080（用任意本地 mp4）"
  Add-Check "W2" "libmpv render API 出帧" "SKIP" "未提供 -Sample1080"
} else {
  $envMap = @{}
  if ($RuntimePath) { $envMap["WP_MPV_LIB"] = (Join-Path $RuntimePath "libmpv-2.dll") }
  # 让 libmpv 的依赖 DLL 可被找到（Windows 默认只搜 exe 目录/System32/PATH）
  $envMap["PATH"] = "$RuntimePath;$env:PATH"
  $r = Invoke-Tool -Exe $renderProbe -ArgList @($sample1080Path, "--seconds", "$Seconds", "--json") -TimeoutSec 180 -EnvMap $envMap
  $renderJson = Get-LastJson $r.stdout
  if ($r.timed_out) {
    Add-Check "W2" "libmpv render API 出帧" "FAIL" "render_probe 超时（无桌面会话？可用 -SkipGl）"
  } elseif ($r.code -ne 0) {
    Add-Check "W2" "libmpv render API 出帧" "FAIL" "render_probe 退出码 $($r.code)：$((($r.stderr -split "`r?`n") | Select-Object -Last 2) -join ' / ')"
  } elseif (-not $renderJson) {
    Add-Check "W2" "libmpv render API 出帧" "FAIL" "render_probe 无 JSON 输出"
  } else {
    $frames = 0
    if ($renderJson.render -and $null -ne $renderJson.render.frames_rendered) { $frames = [int]$renderJson.render.frames_rendered }
    $firstFrame = $false
    if ($renderJson.render -and $renderJson.render.first_frame) { $firstFrame = [bool]$renderJson.render.first_frame }
    if ($frames -gt 0 -and $firstFrame) {
      $renderOk = $true
      Add-Check "W2" "libmpv render API 出帧" "PASS" "frames_rendered=$frames，first_frame=true，avg_fps=$([math]::Round([double]$renderJson.avg_fps, 2))"
    } else {
      Add-Check "W2" "libmpv render API 出帧" "FAIL" "frames_rendered=$frames，first_frame=$firstFrame（render.gl_error=$($renderJson.render.gl_error)）"
    }
  }

  # W1：半自动（有 GL_RENDERER 日志则自动判定）
  if ($GlRendererLog -and (Test-Path -LiteralPath $GlRendererLog)) {
    $glLog = Get-Content -LiteralPath $GlRendererLog -Raw
    if ($glLog -match "ANGLE" -or $glLog -match "GL_RENDERER[^\r\n]*ANGLE") {
      Add-Check "W1" "GL 上下文为 EGL/ANGLE" "PASS" "日志 GL_RENDERER 含 ANGLE（$GlRendererLog）"
    } elseif ($glLog -match "WGL") {
      Add-Check "W1" "GL 上下文为 EGL/ANGLE" "WARN" "日志显示 WGL 回退（非 ANGLE）：$GlRendererLog"
    } else {
      Add-Check "W1" "GL 上下文为 EGL/ANGLE" "WARN" "日志未匹配 ANGLE/WGL：$GlRendererLog"
    }
  } elseif ($renderOk) {
    Add-Check "W1" "GL 上下文为 EGL/ANGLE" "WARN" "半自动：render 出帧正常且 ANGLE DLL 存在，但需人工确认 GL_RENDERER 含 ANGLE（提供 -GlRendererLog 可自动判定）"
  } else {
    Add-Check "W1" "GL 上下文为 EGL/ANGLE" "FAIL" "GL 渲染未通过，无法判定上下文类型"
  }
}

# ════════════════ W3：硬解 hwdec ════════════════
Write-Say "[W3] 硬解 hwdec-current"
if (-not $mpvProbe) {
  # CI（-SkipBuild）不构建探针，W3 属真机项 → SKIP；真机跑时探针缺失才提示
  if ($SkipBuild) {
    Add-Check "W3" "硬解 hwdec-current" "SKIP" "-SkipBuild 未构建 mpv-probe（CI 无桌面/样本，W3 留待真机）"
  } else {
    Add-Check "W3" "硬解 hwdec-current" "WARN" "mpv-probe.exe 不可用（可去掉 -SkipBuild 让脚本 cargo 构建）"
  }
} elseif (-not $sample1080Path) {
  Add-Check "W3" "硬解 hwdec-current" "SKIP" "未提供 -Sample1080"
} else {
  $hdrArgs = @($sample1080Path, "--seconds", "$Seconds", "--json")
  $r = Invoke-Tool -Exe $mpvProbe -ArgList $hdrArgs -TimeoutSec 180 -EnvMap @{ "WP_MPV_LIB" = (Join-Path $RuntimePath "libmpv-2.dll"); "PATH" = "$RuntimePath;$env:PATH" }
  $j = Get-LastJson $r.stdout
  if (-not $j) {
    Add-Check "W3" "硬解 hwdec-current" "FAIL" "mpv-probe 无 JSON（退出码 $($r.code)）"
  } else {
    $hw = [string]$j.hwdec_current
    if ($hw -match "d3d11va|nvdec|dxva2|d3d11") {
      Add-Check "W3" "硬解 hwdec-current" "PASS" "hwdec-current=$hw（video=$($j.video_size)）"
    } elseif ([string]::IsNullOrWhiteSpace($hw)) {
      Add-Check "W3" "硬解 hwdec-current" "WARN" "hwdec-current 为空（可能软解；检查显卡驱动/ANGLE 上下文）"
    } else {
      Add-Check "W3" "硬解 hwdec-current" "WARN" "hwdec-current=$hw（非 d3d11va/nvdec；记录并评估）"
    }
  }
}

# ════════════════ W4：4K HEVC 性能 ════════════════
Write-Say "[W4] 4K HEVC 10bit 性能"
if (-not $mpvProbe) {
  if ($SkipBuild) {
    Add-Check "W4" "4K HEVC 性能" "SKIP" "-SkipBuild 未构建 mpv-probe（CI 无 4K 样本，W4 留待真机）"
  } else {
    Add-Check "W4" "4K HEVC 性能" "WARN" "mpv-probe.exe 不可用（可去掉 -SkipBuild 让脚本 cargo 构建）"
  }
} elseif (-not $sample4kPath) {
  Add-Check "W4" "4K HEVC 性能" "SKIP" "未提供 -Sample4K（需 4K HEVC 10bit 样本）"
} else {
  $r = Invoke-Tool -Exe $mpvProbe -ArgList @($sample4kPath, "--seconds", "10", "--json") -TimeoutSec 240 -EnvMap @{ "WP_MPV_LIB" = (Join-Path $RuntimePath "libmpv-2.dll"); "PATH" = "$RuntimePath;$env:PATH" }
  $j = Get-LastJson $r.stdout
  if (-not $j) {
    Add-Check "W4" "4K HEVC 性能" "FAIL" "mpv-probe 无 JSON（退出码 $($r.code)）"
  } else {
    $dropped = 0
    if ($null -ne $j.dropped_frames) { $dropped = [int]$j.dropped_frames }
    $vs = $j.video_size
    $is4k = $false
    if ($vs -and $vs.Count -ge 2) { $is4k = ([int]$vs[0] -ge 3000 -or [int]$vs[1] -ge 3000) }
    $detail = "dropped_frames=$dropped，video=$vs，hwdec=$($j.hwdec_current)，cpu_cores_avg=$([math]::Round([double]$j.cpu_cores_avg, 3))"
    if (-not $is4k) {
      Add-Check "W4" "4K HEVC 性能" "WARN" "$detail（样本非 4K，结论不完整）"
    } elseif ($dropped -le 0) {
      Add-Check "W4" "4K HEVC 性能" "PASS" "$detail"
    } elseif ($dropped -le 5) {
      Add-Check "W4" "4K HEVC 性能" "WARN" "$detail（轻微丢帧，复核 CPU/GPU 负载）"
    } else {
      Add-Check "W4" "4K HEVC 性能" "FAIL" "$detail（丢帧过多）"
    }
  }
}

# ════════════════ W5：HDR ════════════════
Write-Say "[W5] HDR"
if (-not $HdrScreen) {
  Add-Check "W5" "HDR（target-colorspace-hint）" "SKIP" "需 HDR 显示器；加 -HdrScreen 时记录播放结果（仍建议人工确认直通/tone-map）"
} elseif (-not $renderProbe -or -not $sample4kPath) {
  Add-Check "W5" "HDR（target-colorspace-hint）" "SKIP" "-HdrScreen 需要 -Sample4K 且可用 render_probe"
} else {
  $r = Invoke-Tool -Exe $renderProbe -ArgList @($sample4kPath, "--seconds", "$Seconds", "--json", "--prop", "target-colorspace-hint=auto") -TimeoutSec 180 -EnvMap @{ "WP_MPV_LIB" = (Join-Path $RuntimePath "libmpv-2.dll"); "PATH" = "$RuntimePath;$env:PATH" }
  $j = Get-LastJson $r.stdout
  if ($j -and $r.code -eq 0) {
    Add-Check "W5" "HDR（target-colorspace-hint）" "WARN" "已跑 HDR 样本（video=$($j.video_size)，hwdec=$($j.final_snapshot.hwdec)）；直通/tone-map 结果需人工记录到 docs"
  } else {
    Add-Check "W5" "HDR（target-colorspace-hint）" "FAIL" "HDR 样本播放失败（退出码 $($r.code)）"
  }
}

# ════════════════ W6：无 ANGLE 回退 ════════════════
Write-Say "[W6] 无 ANGLE 回退（不崩溃）"
if ($SkipGl) {
  Add-Check "W6" "无 ANGLE 回退" "SKIP" "-SkipGl（无桌面会话/CI）"
} elseif (-not $renderProbe -or -not $sample1080Path) {
  Add-Check "W6" "无 ANGLE 回退" "SKIP" "需要 render_probe 与 -Sample1080"
} else {
  $tmpRt = Join-Path ([System.IO.Path]::GetTempPath()) ("wp-noangle-" + [guid]::NewGuid().ToString("N").Substring(0, 8))
  try {
    Copy-Item -Recurse -Force -LiteralPath $RuntimePath -Destination $tmpRt
    foreach ($dll in @("libEGL.dll", "libGLESv2.dll")) {
      $p = Join-Path $tmpRt $dll
      if (Test-Path $p) { Remove-Item -LiteralPath $p -Force }
    }
    $r = Invoke-Tool -Exe $renderProbe -ArgList @($sample1080Path, "--seconds", "4", "--json") -TimeoutSec 120 -EnvMap @{ "WP_MPV_LIB" = (Join-Path $tmpRt "libmpv-2.dll"); "PATH" = "$tmpRt;$env:PATH" }
    if ($r.timed_out) {
      Add-Check "W6" "无 ANGLE 回退" "WARN" "移除 ANGLE 后 render_probe 超时（可能弹窗/无桌面会话）"
    } elseif (Test-CrashExit $r.code) {
      Add-Check "W6" "无 ANGLE 回退" "FAIL" "移除 ANGLE 后崩溃（退出码 $($r.code)）"
    } elseif ($r.code -eq 0) {
      Add-Check "W6" "无 ANGLE 回退" "PASS" "移除 ANGLE 后仍正常退出（WGL 回退，退出码 0）"
    } else {
      Add-Check "W6" "无 ANGLE 回退" "WARN" "移除 ANGLE 后退出了但非崩溃（退出码 $($r.code)）；记录错误信息供 UI 提示"
    }
  } finally {
    Remove-Item -LiteralPath $tmpRt -Recurse -Force -ErrorAction SilentlyContinue
  }
}

# ════════════════ 汇总 + JSON ════════════════
$pass = @($script:Checks | Where-Object { $_.status -eq "PASS" }).Count
$warn = @($script:Checks | Where-Object { $_.status -eq "WARN" }).Count
$fail = @($script:Checks | Where-Object { $_.status -eq "FAIL" }).Count
$skip = @($script:Checks | Where-Object { $_.status -eq "SKIP" }).Count

$report = [ordered]@{
  schema = 1
  generated_at = (Get-Date).ToString("yyyy-MM-ddTHH:mm:sszzz")
  runtime_dir = $RuntimePath
  app_exe = (Resolve-Existing $AppExe)
  host = $hostInfo
  checks = @($script:Checks)
  summary = [ordered]@{ pass = $pass; warn = $warn; fail = $fail; skip = $skip; total = @($script:Checks).Count }
}

$OutJsonPath = $OutJson
if (-not [System.IO.Path]::IsPathRooted($OutJsonPath)) { $OutJsonPath = Join-Path $Root $OutJsonPath }
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $OutJsonPath) | Out-Null
$utf8 = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText($OutJsonPath, ($report | ConvertTo-Json -Depth 10), $utf8)

Write-Host ""
Write-Say "结果：PASS=$pass WARN=$warn FAIL=$fail SKIP=$skip（共 $($report.summary.total) 项）"
Write-Host "    报告：$OutJsonPath"
Write-Host ""
Write-Host "仅 Windows 实测项：W1（GL_RENDERER 需人工确认）、W3（驱动相关）、W4（需 4K 样本与性能环境）、W5（需 HDR 显示器）、W6（无桌面会话时 SKIP）"
if ($fail -gt 0) {
  Write-Err2 "存在 $fail 项 FAIL"
  exit 1
}
exit 0
