# Downloads onnxruntime.dll (CPU build) into vendor/ so build.rs can copy it next to the exe.
param([string]$Version = "1.30.0")
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$work = Join-Path ([IO.Path]::GetTempPath()) ("ort_" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory $work | Out-Null
try {
    $zip = Join-Path $work "ort.zip"
    Invoke-WebRequest "https://github.com/microsoft/onnxruntime/releases/download/v$Version/onnxruntime-win-x64-$Version.zip" -OutFile $zip
    Expand-Archive $zip -DestinationPath (Join-Path $work "x")
    New-Item -ItemType Directory -Force (Join-Path $root "vendor") | Out-Null
    Copy-Item (Get-ChildItem (Join-Path $work "x") -Recurse -Filter onnxruntime.dll | Select-Object -First 1).FullName (Join-Path $root "vendor\onnxruntime.dll") -Force
    Write-Host "vendor\onnxruntime.dll ready (ONNX Runtime $Version)"
} finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
