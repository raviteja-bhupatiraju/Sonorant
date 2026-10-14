# Downloads libneedle3.dll (the Whistle speech recognition engine, Apache 2.0) into vendor/ so build.rs
# can copy it next to the exe. The engine ships inside Cactus Compute's Python wheel for Windows.
# The wheel is taken from a fixed commit of the Hugging Face repository and checked against its
# SHA-256. Skips the download when vendor\libneedle3.version already records this version (use
# -Force to fetch anyway). Another version needs -Version x.y.z -Commit <hf commit> -Sha256 <hash>.
param(
    [string]$Version = "3.2.0",
    [string]$Commit = "93e88ede867e4311c39151285e74df513e4b835b",
    [string]$Sha256 = "",
    [switch]$Force
)
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue" # the progress bar slows Invoke-WebRequest down a lot
$known = @{ "3.2.0@93e88ede867e4311c39151285e74df513e4b835b" = "0bd41dc812a4b7e504f6ba294de65a363a51b031f154d24aac748b07249dc952" }
if (-not $Sha256) { $Sha256 = $known["$Version@$Commit"] }
if (-not $Sha256) { throw "No known SHA-256 for the Needle $Version wheel at commit $Commit; pass -Sha256" }

$root = Split-Path -Parent $PSScriptRoot
$vendor = Join-Path $root "vendor"
$dll = Join-Path $vendor "libneedle3.dll"
$stampFile = Join-Path $vendor "libneedle3.version"
$stamp = "$Version $Commit $Sha256"
if (-not $Force -and (Test-Path $dll) -and (Test-Path $stampFile) -and ((Get-Content $stampFile -Raw).Trim() -eq $stamp)) {
    Write-Host "vendor\libneedle3.dll is up to date (Needle engine $Version)"
    return
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("needle_" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory $work | Out-Null
try {
    $whl = Join-Path $work "needle.zip" # a wheel is a zip archive
    Invoke-WebRequest "https://huggingface.co/Cactus-Compute/needle3/resolve/$Commit/python/cactus_needle-$Version-py3-none-win_amd64.whl" -OutFile $whl -UseBasicParsing
    $actual = (Get-FileHash $whl -Algorithm SHA256).Hash
    if ($actual -ne $Sha256) { throw "SHA-256 mismatch for cactus_needle-$Version-py3-none-win_amd64.whl: expected $Sha256, got $actual" }
    Expand-Archive $whl -DestinationPath (Join-Path $work "x")
    New-Item -ItemType Directory -Force $vendor | Out-Null
    Copy-Item (Join-Path $work "x\needle\libneedle3.dll") $dll -Force
    Set-Content $stampFile $stamp -Encoding ASCII
    Write-Host "vendor\libneedle3.dll ready (Needle engine $Version)"
} finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
