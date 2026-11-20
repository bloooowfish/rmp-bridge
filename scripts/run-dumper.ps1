[CmdletBinding()]
param(
    [string]$SourceDir = "D:\ReMakePlace_Latest",
    [int]$TimeoutSeconds = 35
)

$ErrorActionPreference = "Stop"

$base = Split-Path -Parent $PSScriptRoot
$prepareScript = Join-Path $base "scripts\prepare-shadow.ps1"
$dumpsDir = Join-Path $base "dumps"
$doneFlag = Join-Path $dumpsDir "dump_done.flag"
$shadowExe = Join-Path $base "runtime\MakePlace\Binaries\Win64\MakePlace-Win64-Shipping.exe"
$ue4ssLog = Join-Path $base "runtime\MakePlace\Binaries\Win64\ue4ss\UE4SS.log"

& $prepareScript -SourceDir $SourceDir -EnableDumper
if ($LASTEXITCODE -ne 0) {
    throw "prepare-shadow.ps1 failed with exit code $LASTEXITCODE."
}

if (Test-Path $doneFlag) { Remove-Item -Force $doneFlag }
if (Test-Path $ue4ssLog) { Remove-Item -Force $ue4ssLog }

$sourceWin64 = Join-Path $SourceDir "MakePlace\Binaries\Win64"
$beforeSourceFiles = (Get-ChildItem -Path $sourceWin64 -Recurse | Select-Object -ExpandProperty FullName) -join "`n"

$proc = Start-Process -FilePath $shadowExe -ArgumentList "-windowed -ResX=800 -ResY=600" -WorkingDirectory (Split-Path -Parent $shadowExe) -PassThru
Write-Host "Started shadow MakePlace-Win64-Shipping.exe (PID=$($proc.Id)); waiting up to ${TimeoutSeconds}s..."

$deadline = (Get-Date).AddSeconds($TimeoutSeconds)
$completed = $false
try {
    while ((Get-Date) -lt $deadline) {
        if ($proc.HasExited) {
            Write-Host "Process exited early with code $($proc.ExitCode)."
            break
        }
        if (Test-Path $doneFlag) {
            $completed = $true
            Start-Sleep -Milliseconds 1500
            break
        }
        Start-Sleep -Milliseconds 500
    }
} finally {
    if (-not $proc.HasExited) {
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        $proc.WaitForExit(5000) | Out-Null
        Write-Host "Stopped shadow MakePlace process PID=$($proc.Id)."
    }
}

$afterSourceFiles = (Get-ChildItem -Path $sourceWin64 -Recurse | Select-Object -ExpandProperty FullName) -join "`n"
$zeroTouchOk = ($beforeSourceFiles -eq $afterSourceFiles)

Write-Host "Dump completed=$completed, SourceWin64 untouched=$zeroTouchOk"
if (Test-Path $ue4ssLog) {
    Write-Host "=== UE4SS.log ==="
    Get-Content -Path $ue4ssLog
} else {
    Write-Warning "UE4SS.log was not created."
}
if (-not $completed) {
    throw "FoundationDumper did not produce dump_done.flag within ${TimeoutSeconds}s."
}
