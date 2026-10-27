# Windows 10/11 构建脚本（PowerShell）：产出单文件 exe（绿色版）。
#
# 用法（在项目根目录打开 PowerShell）：
#   powershell -ExecutionPolicy Bypass -File .\scripts\build-windows.ps1
#   powershell -ExecutionPolicy Bypass -File .\scripts\build-windows.ps1 -Portable
#
# 前置：
#   1) 安装 Rust（https://rustup.rs），默认 MSVC 工具链；
#   2) 安装 Visual Studio Build Tools（含“使用 C++ 的桌面开发”）；
#   3) 无需 GTK/Qt 等系统 GUI 依赖（egui 自带窗口/渲染依赖，走系统 OpenGL）。
param(
  [switch]$Debug,
  [switch]$Portable
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $root

Write-Host "==> 构建" -ForegroundColor Cyan
if ($Debug) {
  cargo build -p wp-gui -p wp-cli
} else {
  cargo build --release -p wp-gui -p wp-cli
}

# 用 cargo metadata 解析真实产物路径（尊重 CARGO_TARGET_DIR）
$meta = cargo metadata --format-version 1 --no-deps | ConvertFrom-Json
$mode = if ($Debug) { "debug" } else { "release" }
$bin = Join-Path $meta.target_directory (Join-Path $mode "\u7f51\u76d8\u7ba1\u7406.exe")
if (-not (Test-Path $bin)) { throw "构建产物不存在：$bin" }
Write-Host "==> 产物：$bin" -ForegroundColor Green
Get-Item $bin | Format-List Length, FullName

if ($Portable) {
  $out = Join-Path $root "出\网盘管理-win64"
  # 先打包到临时目录再整体替换：不能直接删 $out（里面可能有用户 数据\：账号/日志/下载）
  $stage = Join-Path $root "出\.网盘管理-win64.打包临时.$PID"
  if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
  New-Item -ItemType Directory -Path $stage | Out-Null
  $outForData = $out
  $out = $stage
  New-Item -ItemType Directory -Path $out -Force | Out-Null
  Copy-Item $bin (Join-Path $out "网盘管理.exe")
  $cli = Join-Path $meta.target_directory (Join-Path $mode "wp-cli.exe")
  if (Test-Path $cli) { Copy-Item $cli (Join-Path $out "wp-cli.exe") }
  Copy-Item "docs" (Join-Path $out "docs") -Recurse -ErrorAction SilentlyContinue
  Copy-Item "README.md" $out -ErrorAction SilentlyContinue
  if (Test-Path "assets/icons") {
    $iconOut = Join-Path $out "图标"
    New-Item -ItemType Directory -Force -Path $iconOut | Out-Null
    Copy-Item "assets/icons/netdisk-manager.ico" $iconOut -ErrorAction SilentlyContinue
    Copy-Item "assets/icons/netdisk-manager*.png" $iconOut -ErrorAction SilentlyContinue
    Copy-Item "assets/icons/hero.png","assets/icons/preview.png" $iconOut -ErrorAction SilentlyContinue
  }
  # 内置登录内核（Chromium for Windows）
  $kernel = Join-Path $root "登录内核"
  if (Test-Path $kernel) {
    Copy-Item $kernel (Join-Path $out "登录内核") -Recurse -Force
    Write-Host "==> 已打包登录内核"
  } else {
    Write-Host "==> 提示：未找到 登录内核\，登录窗口不可用；先跑 .\scripts\fetch-login-chromium.ps1" -ForegroundColor Yellow
  }
  # 绿色版标记：数据写在 exe 旁（U 盘/解压即用）
  New-Item -ItemType File -Path (Join-Path $out "portable") | Out-Null
  # 保留用户数据目录
  $dataOld = Join-Path $outForData "数据"
  if (Test-Path $dataOld) {
    Move-Item $dataOld (Join-Path $stage "数据")
    Write-Host "==> 已保留数据目录"
  }
  if (Test-Path $outForData) { Remove-Item -Recurse -Force $outForData }
  Move-Item $stage $outForData
  Write-Host "==> 绿色版目录：$outForData" -ForegroundColor Green
  Write-Host "    运行：$outForData\网盘管理.exe --demo"
}
