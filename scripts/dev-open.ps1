param(
    [Parameter(Mandatory = $false)]
    [ValidateSet("debug", "release")]
    [string]$Profile = "debug",
    [Parameter(Mandatory = $false)]
    [string]$DbPath = "",
    [Parameter(Mandatory = $false)]
    [switch]$NoBuild,
    [Parameter(Mandatory = $false)]
    [switch]$OnlyDaemon,
    [Parameter(Mandatory = $false)]
    [switch]$OnlyUi
)

$ErrorActionPreference = "Stop"

if ($OnlyDaemon -and $OnlyUi) {
    throw "OnlyDaemon and OnlyUi cannot be used together."
}

$projectRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
Push-Location $projectRoot

try {
    if (-not $DbPath) {
        if ($env:LOCALAPPDATA) {
            $DbPath = Join-Path $env:LOCALAPPDATA "LimeTrace\tracker.db"
        } else {
            $DbPath = "data\tracker.db"
        }
    }

    if (-not $NoBuild) {
        $buildArgs = @("build", "-p", "limetrace-backend", "-p", "limetrace")
        if ($Profile -eq "release") {
            $buildArgs += "--release"
        }

        Write-Host "Building binaries ($Profile)..."
        & cargo @buildArgs
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed with code $LASTEXITCODE"
        }
    }

    $binDir = Join-Path $projectRoot ("target\" + $Profile)
    $daemonExe = Join-Path $binDir "limetrace-backend.exe"
    $uiExe = Join-Path $binDir "limetrace.exe"

    if (-not $OnlyUi) {
        if (-not (Test-Path $daemonExe)) {
            throw "Missing file: $daemonExe"
        }
        Write-Host "Starting LimeTrace Backend..."
        Start-Process -FilePath $daemonExe -ArgumentList @("--db", $DbPath) -WorkingDirectory $binDir -WindowStyle Minimized | Out-Null
    }

    if (-not $OnlyDaemon) {
        if (-not (Test-Path $uiExe)) {
            throw "Missing file: $uiExe"
        }
        Write-Host "Starting LimeTrace..."
        Start-Process -FilePath $uiExe -ArgumentList @("--db", $DbPath) -WorkingDirectory $binDir | Out-Null
    }

    Write-Host ""
    Write-Host "Done."
    Write-Host "DB: $DbPath"
    Write-Host "Stop daemon command: taskkill /IM limetrace-backend.exe /F"
} finally {
    Pop-Location
}
