[CmdletBinding()]
param(
    [string]$SourceDir = "D:\ReMakePlace_Latest",
    [string]$ApiUrl = "http://127.0.0.1:21064",
    [int]$LandIdent = 0,
    [switch]$NoBuild
)

$ErrorActionPreference = "Stop"

$bridgeRoot = Split-Path -Parent $PSScriptRoot
$prepareScript = Join-Path $PSScriptRoot "prepare-shadow.ps1"
$relayManifest = Join-Path $bridgeRoot "relay\Cargo.toml"
$relayExe = Join-Path $bridgeRoot "relay\target\release\rmp-bridge-relay.exe"
$shadowExe = Join-Path $bridgeRoot "runtime\MakePlace\Binaries\Win64\MakePlace-Win64-Shipping.exe"
$shadowWorkDir = Split-Path -Parent $shadowExe
$ipcDir = Join-Path $bridgeRoot "runtime\bridge_ipc"

Write-Host "[1/3] Preparing zero-touch shadow ReMakePlace runtime..."
& $prepareScript -SourceDir $SourceDir

if (-not $NoBuild -or -not (Test-Path $relayExe)) {
    Write-Host "[2/3] Building rmp-bridge-relay (release)..."
    & cargo build --manifest-path $relayManifest --release
    if ($LASTEXITCODE -ne 0) {
        throw "Failed building rmp-bridge-relay"
    }
}

$relayArgs = @("--api-url", $ApiUrl, "--ipc-dir", $ipcDir, "--parent-pid", "$PID")
if ($LandIdent -gt 0) {
    $relayArgs += @("--land-ident", "$LandIdent")
}

Write-Host "[3/3] Starting rmp-bridge-relay and shadow ReMakePlace..."
$relayProc = $null
try {
    $relayProc = Start-Process -FilePath $relayExe -ArgumentList $relayArgs -NoNewWindow -PassThru
    $rmpProc = Start-Process -FilePath $shadowExe -WorkingDirectory $shadowWorkDir -PassThru
    Wait-Process -Id $rmpProc.Id
} finally {
    if ($null -ne $relayProc -and -not $relayProc.HasExited) {
        Stop-Process -Id $relayProc.Id -Force -ErrorAction SilentlyContinue
    }
}
