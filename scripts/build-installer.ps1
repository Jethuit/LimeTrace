param(
    [Parameter(Mandatory = $false)]
    [string]$ScriptPath = "installer/LimeTrace.iss"
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $ScriptPath)) {
    throw "Inno Setup script not found: $ScriptPath"
}

try {
    $isccCmd = Get-Command ISCC.exe -ErrorAction Stop
    $iscc = $isccCmd.Source
} catch {
    $iscc = $null
}

if (-not $iscc) {
    $candidatePaths = @(
        "$env:ProgramFiles(x86)\Inno Setup 7\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 7\ISCC.exe",
        "$env:ProgramFiles(x86)\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
        "$env:ChocolateyInstall\lib\innosetup\tools\ISCC.exe",
        "$env:ProgramData\chocolatey\lib\innosetup\tools\ISCC.exe"
    )
    $iscc = $candidatePaths | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
}

if (-not $iscc) {
    throw "ISCC.exe not found. Install Inno Setup first."
}

Write-Host "Using ISCC: $iscc"

$scriptFullPath = Resolve-Path $ScriptPath
& $iscc $scriptFullPath

if ($LASTEXITCODE -ne 0) {
    throw "ISCC failed with code $LASTEXITCODE"
}

Write-Host "Installer built successfully."
