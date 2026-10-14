# Builds the release exe and the Windows installer into .\dist
#   powershell -ExecutionPolicy Bypass -File installer\build.ps1 [-FFmpegBin C:\ffmpeg\bin]
param([string]$FFmpegBin = "")

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

if (-not $FFmpegBin) {
    $found = Get-Command ffmpeg -ErrorAction SilentlyContinue
    if (-not $found) { throw "FFmpeg not found: pass -FFmpegBin <folder with ffmpeg.exe and ffprobe.exe>" }
    $FFmpegBin = Split-Path -Parent $found.Source
}
foreach ($tool in "ffmpeg.exe", "ffprobe.exe") {
    if (-not (Test-Path (Join-Path $FFmpegBin $tool))) { throw "$tool is missing in $FFmpegBin" }
}

$iscc = @(
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 not found: winget install JRSoftware.InnoSetup" }

$version = ([regex]'(?m)^version\s*=\s*"([^"]+)"').Match((Get-Content Cargo.toml -Raw)).Groups[1].Value

New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item target\release\simple-converter.exe "dist\SimpleConverter-$version-portable.exe" -Force

& $iscc /Q "/DAppVersion=$version" "/DFFmpegBin=$FFmpegBin" installer\setup.iss
if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }

Get-ChildItem dist | Format-Table Name, @{ n = "MB"; e = { [math]::Round($_.Length / 1MB, 1) } } -AutoSize
