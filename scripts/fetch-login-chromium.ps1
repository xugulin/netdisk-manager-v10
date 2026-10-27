# 下载并精简 Chromium（Windows 版）—— 只用于登录窗口的「登录内核」。
#
# 用法（PowerShell）：
#   .\scripts\fetch-login-chromium.ps1              # 取官方最新快照
#   .\scripts\fetch-login-chromium.ps1 -Rev 1728157 # 固定 revision
#
# 产出：<仓库>\登录内核\（chrome.exe + 必需 dll/资源，约 400MB）
$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $Root

# 简单参数解析（-Rev <revision>）
$Rev = $null
for ($i = 0; $i -lt $args.Count; $i++) {
  if ($args[$i] -eq "-Rev" -and $i + 1 -lt $args.Count) { $Rev = $args[$i + 1] }
}
$Base = "https://storage.googleapis.com/chromium-browser-snapshots/Win_x64"
# Windows 快照的 revision 与 Linux 不同：优先用 scripts/login-kernel-win.rev 固定的版本
$RevFile = Join-Path $Root "scripts/login-kernel-win.rev"
if (-not $Rev -and (Test-Path $RevFile)) { $Rev = (Get-Content $RevFile -Raw).Trim() }
if (-not $Rev) {
  $Rev = (Invoke-WebRequest -UseBasicParsing "$Base/LAST_CHANGE").Content.Trim()
}
if (-not $Rev) { throw "取 Chromium 版本号失败（网络？）" }

$Url = "$Base/$Rev/chrome-win.zip"
$Cache = Join-Path $Root ".cache"
New-Item -ItemType Directory -Force -Path $Cache | Out-Null
$Zip = Join-Path $Cache "chrome-win-$Rev.zip"

if (-not (Test-Path $Zip) -or (Get-Item $Zip).Length -eq 0) {
  Write-Host "==> 下载 Chromium 快照 $Rev（Win_x64，约 250MB）"
  curl.exe -fL --retry 5 --progress-bar -o $Zip $Url
  if ($LASTEXITCODE -ne 0) { Write-Host "==> 下载退出码 $LASTEXITCODE（固定 revision 可能在该架构无快照）" }
}
# 固定 revision 在 Win_x64 不存在时（404），回退到该架构最新快照
if ((-not (Test-Path $Zip)) -or ((Get-Item $Zip).Length -lt 50MB)) {
  $Last = (Invoke-WebRequest -UseBasicParsing "$Base/LAST_CHANGE").Content.Trim()
  if ($Last -and $Last -ne $Rev) {
    Write-Host "==> 固定 revision $Rev 不可用，回退 Win_x64 最新 $Last"
    $Rev = $Last
    $Url = "$Base/$Rev/chrome-win.zip"
    $Zip = Join-Path $Cache "chrome-win-$Rev.zip"
    curl.exe -fL --retry 5 --progress-bar -o $Zip $Url
  }
}
if (-not (Test-Path $Zip)) { throw "下载失败：$Zip（url=$Url）" }

$ZipLen = (Get-Item $Zip).Length
Write-Host ("==> 压缩包大小 {0:N1} MB" -f ($ZipLen / 1MB))
if ($ZipLen -lt 50MB) { throw "下载文件过小（$ZipLen 字节），可能下载失败：$Zip" }

Write-Host "==> 解包并精简"
$Tmp = Join-Path $Cache "chrome-win-unpack-$Rev"
if (Test-Path $Tmp) { Remove-Item -Recurse -Force $Tmp }
New-Item -ItemType Directory -Path $Tmp -Force | Out-Null
# 优先用 7-Zip / bsdtar 解包（Expand-Archive 在部分 pwsh 环境下会抛空异常）
try {
  $sevenZip = Join-Path $env:ProgramFiles "7-Zip\7z.exe"
  if (Test-Path $sevenZip) {
    Write-Host "==> 用 7-Zip 解包"
    & $sevenZip x $Zip "-o$Tmp" -y | Out-Null
  } elseif (Get-Command tar.exe -ErrorAction SilentlyContinue) {
    Write-Host "==> 用 tar(bsdtar) 解包"
    & tar.exe -xf $Zip -C $Tmp
  } else {
    Write-Host "==> 用 Expand-Archive 解包"
    Expand-Archive -Path $Zip -DestinationPath $Tmp -Force
  }
} catch {
  Write-Host "解包失败：$($_.Exception.Message)"
  throw
}
$Src = Join-Path $Tmp "chrome-win"
if (-not (Test-Path (Join-Path $Src "chrome.exe"))) { throw "解包后没找到 chrome.exe" }

$Out = Join-Path $Root "登录内核"
if (Test-Path $Out) { Remove-Item -Recurse -Force $Out }
New-Item -ItemType Directory -Path $Out | Out-Null

# Windows 版依赖较多 dll，保守做法：整目录复制，只裁语言包/调试文件
Copy-Item (Join-Path $Src "*") $Out -Recurse -Force
Get-ChildItem (Join-Path $Out "locales") -ErrorAction SilentlyContinue |
  Where-Object { $_.Name -notin @("zh-CN.pak", "en-US.pak") } |
  Remove-Item -Force -ErrorAction SilentlyContinue
Get-ChildItem $Out -Recurse -Include *.pdb,*.debug -ErrorAction SilentlyContinue |
  Remove-Item -Force -ErrorAction SilentlyContinue

$Version = & (Join-Path $Out "chrome.exe") --version 2>$null
@(
  "chromium-snapshot $Rev",
  "source $Url",
  "exe-version $Version"
) | Set-Content -Encoding UTF8 (Join-Path $Out "VERSION.txt")

Remove-Item -Recurse -Force $Tmp
$Size = "{0:N0} MB" -f ((Get-ChildItem $Out -Recurse | Measure-Object Length -Sum).Sum / 1MB)
Write-Host "==> 完成：$Out（$Size）"
