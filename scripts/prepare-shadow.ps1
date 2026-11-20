[CmdletBinding()]
param(
    [string]$SourceDir = "D:\ReMakePlace_Latest",
    [string]$ShadowDir = "",
    [string]$Ue4ssDir = "",
    [string]$ModsSourceDir = "",
    [string]$DumpsDir = "",
    [switch]$EnableDumper,
    [switch]$DisableBridge
)

$ErrorActionPreference = "Stop"

$bridgeRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($ShadowDir)) { $ShadowDir = Join-Path $bridgeRoot "runtime" }
if ([string]::IsNullOrWhiteSpace($Ue4ssDir)) { $Ue4ssDir = Join-Path $bridgeRoot "vendor\ue4ss-v4.0.0" }
if ([string]::IsNullOrWhiteSpace($ModsSourceDir)) { $ModsSourceDir = Join-Path $bridgeRoot "mods" }
if ([string]::IsNullOrWhiteSpace($DumpsDir)) { $DumpsDir = Join-Path $bridgeRoot "dumps" }

$Ue4ssVersion = "v4.0.0"
$Ue4ssDownloadUrl = "https://github.com/UE4SS-RE/RE-UE4SS/releases/download/$Ue4ssVersion/UE4SS_$Ue4ssVersion.zip"
$Ue4ssExpectedSha256 = "251AD0D1381A241750E036F5918CBF2B5D7EC58EBC608CB2DD09D459D0C81E45"

$sourceWin64 = Join-Path $SourceDir "MakePlace\Binaries\Win64"
$sourceExe = Join-Path $sourceWin64 "MakePlace-Win64-Shipping.exe"
if (-not (Test-Path $sourceExe)) {
    throw "Source MakePlace executable not found at '$sourceExe'."
}

$dwmapiPath = Join-Path $Ue4ssDir "dwmapi.dll"
$ue4ssDllPath = Join-Path $Ue4ssDir "ue4ss\UE4SS.dll"
if (-not (Test-Path $dwmapiPath) -or -not (Test-Path $ue4ssDllPath)) {
    Write-Host "RE-UE4SS $Ue4ssVersion not found under '$Ue4ssDir'; downloading official release..."
    New-Item -ItemType Directory -Force -Path $Ue4ssDir | Out-Null
    $tmpZip = Join-Path ([System.IO.Path]::GetTempPath()) ("UE4SS_{0}_{1}.zip" -f $Ue4ssVersion, [System.Guid]::NewGuid().ToString("N"))
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 -bor [Net.ServicePointManager]::SecurityProtocol
        Invoke-WebRequest -Uri $Ue4ssDownloadUrl -OutFile $tmpZip -UseBasicParsing
        $actualHash = (Get-FileHash -Path $tmpZip -Algorithm SHA256).Hash
        if ($actualHash -ne $Ue4ssExpectedSha256) {
            throw "SHA-256 mismatch for downloaded $Ue4ssDownloadUrl (expected $Ue4ssExpectedSha256, got $actualHash)."
        }
        Expand-Archive -Path $tmpZip -DestinationPath $Ue4ssDir -Force
    } finally {
        if (Test-Path $tmpZip) {
            Remove-Item -Path $tmpZip -Force -ErrorAction SilentlyContinue
        }
    }
}

if (-not (Test-Path $dwmapiPath) -or -not (Test-Path $ue4ssDllPath)) {
    throw "Extracted UE4SS files not found under '$Ue4ssDir'."
}

