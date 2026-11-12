<#
.SYNOPSIS
  Builds the Windows installer: dist\Snag-Setup-<version>.exe.

  Runs scripts\package.ps1 (release build + dist\Snag folder), then
  compiles installer\snag.iss with Inno Setup 6. Install Inno Setup once
  with:  winget install JRSoftware.InnoSetup

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\installer.ps1
#>
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    & powershell -ExecutionPolicy Bypass -File "$PSScriptRoot\package.ps1"
    if ($LASTEXITCODE -ne 0) { throw "packaging failed" }
    $version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value

    $iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe") |
        Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $iscc) { throw "Inno Setup 6 not found: winget install JRSoftware.InnoSetup" }

    # The app icon build.rs generated (same one the exe carries).
    $icon = Get-ChildItem "target\release\build" -Recurse -Filter "snag.ico" | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    $args = @("/Q", "/DAppVersion=$version", "/DSourceDir=$root\dist\Snag")
    if ($icon) { $args += "/DIconFile=$($icon.FullName)" }
    & $iscc @args "installer\snag.iss"
    if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }
    $out = Get-Item "dist\Snag-Setup-$version.exe"
    "Installer: $($out.FullName) ($([math]::Round($out.Length / 1MB, 1)) MB)"
} finally {
    Pop-Location
}
