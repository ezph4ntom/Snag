<#
.SYNOPSIS
  Copies ffmpeg.exe and ffprobe.exe into .\bin so ClipApp uses its own
  bundled copy (the "sidecar") instead of whatever is on PATH.

.DESCRIPTION
  ClipApp looks for ffmpeg in this order:
    1. %CLIPAPP_FFMPEG_DIR%
    2. bin\ next to the themes\ and assets\ folders (this script fills it)
    3. bin\ next to clipapp.exe, then next to clipapp.exe itself
    4. every folder on PATH (including PATH entries added after login)

  You need an ffmpeg *full* build that includes h264_nvenc, h264_amf,
  h264_qsv and libx264, for example:
    winget install Gyan.FFmpeg           (gyan.dev "full" build)
  or download a "full" / "gpl" build from https://www.gyan.dev/ffmpeg/builds/
  or https://github.com/BtbN/FFmpeg-Builds/releases and pass -From <folder>.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\bundle-ffmpeg.ps1
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\bundle-ffmpeg.ps1 -From C:\tools\ffmpeg\bin
#>
param([string]$From = "")

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $root "bin"

function Find-Tool($name) {
    if ($From) {
        $p = Join-Path $From "$name.exe"
        if (Test-Path $p) { return (Resolve-Path $p).Path }
        throw "$name.exe not found in $From"
    }
    # Refresh PATH from the registry so a just-installed winget package is found.
    $env:Path = [Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [Environment]::GetEnvironmentVariable("Path", "User")
    $cmd = Get-Command "$name.exe" -ErrorAction SilentlyContinue
    if (-not $cmd) { throw "$name.exe not found on PATH. Install it with: winget install Gyan.FFmpeg (then re-run), or pass -From <folder>." }
    $item = Get-Item $cmd.Source
    # winget installs a symlink in ...\WinGet\Links; copy the real file.
    if ($item.LinkType -and $item.Target) { return ($item.Target | Select-Object -First 1) }
    return $item.FullName
}

New-Item -ItemType Directory -Force $dest | Out-Null
foreach ($tool in "ffmpeg", "ffprobe") {
    $src = Find-Tool $tool
    Copy-Item $src (Join-Path $dest "$tool.exe") -Force
    Write-Host "copied $src -> bin\$tool.exe"
}

# Sanity check: the encoders ClipApp uses must be compiled in.
$encoders = & (Join-Path $dest "ffmpeg.exe") -hide_banner -encoders 2>$null | Out-String
foreach ($e in "h264_nvenc", "h264_amf", "h264_qsv", "libx264") {
    if ($encoders -match "\b$e\b") { Write-Host "  [ok] $e" } else { Write-Warning "  this ffmpeg build lacks $e" }
}
Write-Host "Done. ClipApp will now use $dest"