function Ensure-DirectoryJunction {
    param(
        [Parameter(Mandatory = $true)][string]$LinkPath,
        [Parameter(Mandatory = $true)][string]$TargetPath
    )

    if (-not (Test-Path $TargetPath)) {
        throw "Junction target '$TargetPath' does not exist."
    }

    $parent = Split-Path -Parent $LinkPath
    if (-not (Test-Path $parent)) {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    }

    $resolvedTarget = [System.IO.Path]::GetFullPath($TargetPath).TrimEnd('\')
    if (Test-Path $LinkPath) {
        $item = Get-Item -Force $LinkPath
        if ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
            $existingTargets = @($item.Target)
            foreach ($t in $existingTargets) {
                $norm = [System.IO.Path]::GetFullPath($t).TrimEnd('\')
                if ([string]::Equals($norm, $resolvedTarget, [System.StringComparison]::OrdinalIgnoreCase)) {
                    return
                }
            }
            cmd /c rmdir "`"$LinkPath`"" | Out-Null
        } else {
            throw "Path '$LinkPath' exists and is not a reparse point; refusing to overwrite."
        }
    }

    $output = cmd /c mklink /J "`"$LinkPath`"" "`"$resolvedTarget`"" 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to create junction '$LinkPath' -> '$resolvedTarget': $output"
    }
}

$shadowMakePlace = Join-Path $ShadowDir "MakePlace"
$shadowWin64 = Join-Path $shadowMakePlace "Binaries\Win64"
$shadowIpcDir = Join-Path $ShadowDir "bridge_ipc"
New-Item -ItemType Directory -Force -Path $shadowWin64 | Out-Null
New-Item -ItemType Directory -Force -Path $DumpsDir | Out-Null
New-Item -ItemType Directory -Force -Path $shadowIpcDir | Out-Null

# 1. Create NTFS directory junctions back to read-only ReMakePlace asset/read-only directories.
#    MakePlace\Save is intentionally kept as a local shadow copy (never a junction) so any save/autosave
#    never writes into the original ReMakePlace folder.
$junctions = @(
    @{ Link = (Join-Path $ShadowDir "Engine"); Target = (Join-Path $SourceDir "Engine") },
    @{ Link = (Join-Path $shadowMakePlace "Content"); Target = (Join-Path $SourceDir "MakePlace\Content") },
    @{ Link = (Join-Path $shadowMakePlace "Custom"); Target = (Join-Path $SourceDir "MakePlace\Custom") },
    @{ Link = (Join-Path $shadowMakePlace "Download"); Target = (Join-Path $SourceDir "MakePlace\Download") },
    @{ Link = (Join-Path $shadowWin64 "D3D12"); Target = (Join-Path $sourceWin64 "D3D12") }
)
foreach ($j in $junctions) {
    Ensure-DirectoryJunction -LinkPath $j.Link -TargetPath $j.Target
}

$sourceSaveDir = Join-Path $SourceDir "MakePlace\Save"
$shadowSaveDir = Join-Path $shadowMakePlace "Save"
if (Test-Path $shadowSaveDir) {
    $saveItem = Get-Item -Force $shadowSaveDir
    if ($saveItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
        cmd /c rmdir "`"$shadowSaveDir`"" | Out-Null
    }
}
New-Item -ItemType Directory -Force -Path $shadowSaveDir | Out-Null
if (Test-Path $sourceSaveDir) {
    Get-ChildItem -Path $sourceSaveDir -File | ForEach-Object {
        $dstSave = Join-Path $shadowSaveDir $_.Name
        if (-not (Test-Path $dstSave)) {
            Copy-Item -Path $_.FullName -Destination $dstSave
        }
    }
}

# 2. Copy Win64 binary files only when missing or size/mtime differs.
$binFiles = Get-ChildItem -Path $sourceWin64 -File
foreach ($f in $binFiles) {
    $dst = Join-Path $shadowWin64 $f.Name
    $needsCopy = $true
    if (Test-Path $dst) {
        $existing = Get-Item $dst
        if ($existing.Length -eq $f.Length -and $existing.LastWriteTimeUtc -eq $f.LastWriteTimeUtc) {
            $needsCopy = $false
        }
    }
    if ($needsCopy) {
        Copy-Item -Path $f.FullName -Destination $dst -Force
    }
}

# 3. Copy config.json if missing in shadow directory.
$srcCfg = Join-Path $SourceDir "MakePlace\config.json"
$dstCfg = Join-Path $shadowMakePlace "config.json"
if ((Test-Path $srcCfg) -and -not (Test-Path $dstCfg)) {
    Copy-Item -Path $srcCfg -Destination $dstCfg -Force
}

# 4. Install UE4SS into shadow Binaries\Win64.
Copy-Item -Path (Join-Path $Ue4ssDir "dwmapi.dll") -Destination (Join-Path $shadowWin64 "dwmapi.dll") -Force
$shadowUe4ss = Join-Path $shadowWin64 "ue4ss"
New-Item -ItemType Directory -Force -Path $shadowUe4ss | Out-Null
Copy-Item -Path (Join-Path $Ue4ssDir "ue4ss\*") -Destination $shadowUe4ss -Recurse -Force

# 5. Update UE4SS-settings.ini for shadow execution.
$iniPath = Join-Path $shadowUe4ss "UE4SS-settings.ini"
$iniText = Get-Content -Path $iniPath -Raw -Encoding UTF8
$dumpsForward = ($DumpsDir -replace "\\", "/")
$iniText = [regex]::Replace($iniText, "(?m)^EnableHotReloadSystem\s*=.*$", "EnableHotReloadSystem = 1")
$iniText = [regex]::Replace($iniText, "(?m)^ConsoleEnabled\s*=.*$", "ConsoleEnabled = 0")
$iniText = [regex]::Replace($iniText, "(?m)^GuiConsoleEnabled\s*=.*$", "GuiConsoleEnabled = 0")
$iniText = [regex]::Replace($iniText, "(?m)^GuiConsoleVisible\s*=.*$", "GuiConsoleVisible = 0")
$iniText = [regex]::Replace($iniText, "(?m)^OutputPath\s*=.*$", "OutputPath = $dumpsForward")
Set-Content -Path $iniPath -Value $iniText -Encoding UTF8 -NoNewline

# 6. Wire custom bridge/dumper mods via junctions and update Mods/mods.txt.
$shadowModsDir = Join-Path $shadowUe4ss "Mods"
if (Test-Path $ModsSourceDir) {
    foreach ($modDir in (Get-ChildItem -Path $ModsSourceDir -Directory)) {
        $dstMod = Join-Path $shadowModsDir $modDir.Name
        Ensure-DirectoryJunction -LinkPath $dstMod -TargetPath $modDir.FullName
    }
}

$modsTxtPath = Join-Path $shadowModsDir "mods.txt"
$modsLines = [System.Collections.Generic.List[string]](Get-Content -Path $modsTxtPath -Encoding UTF8)

function Set-ModEnabledLine {
    param(
        [System.Collections.Generic.List[string]]$Lines,
        [string]$ModName,
        [bool]$Enabled
    )

    $val = if ($Enabled) { "1" } else { "0" }
    $entry = "$ModName : $val"
    $keybindIdx = -1
    $found = $false
    for ($i = 0; $i -lt $Lines.Count; $i++) {
        if ($Lines[$i] -match ("^\s*" + [regex]::Escape($ModName) + "\s*:")) {
            $Lines[$i] = $entry
            $found = $true
        }
        if ($Lines[$i] -match "^\s*Keybinds\s*:") {
            $keybindIdx = $i
        }
    }
    if (-not $found) {
        if ($keybindIdx -ge 0) {
            $Lines.Insert($keybindIdx, $entry)
        } else {
            $Lines.Add($entry)
        }
    }
}

Set-ModEnabledLine -Lines $modsLines -ModName "FoundationBridge" -Enabled (-not $DisableBridge)
if ($EnableDumper -or (Test-Path (Join-Path $ModsSourceDir "FoundationDumper"))) {
    Set-ModEnabledLine -Lines $modsLines -ModName "FoundationDumper" -Enabled ([bool]$EnableDumper)
}
Set-Content -Path $modsTxtPath -Value ($modsLines -join "`r`n") -Encoding UTF8

Write-Host "Shadow runtime ready:"
Write-Host "  ShadowExe : $(Join-Path $shadowWin64 'MakePlace-Win64-Shipping.exe')"
Write-Host "  SourceDir : $SourceDir (untouched)"
Write-Host "  BridgeIPC : $shadowIpcDir"
Write-Host "  DumpsDir  : $DumpsDir"
