param(
    [Parameter(Mandatory = $false)]
    [string]$BuildDir = "target/release",
    [Parameter(Mandatory = $false)]
    [string]$OutDir = "dist/windows"
)

$ErrorActionPreference = "Stop"

$buildRoot = Resolve-Path $BuildDir
$outRoot = Join-Path (Get-Location) $OutDir
$packageDir = Join-Path $outRoot "limetrace-windows"

if (Test-Path $packageDir) {
    Remove-Item -Path $packageDir -Recurse -Force
}
New-Item -ItemType Directory -Path $packageDir -Force | Out-Null

$daemonExe = Join-Path $buildRoot "limetrace-backend.exe"
$uiExe = Join-Path $buildRoot "limetrace.exe"

if (-not (Test-Path $daemonExe)) {
    throw "Missing file: $daemonExe"
}
if (-not (Test-Path $uiExe)) {
    throw "Missing file: $uiExe"
}

Copy-Item $daemonExe (Join-Path $packageDir "limetrace-backend.exe") -Force
Copy-Item $uiExe (Join-Path $packageDir "limetrace.exe") -Force

if (Test-Path "README.md") {
    Copy-Item "README.md" (Join-Path $packageDir "README.md") -Force
}

@"
@echo off
setlocal
cd /d %~dp0
if not exist logs mkdir logs
start "" /min limetrace-backend.exe >> logs\limetrace-backend.log 2>&1
echo LimeTrace Backend started.
"@ | Set-Content -Path (Join-Path $packageDir "Start LimeTrace Backend.cmd") -Encoding ASCII

@"
@echo off
setlocal
cd /d %~dp0
start "" limetrace.exe
"@ | Set-Content -Path (Join-Path $packageDir "Open LimeTrace.cmd") -Encoding ASCII

@"
@echo off
taskkill /IM limetrace-backend.exe /F
"@ | Set-Content -Path (Join-Path $packageDir "Stop LimeTrace Backend.cmd") -Encoding ASCII

$zipPath = Join-Path $outRoot "limetrace-windows.zip"
if (Test-Path $zipPath) {
    Remove-Item $zipPath -Force
}
Compress-Archive -Path (Join-Path $packageDir "*") -DestinationPath $zipPath -Force

Write-Host "Package created:"
Write-Host "  Folder: $packageDir"
Write-Host "  Zip:    $zipPath"
