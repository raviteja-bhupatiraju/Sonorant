# Downloads libneedle3.dll (the Whistle speech recognition engine, Apache 2.0) into vendor/ so build.rs
# can copy it next to the exe. The engine ships inside Cactus Compute's Python wheel for Windows.
param([string]$Version = "3.2.0")
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$work = Join-Path ([IO.Path]::GetTempPath()) ("needle_" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory $work | Out-Null
try {
    $whl = Join-Path $work "needle.zip" # a wheel is a zip archive
    Invoke-WebRequest "https://huggingface.co/Cactus-Compute/needle3/resolve/main/python/cactus_needle-$Version-py3-none-win_amd64.whl" -OutFile $whl
    Expand-Archive $whl -DestinationPath (Join-Path $work "x")
    New-Item -ItemType Directory -Force (Join-Path $root "vendor") | Out-Null
    Copy-Item (Join-Path $work "x\needle\libneedle3.dll") (Join-Path $root "vendor\libneedle3.dll") -Force
    Write-Host "vendor\libneedle3.dll ready (Needle engine $Version)"
} finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
