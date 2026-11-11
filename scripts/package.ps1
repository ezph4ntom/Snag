<#
.SYNOPSIS
  Builds a release and assembles a portable folder + zip in .\dist:

    dist\Snag\
      snag.exe
      assets\sounds\clip.wav
      themes\*.json, themes\README.md
      README.md, TESTING.md
      bin\ffmpeg.exe, bin\ffprobe.exe   (only with -WithFfmpeg)

  By default ffmpeg is NOT included: the zip stays small (~5 MB) and users
  install ffmpeg once with `winget install Gyan.FFmpeg` (Snag tells them
  if it's missing). ffmpeg "full" builds are GPL-licensed; if you bundle
  them you must also pass on ffmpeg's license and a link to its source.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\package.ps1
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\package.ps1 -WithFfmpeg
#>
param([switch]$WithFfmpeg)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    $version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value

    $out = Join-Path $root "dist\Snag"
    if (Test-Path $out) { Remove-Item -Recurse -Force $out }
    New-Item -ItemType Directory -Force $out | Out-Null
    Copy-Item "target\release\snag.exe" $out
    Copy-Item -Recurse "assets" $out
    Copy-Item -Recurse "themes" $out
    Copy-Item "README.md", "TESTING.md" $out
    if ($WithFfmpeg) {
        if (-not (Test-Path "bin\ffmpeg.exe")) { throw "bin\ffmpeg.exe missing: run scripts\bundle-ffmpeg.ps1 first" }
        Copy-Item -Recurse "bin" $out
        Write-Warning "ffmpeg included: ship ffmpeg's GPL license and a link to its source with this zip."
    }
    $zip = Join-Path $root "dist\Snag-v$version.zip"
    if (Test-Path $zip) { Remove-Item $zip }
    Compress-Archive -Path $out -DestinationPath $zip
    Write-Host "Portable build: $out"
    Write-Host "Zip:            $zip ($([math]::Round((Get-Item $zip).Length / 1MB, 1)) MB)"
} finally {
    Pop-Location
}
