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
if (-not $Rev) {
  $Rev = (Invoke-WebRequest -UseBasicParsing "https://storage.googleapis.com/chromium-browser-snapshots/Win_x64/LAST_CHANGE").Content.Trim()
}
if (-not $Rev) { throw "取 Chromium 版本号失败（网络？）" }

$Url = "https://storage.googleapis.com/chromium-browser-snapshots/Win_x64/$Rev/chrome-win.zip"
$Cache = Join-Path $Root ".cache"
New-Item -ItemType Directory -Force -Path $Cache | Out-Null
$Zip = Join-Path $Cache "chrome-win-$Rev.zip"

if (-not (Test-Path $Zip) -or (Get-Item $Zip).Length -eq 0) {
  Write-Host "==> 下载 Chromium 快照 $Rev（约 250MB）"
  curl.exe -L --retry 5 -C - --progress-bar -o $Zip $Url
}
if (-not (Test-Path $Zip)) { throw "下载失败：$Zip" }

Write-Host "==> 解包并精简"
$Tmp = Join-Path $Cache "chrome-win-unpack-$Rev"
if (Test-Path $Tmp) { Remove-Item -Recurse -Force $Tmp }
Expand-Archive -Path $Zip -DestinationPath $Tmp -Force
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
