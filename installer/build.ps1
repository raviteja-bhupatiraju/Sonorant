# Builds the release binary and compiles the installer to installer\output\.
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    if (-not (Test-Path "vendor\onnxruntime.dll")) { & "$root\scripts\fetch-onnxruntime.ps1" }
    if (-not (Test-Path "vendor\libneedle3.dll")) { & "$root\scripts\fetch-needle.ps1" }
    # Vulkan build tools for the GPU backend (once; see scripts\setup-gpu-build.ps1)
    if (-not (Test-Path "vendor\vulkan-sdk\Bin\glslc.exe")) { & "$root\scripts\setup-gpu-build.ps1" }
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

    # Stage espeak-ng's data folder next to the exe (espeak-rs looks there at runtime). The data only
    # exists inside the build tree, so without this an installed copy could not phonemize.
    $data = Get-ChildItem "target\release\build\espeak-rs-sys-*\out\share\espeak-ng-data" -Directory |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $data) { throw "espeak-ng-data not found in the release build tree" }
    $dest = Join-Path $root "target\release\espeak-ng-data"
    if (Test-Path $dest) { Remove-Item $dest -Recurse -Force }
    Copy-Item $data.FullName $dest -Recurse
    # Sonorant speaks US English only: drop every other language's pronunciation dictionary.
    Get-ChildItem $dest -Filter *_dict | Where-Object { $_.Name -ne 'en_dict' } | Remove-Item -Force
} finally { Pop-Location }

# Stage the Visual C++ runtime DLLs (app-local deployment) from the installed Build Tools.
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$crt = Get-ChildItem (Join-Path $vs "VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT") -Directory |
    Sort-Object FullName | Select-Object -Last 1
if (-not $crt) { throw "Visual C++ redistributable files not found under $vs" }
$redist = Join-Path $PSScriptRoot "redist"
if (Test-Path $redist) { Remove-Item $redist -Recurse -Force }
New-Item -ItemType Directory $redist | Out-Null
foreach ($f in "vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll", "msvcp140_1.dll") {
    Copy-Item (Join-Path $crt.FullName $f) $redist
}

$iscc = (Get-Command ISCC.exe -ErrorAction SilentlyContinue).Source
if (-not $iscc) {
    $iscc = @("$env:ProgramFiles\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
              "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $iscc) { throw "Inno Setup 6 not found. Install it from https://jrsoftware.org/isinfo.php" }
& $iscc "$PSScriptRoot\sonorant.iss"
