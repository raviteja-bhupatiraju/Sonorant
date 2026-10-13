# Prepares everything needed to compile the GPU (Vulkan) backend of the speech engine, without
# installing a Vulkan SDK system-wide. Run once; the result lives in vendor\vulkan-sdk (git-ignored)
# and is picked up through .cargo\config.toml.
#
#   Include\   Vulkan headers (Khronos) + SPIR-V headers
#   Lib\       vulkan-1.lib, generated from the vulkan-1.dll that ships with your GPU driver
#   Bin\       glslc (shader compiler) and its DLLs, from conda-forge
#   prefix\    SPIRV-Headers CMake package
#
# Requires: Visual Studio Build Tools (MSVC), CMake, Windows 10+ (tar.exe), internet access.
param(
    [string]$Target = (Join-Path (Split-Path -Parent $PSScriptRoot) "vendor\vulkan-sdk"),
    [string]$VulkanHeadersTag = "v1.4.309",
    [switch]$Force
)
$ErrorActionPreference = "Stop"

if ((Test-Path (Join-Path $Target "Bin\glslc.exe")) -and (Test-Path (Join-Path $Target "Lib\vulkan-1.lib")) -and -not $Force) {
    Write-Host "Vulkan build tools already present in $Target (use -Force to rebuild)"
    return
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("sonorant_vk_" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory $work | Out-Null
foreach ($d in "Include", "Lib", "Bin", "prefix") { New-Item -ItemType Directory -Force (Join-Path $Target $d) | Out-Null }

function Download($url, $dest) { Invoke-WebRequest $url -OutFile $dest -UseBasicParsing }

try {
    # 1. Vulkan headers
    Write-Host "Vulkan headers $VulkanHeadersTag ..."
    $zip = Join-Path $work "vk-headers.zip"
    Download "https://github.com/KhronosGroup/Vulkan-Headers/archive/refs/tags/$VulkanHeadersTag.zip" $zip
    Expand-Archive $zip -DestinationPath (Join-Path $work "vkh")
    $inc = Get-ChildItem (Join-Path $work "vkh") -Directory | Select-Object -First 1
    Copy-Item (Join-Path $inc.FullName "include\*") (Join-Path $Target "Include") -Recurse -Force

    # 2. SPIR-V headers (needs a CMake package config as well as the headers)
    Write-Host "SPIR-V headers ..."
    $zip = Join-Path $work "spirv.zip"
    Download "https://github.com/KhronosGroup/SPIRV-Headers/archive/refs/heads/main.zip" $zip
    Expand-Archive $zip -DestinationPath (Join-Path $work "spirv")
    $src = (Get-ChildItem (Join-Path $work "spirv") -Directory | Select-Object -First 1).FullName
    $prefix = Join-Path $Target "prefix"
    cmake -S $src -B (Join-Path $work "spirv-build") "-DCMAKE_INSTALL_PREFIX=$prefix" | Out-Null
    cmake --install (Join-Path $work "spirv-build") | Out-Null
    Copy-Item (Join-Path $prefix "include\spirv") (Join-Path $Target "Include") -Recurse -Force

    # 3. Import library for the Vulkan loader, generated from the installed vulkan-1.dll
    Write-Host "vulkan-1.lib ..."
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    $vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (-not $vs) { throw "Visual Studio C++ build tools not found" }
    $vcvars = Join-Path $vs "VC\Auxiliary\Build\vcvars64.bat"
    $dll = Join-Path $env:SystemRoot "System32\vulkan-1.dll"
    if (-not (Test-Path $dll)) { throw "vulkan-1.dll not found: install a GPU driver with Vulkan support" }
    $bat = Join-Path $work "mklib.bat"
    $lib = Join-Path $Target "Lib"
    @"
@echo off
call "$vcvars" >nul
cd /d "$lib"
echo LIBRARY vulkan-1 > vulkan-1.def
echo EXPORTS >> vulkan-1.def
for /f "skip=19 tokens=4" %%A in ('dumpbin /exports "$dll" ^| findstr /R "^ *[0-9][0-9]* *[0-9A-F][0-9A-F]* *[0-9A-F]"') do echo %%A >> vulkan-1.def
lib /nologo /def:vulkan-1.def /out:vulkan-1.lib /machine:x64
"@ | Set-Content $bat -Encoding ASCII
    cmd /c $bat | Out-Null
    Remove-Item (Join-Path $lib "vulkan-1.exp") -ErrorAction SilentlyContinue
    if (-not (Test-Path (Join-Path $lib "vulkan-1.lib"))) { throw "could not generate vulkan-1.lib" }

    # 4. glslc (shader compiler): a recent build is required (the Android NDK one is too old)
    foreach ($pkg in "shaderc", "spirv-tools") {
        Write-Host "$pkg (conda-forge) ..."
        $files = Invoke-RestMethod "https://api.anaconda.org/package/conda-forge/$pkg/files"
        $latest = $files | Where-Object { $_.attrs.subdir -eq "win-64" -and $_.basename -like "*.conda" } |
            Sort-Object upload_time | Select-Object -Last 1
        $pkgZip = Join-Path $work "$pkg.conda.zip"
        Download "https://conda.anaconda.org/conda-forge/$($latest.basename)" $pkgZip
        $x = Join-Path $work "x_$pkg"
        Expand-Archive $pkgZip -DestinationPath $x
        $tarball = Get-ChildItem $x -Filter "pkg-*.tar.zst" | Select-Object -First 1
        $out = Join-Path $x "out"
        New-Item -ItemType Directory $out | Out-Null
        & "$env:SystemRoot\System32\tar.exe" -xf $tarball.FullName -C $out
        Get-ChildItem (Join-Path $out "Library\bin") -File | Where-Object { $_.Extension -in ".exe", ".dll" } |
            Copy-Item -Destination (Join-Path $Target "Bin") -Force
    }
    & (Join-Path $Target "Bin\glslc.exe") --version | Select-Object -First 1
    Write-Host "Done: $Target"
} finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
