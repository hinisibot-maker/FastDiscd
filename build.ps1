# RustCord をビルドしてユーザー領域へインストールし、デスクトップにショートカットを作る
# 使い方: powershell -ExecutionPolicy Bypass -File .\build.ps1

$ErrorActionPreference = "Stop"

$Project = Split-Path -Parent $MyInvocation.MyCommand.Path
$SrcDir  = Join-Path $Project "src-tauri"
$Install = Join-Path $env:LOCALAPPDATA "Programs\RustCord"
$Exe     = Join-Path $Install "RustCord.exe"

$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

Push-Location $SrcDir
try {
    Write-Host "== cargo build --release ==" -ForegroundColor Cyan
    cargo build --release
} finally {
    Pop-Location
}

$built = Join-Path $SrcDir "target\release\rustcord.exe"
if (-not (Test-Path $built)) { throw "ビルド成果物が見つかりません: $built" }

Get-Process rustcord -ErrorAction SilentlyContinue | Stop-Process -Force
New-Item -ItemType Directory -Force -Path $Install | Out-Null
Copy-Item $built $Exe -Force
Write-Host "installed: $Exe" -ForegroundColor Green

$desktop = [Environment]::GetFolderPath("Desktop")
$lnkPath = Join-Path $desktop "RustCord (Discord).lnk"
if (-not (Test-Path $lnkPath)) {
    $ws = New-Object -ComObject WScript.Shell
    $lnk = $ws.CreateShortcut($lnkPath)
    $lnk.TargetPath = $Exe
    $lnk.WorkingDirectory = $Install
    $lnk.IconLocation = "$Exe,0"
    $lnk.Description = "Discord を軽量な Rust/WebView2 で動かすクライアント"
    $lnk.Save()
    Write-Host "shortcut: $lnkPath" -ForegroundColor Green
}

Write-Host "起動するにはデスクトップの 'RustCord (Discord)' をダブルクリックしてください。" -ForegroundColor Yellow
