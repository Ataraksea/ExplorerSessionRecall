<#
.SYNOPSIS
    Builds the release esr.exe and the Explorer Session Recall installer.

.DESCRIPTION
    1. cargo build --release -p esr-app --bin esr
    2. Compiles installer\ExplorerSessionRecall.iss with Inno Setup 6 (ISCC.exe)
    3. Writes to dist\:
         ExplorerSessionRecall-Setup-<version>.exe   the installer
         esr-<version>-x64.exe                       the bare program
         SHA256SUMS.txt                              checksums of both

    Used locally and by .github/workflows/release.yml.

.PARAMETER Version
    Version to stamp on the installer. Defaults to [workspace.package]
    version in the root Cargo.toml.
#>
param([string]$Version)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

if (-not $Version) {
    $inWorkspacePackage = $false
    foreach ($line in Get-Content (Join-Path $root 'Cargo.toml')) {
        if ($line -match '^\s*\[(.+)\]\s*$') {
            $inWorkspacePackage = $Matches[1] -eq 'workspace.package'
        } elseif ($inWorkspacePackage -and $line -match '^\s*version\s*=\s*"([^"]+)"') {
            $Version = $Matches[1]
            break
        }
    }
    if (-not $Version) { throw 'Could not read [workspace.package] version from Cargo.toml.' }
}
Write-Host "Building Explorer Session Recall $Version"

# Release build.
$env:CARGO_INCREMENTAL = '0'
Push-Location $root
try {
    cargo build --release -p esr-app --bin esr
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE." }
} finally {
    Pop-Location
}
$exe = Join-Path $root 'target\release\esr.exe'
if (-not (Test-Path $exe)) { throw "$exe was not built." }

# Inno Setup compiler.
$iscc = $null
$cmd = Get-Command ISCC.exe -ErrorAction SilentlyContinue
if ($cmd) { $iscc = $cmd.Source }
if (-not $iscc) {
    $candidates = @(
        (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
        (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe'),
        (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe')
    )
    $iscc = $candidates | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
}
if (-not $iscc) { throw 'ISCC.exe (Inno Setup 6) not found. Install it from https://jrsoftware.org/isinfo.php.' }

# Installer.
$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$iss = Join-Path $root 'installer\ExplorerSessionRecall.iss'
& $iscc "/DAppVersion=$Version" "/DSourceExe=$exe" "/O$dist" $iss
if ($LASTEXITCODE -ne 0) { throw "ISCC failed with exit code $LASTEXITCODE." }
$setup = Join-Path $dist "ExplorerSessionRecall-Setup-$Version.exe"
if (-not (Test-Path $setup)) { throw "$setup was not created." }

# Bare program.
$bare = Join-Path $dist "esr-$Version-x64.exe"
Copy-Item -Force $exe $bare

# Checksums: "<sha256>  <name>", LF line endings, no BOM.
$lines = foreach ($file in @($setup, $bare)) {
    $hash = (Get-FileHash -Algorithm SHA256 $file).Hash.ToLowerInvariant()
    "$hash  $(Split-Path -Leaf $file)"
}
$sums = Join-Path $dist 'SHA256SUMS.txt'
[System.IO.File]::WriteAllText($sums, (($lines -join "`n") + "`n"), (New-Object System.Text.UTF8Encoding($false)))

Write-Host ''
Write-Host 'Output:'
foreach ($file in @($setup, $bare, $sums)) { Write-Host "  $file" }
