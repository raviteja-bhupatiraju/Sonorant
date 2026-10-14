# Downloads onnxruntime.dll (CPU build) into vendor/ so build.rs can copy it next to the exe.
# Skips the download when vendor\onnxruntime.version already records this version (use -Force to
# fetch anyway). Another version needs its archive's SHA-256: -Version x.y.z -Sha256 <hash>.
param(
    [string]$Version = "1.30.0",
    [string]$Sha256 = "",
    [switch]$Force
)
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue" # the progress bar slows Invoke-WebRequest down a lot
$known = @{ "1.30.0" = "c6ba983baf5681af108599675d2a89c2d145512d02de28aed0bff177cd0ba949" }
if (-not $Sha256) { $Sha256 = $known[$Version] }
if (-not $Sha256) { throw "No known SHA-256 for ONNX Runtime $Version; pass -Sha256" }

$root = Split-Path -Parent $PSScriptRoot
$vendor = Join-Path $root "vendor"
$dll = Join-Path $vendor "onnxruntime.dll"
$stampFile = Join-Path $vendor "onnxruntime.version"
$stamp = "$Version $Sha256"
if (-not $Force -and (Test-Path $dll) -and (Test-Path $stampFile) -and ((Get-Content $stampFile -Raw).Trim() -eq $stamp)) {
    Write-Host "vendor\onnxruntime.dll is up to date (ONNX Runtime $Version)"
    return
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("ort_" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory $work | Out-Null
try {
    $zip = Join-Path $work "ort.zip"
    Invoke-WebRequest "https://github.com/microsoft/onnxruntime/releases/download/v$Version/onnxruntime-win-x64-$Version.zip" -OutFile $zip -UseBasicParsing
    $actual = (Get-FileHash $zip -Algorithm SHA256).Hash
    if ($actual -ne $Sha256) { throw "SHA-256 mismatch for onnxruntime-win-x64-$Version.zip: expected $Sha256, got $actual" }
    Expand-Archive $zip -DestinationPath (Join-Path $work "x")
    New-Item -ItemType Directory -Force $vendor | Out-Null
    Copy-Item (Get-ChildItem (Join-Path $work "x") -Recurse -Filter onnxruntime.dll | Select-Object -First 1).FullName $dll -Force
    Set-Content $stampFile $stamp -Encoding ASCII
    Write-Host "vendor\onnxruntime.dll ready (ONNX Runtime $Version)"
} finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
