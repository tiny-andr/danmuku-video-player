# Fetch the mpv build used by the audio bypass into src-tauri/resources/mpv/.
# The binary (~116 MB) is not committed; run this once before building.
# Usage: powershell -ExecutionPolicy Bypass -File scripts/fetch-mpv.ps1
param(
    [string]$Version = "20261111",
    [string]$Tag = "bcde6b93cc"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$dest = Join-Path $root "src-tauri\resources\mpv"
$exe = Join-Path $dest "mpv.exe"

if (Test-Path $exe) {
    Write-Host "mpv.exe already present: $exe"
    exit 0
}

$sevenZip = "C:\Program Files\7-Zip\7z.exe"
if (-not (Test-Path $sevenZip)) {
    throw "7-Zip not found at $sevenZip (needed to unpack the mpv .7z)"
}

$name = "mpv-x86_64-$Version-git-$Tag.7z"
$url = "https://github.com/shinchiro/mpv-winbuild-cmake/releases/download/$Version/$name"
$tmp = Join-Path $env:TEMP $name

if (-not $env:https_proxy -and -not $env:HTTP_PROXY) {
    Write-Host "tip: behind a firewall set `$env:https_proxy=http://127.0.0.1:7897 first"
}

New-Item -ItemType Directory -Force $dest | Out-Null
Write-Host "downloading $url"
curl.exe -L --limit-rate 55M -s -o $tmp $url
if (-not (Test-Path $tmp)) { throw "download failed" }

& $sevenZip x -y -o"$dest\_unpack" $tmp | Out-Null
Copy-Item (Join-Path $dest "_unpack\mpv.exe") $exe -Force
Remove-Item (Join-Path $dest "_unpack") -Recurse -Force
Remove-Item $tmp -Force

Write-Host ("mpv.exe ready: {0} ({1:N1} MB)" -f $exe, ((Get-Item $exe).Length / 1MB))
