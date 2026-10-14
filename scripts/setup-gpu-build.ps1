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
#
# Every download is pinned (tag, commit or exact package file) and checked against its SHA-256.
# To update one, change the pin and its hash below; the stamp file then no longer matches and the
# next run (or installer\build.ps1) rebuilds the folder.
param(
    [string]$Target = (Join-Path (Split-Path -Parent $PSScriptRoot) "vendor\vulkan-sdk"),
    [switch]$Force
)
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue" # the progress bar slows Invoke-WebRequest down a lot

$VulkanHeadersTag = "v1.4.309"
$VulkanHeadersSha256 = "51cd37049511e0ba24f6ce3c70a4e6557620a33c83945cce9b28560206dfed86"
$SpirvHeadersCommit = "86f980c731e62ae4eaf383d320449d71687936bf"
$SpirvHeadersSha256 = "5abe65f980fad5c3365a1060acf3f16c552dea41db40aae54a27dfbebb840995"
# conda-forge packages providing glslc and the SPIR-V tools it needs.
$CondaPackages = @(
    @{ File = "win-64/shaderc-2026.4-hef10606_0.conda"; Sha256 = "6a50d25b25e65adfc01f5333edced1c98991ef73dbf1efb7f8c2076482ed247f" },
    @{ File = "win-64/spirv-tools-2026.4-h49e36cd_0.conda"; Sha256 = "1dc41c5196116e25c2da19288839462f4b6ded5013d39a5798631096632a7734" }
)

$stampFile = Join-Path $Target "sonorant-vulkan.version"
$stamp = (@("$VulkanHeadersTag $VulkanHeadersSha256", "$SpirvHeadersCommit $SpirvHeadersSha256") +
          ($CondaPackages | ForEach-Object { "$($_.File) $($_.Sha256)" })) -join ";"
$upToDate = (Test-Path $stampFile) -and ((Get-Content $stampFile -Raw).Trim() -eq $stamp)
if ((Test-Path (Join-Path $Target "Bin\glslc.exe")) -and (Test-Path (Join-Path $Target "Lib\vulkan-1.lib")) -and $upToDate -and -not $Force) {
    Write-Host "Vulkan build tools already present in $Target (use -Force to rebuild)"
    return
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("sonorant_vk_" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory $work | Out-Null
foreach ($d in "Include", "Lib", "Bin", "prefix") {
    # Start clean so files from an older pinned version cannot linger.
    Remove-Item (Join-Path $Target $d) -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force (Join-Path $Target $d) | Out-Null
}
Remove-Item $stampFile -Force -ErrorAction SilentlyContinue

function Download($url, $dest, $sha256) {
    Invoke-WebRequest $url -OutFile $dest -UseBasicParsing
    $actual = (Get-FileHash $dest -Algorithm SHA256).Hash
    if ($actual -ne $sha256) { throw "SHA-256 mismatch for ${url}: expected $sha256, got $actual" }
}

try {
    # 1. Vulkan headers
    Write-Host "Vulkan headers $VulkanHeadersTag ..."
    $zip = Join-Path $work "vk-headers.zip"
    Download "https://github.com/KhronosGroup/Vulkan-Headers/archive/refs/tags/$VulkanHeadersTag.zip" $zip $VulkanHeadersSha256
    Expand-Archive $zip -DestinationPath (Join-Path $work "vkh")
    $inc = Get-ChildItem (Join-Path $work "vkh") -Directory | Select-Object -First 1
    Copy-Item (Join-Path $inc.FullName "include\*") (Join-Path $Target "Include") -Recurse -Force

    # 2. SPIR-V headers (needs a CMake package config as well as the headers)
    Write-Host "SPIR-V headers $SpirvHeadersCommit ..."
    $zip = Join-Path $work "spirv.zip"
    Download "https://github.com/KhronosGroup/SPIRV-Headers/archive/$SpirvHeadersCommit.zip" $zip $SpirvHeadersSha256
    Expand-Archive $zip -DestinationPath (Join-Path $work "spirv")
    $src = (Get-ChildItem (Join-Path $work "spirv") -Directory | Select-Object -First 1).FullName
    $prefix = Join-Path $Target "prefix"
    cmake -S $src -B (Join-Path $work "spirv-build") "-DCMAKE_INSTALL_PREFIX=$prefix" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "cmake configure of SPIRV-Headers failed (exit code $LASTEXITCODE)" }
    cmake --install (Join-Path $work "spirv-build") | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "cmake --install of SPIRV-Headers failed (exit code $LASTEXITCODE)" }
    Copy-Item (Join-Path $prefix "include\spirv") (Join-Path $Target "Include") -Recurse -Force

    # 3. Import library for the Vulkan loader, generated from the installed vulkan-1.dll
    Write-Host "vulkan-1.lib ..."
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { throw "vswhere.exe not found: install Visual Studio Build Tools with the C++ workload" }
    $vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ($LASTEXITCODE -ne 0) { throw "vswhere failed (exit code $LASTEXITCODE)" }
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
    if ($LASTEXITCODE -ne 0) { throw "generating vulkan-1.lib failed (exit code $LASTEXITCODE)" }
    Remove-Item (Join-Path $lib "vulkan-1.exp") -ErrorAction SilentlyContinue
    if (-not (Test-Path (Join-Path $lib "vulkan-1.lib"))) { throw "could not generate vulkan-1.lib" }

    # 4. glslc (shader compiler): a recent build is required (the Android NDK one is too old)
    foreach ($p in $CondaPackages) {
        $pkg = [IO.Path]::GetFileNameWithoutExtension($p.File)
        Write-Host "$pkg (conda-forge) ..."
        $pkgZip = Join-Path $work "$pkg.conda.zip"
        Download "https://conda.anaconda.org/conda-forge/$($p.File)" $pkgZip $p.Sha256
        $x = Join-Path $work "x_$pkg"
        Expand-Archive $pkgZip -DestinationPath $x
        $tarball = Get-ChildItem $x -Filter "pkg-*.tar.zst" | Select-Object -First 1
        $out = Join-Path $x "out"
        New-Item -ItemType Directory $out | Out-Null
        & "$env:SystemRoot\System32\tar.exe" -xf $tarball.FullName -C $out
        if ($LASTEXITCODE -ne 0) { throw "extracting $($tarball.Name) failed (exit code $LASTEXITCODE)" }
        Get-ChildItem (Join-Path $out "Library\bin") -File | Where-Object { $_.Extension -in ".exe", ".dll" } |
            Copy-Item -Destination (Join-Path $Target "Bin") -Force
    }
    $glslcVersion = & (Join-Path $Target "Bin\glslc.exe") --version
    if ($LASTEXITCODE -ne 0) { throw "glslc does not run (exit code $LASTEXITCODE)" }
    $glslcVersion | Select-Object -First 1
    Set-Content $stampFile $stamp -Encoding ASCII
    Write-Host "Done: $Target"
} finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
