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
  cargo build -p wp-gui
} else {
  cargo build --release -p wp-gui
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
  if (Test-Path $out) { Remove-Item -Recurse -Force $out }
  New-Item -ItemType Directory -Path $out | Out-Null
  Copy-Item $bin (Join-Path $out "网盘管理.exe")
  Copy-Item "docs" (Join-Path $out "docs") -Recurse -ErrorAction SilentlyContinue
  Copy-Item "README.md" $out -ErrorAction SilentlyContinue
  # 绿色版标记：数据写在 exe 旁（U 盘/解压即用）
  New-Item -ItemType File -Path (Join-Path $out "portable") | Out-Null
  Write-Host "==> 绿色版目录：$out" -ForegroundColor Green
  Write-Host "    运行：$out\网盘管理.exe --demo"
}
