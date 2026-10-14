# Builds the installers into installer\output\: the Windows installer (Inno Setup), then the Linux
# packages (.AppImage, .deb, .rpm) through WSL.
#
#   installer\build.ps1                  Windows installer + Linux packages
#   installer\build.ps1 -SkipLinux       Windows installer only
#   installer\build.ps1 -SkipWindows     Linux packages only
#   installer\build.ps1 -Distro Fedora   use another WSL distribution (needs podman or docker)
#
# The Linux build runs installer/build-linux.sh in a Debian 12 container inside the WSL distribution
# (see that script); the first run downloads the toolchain and can take a long while.
param(
    [switch]$SkipWindows,
    [switch]$SkipLinux,
    [string]$Distro = "Arch"
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

if (-not $SkipWindows) {
Push-Location $root
try {
    # The fetch scripts skip the download when vendor\ already holds the pinned version (they
    # compare a version stamp), so a version bump in a script is picked up here.
    & "$root\scripts\fetch-onnxruntime.ps1"
    & "$root\scripts\fetch-needle.ps1"
    # Vulkan build tools for the GPU backend (rebuilt only when missing or out of date; see
    # scripts\setup-gpu-build.ps1)
    & "$root\scripts\setup-gpu-build.ps1"
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit code $LASTEXITCODE)" }

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

# The installer's version is the package version in Cargo.toml.
$cargoToml = Get-Content (Join-Path $root "Cargo.toml") -Raw
$package = [regex]::Match($cargoToml, '(?ms)^\[package\]\s*$(.*?)(?=^\[)')
$version = [regex]::Match($package.Groups[1].Value, '(?m)^\s*version\s*=\s*"([^"]+)"').Groups[1].Value
if (-not $version) { throw "could not read the package version from Cargo.toml" }

# Stage the Visual C++ runtime DLLs (app-local deployment) from the installed Build Tools.
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) { throw "vswhere.exe not found: install Visual Studio Build Tools with the C++ workload" }
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if ($LASTEXITCODE -ne 0) { throw "vswhere failed (exit code $LASTEXITCODE)" }
if (-not $vs) { throw "Visual Studio C++ build tools not found (needed for the Visual C++ runtime DLLs)" }
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
& $iscc "/DAppVersion=$version" "$PSScriptRoot\sonorant.iss"
if ($LASTEXITCODE -ne 0) { throw "Inno Setup failed (exit code $LASTEXITCODE)" }

}

if (-not $SkipLinux) {
    if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) { throw "wsl.exe not found: install WSL, or pass -SkipLinux" }
    # wsl.exe prints UTF-16, so list the distributions with the output encoding switched.
    $enc = [Console]::OutputEncoding
    [Console]::OutputEncoding = [Text.Encoding]::Unicode
    try { $distros = @(wsl.exe -l -q) | ForEach-Object { $_.Trim() } } finally { [Console]::OutputEncoding = $enc }
    if ($distros -notcontains $Distro) { throw "WSL distribution '$Distro' not found (have: $($distros -join ', ')). Use -Distro or -SkipLinux" }
    Write-Host "Building the Linux packages in WSL ($Distro)..."
    wsl.exe -d $Distro --cd $root --exec sh installer/build-linux.sh
    if ($LASTEXITCODE -ne 0) { throw "Linux build failed (exit code $LASTEXITCODE)" }
}