#!/bin/sh
# Builds Sonorant for Linux and packages it as an AppImage, a .deb and an .rpm in installer/output/.
#
#   installer/build-linux.sh            build in a Debian 12 container (podman or docker)
#   installer/build-linux.sh --native   build on this machine
#
# Binaries only run on systems with the glibc they were built against or newer. The container
# build needs glibc 2.36 (Debian 12+, Ubuntu 23.04+, Fedora 37+); a native build on a rolling
# distribution would not start on most others. Tools (Rust, nfpm, appimagetool) and the build
# cache are kept in ~/.cache/sonorant-build.
set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
image="docker.io/library/debian:12"
nfpm_version="2.47.0"
vulkan_sdk_version="1.4.363.0" # container builds only
cache="${XDG_CACHE_HOME:-$HOME/.cache}/sonorant-build"

# ---- run this script again inside the container ------------------------------------------------
if [ "${1:-}" != "--native" ]; then
    engine="$(command -v podman || command -v docker || true)"
    [ -n "$engine" ] || { echo "error: podman or docker is needed (or pass --native)" >&2; exit 1; }
    mkdir -p "$cache"
    echo "Building in $image with $(basename "$engine")..."
    # Rootless podman already maps the container's root to this user; docker needs the files handed back.
    ids=""
    [ "$(basename "$engine")" = docker ] && ids="$(id -u):$(id -g)"
    exec "$engine" run --rm \
        -e SONORANT_CACHE=/cache -e HOST_IDS="$ids" \
        -v "$root:/src" -v "$cache:/cache" -w /src \
        "$image" sh installer/build-linux.sh --native --in-container
fi

if [ "${2:-}" = "--in-container" ]; then
    cache="$SONORANT_CACHE"
    echo "Installing build dependencies..."
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
        ca-certificates curl git file unzip xz-utils build-essential cmake clang libclang-dev pkg-config libasound2-dev \
       libvulkan-dev >/dev/null
    export CARGO_HOME="$cache/cargo" RUSTUP_HOME="$cache/rustup"
    if [ ! -x "$CARGO_HOME/bin/cargo" ]; then
        curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable >/dev/null
    fi
    export PATH="$CARGO_HOME/bin:$PATH"
    rustup component add rustfmt >/dev/null 2>&1 # espeak's bindings generator formats with it
    export CARGO_TARGET_DIR="$cache/target"

    # Debian 12's glslc is too old for transcribe.cpp's Vulkan shaders: take glslc and the Vulkan and
    # SPIR-V headers from LunarG's Vulkan SDK (built for older glibc; cached). The loader is Debian's.
    sdk="$cache/tools/vulkan-sdk-$vulkan_sdk_version"
    if [ ! -x "$sdk/x86_64/bin/glslc" ]; then
        archive="$cache/tools/vulkan-sdk-$vulkan_sdk_version.tar.xz"
        mkdir -p "$sdk"
        [ -s "$archive" ] || curl -fsSL -o "$archive" \
            "https://sdk.lunarg.com/sdk/download/$vulkan_sdk_version/linux/vulkansdk-linux-x86_64-$vulkan_sdk_version.tar.xz"
        tar -xJf "$archive" -C "$sdk" --strip-components=1 --wildcards \
           "$vulkan_sdk_version/x86_64/bin/glslc" "$vulkan_sdk_version/x86_64/include/*" \
           "$vulkan_sdk_version/x86_64/share/cmake/SPIRV-Headers/*"
    fi
    vulkan="$sdk/x86_64"
    export PATH="$vulkan/bin:$PATH"
fi

# ---- build ------------------------------------------------------------------------------------
cd "$root"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"
out="$root/installer/output"
tools="$cache/tools"
mkdir -p "$out" "$tools"

[ -f vendor/libonnxruntime.so ] || sh scripts/fetch-onnxruntime.sh
[ -f vendor/libneedle3.so ] || sh scripts/fetch-needle.sh
# .cargo/config.toml points at the Windows Vulkan SDK folder; use the system's headers and glslc
# (or, in the container, the LunarG SDK's).
export VULKAN_SDK="${vulkan:-/usr}" CMAKE_PREFIX_PATH="${vulkan:-/usr}"
cargo build --release
bin="${CARGO_TARGET_DIR:-$root/target}/release"

# ---- stage the files (the AppImage's AppDir doubles as the package root) ----------------------
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
app="$work/AppDir"
lib="$app/usr/lib/sonorant"
mkdir -p "$lib" "$app/usr/share/applications" "$app/usr/share/icons/hicolor/256x256/apps" "$app/usr/share/licenses/sonorant"
cp "$bin/sonorant" "$bin/sonorant.png" "$lib/"
# install -m: files from a Windows drive (WSL) would otherwise keep an executable bit.
install -m 644 LICENSE "$app/usr/share/licenses/sonorant/"
cp -P "$bin"/lib*.so* "$lib/" # transcribe.cpp, its GPU / CPU modules, ONNX Runtime and the Whistle engine; keeps version symlinks
# espeak-ng's pronunciation data lives in the build tree; Sonorant speaks US English only.
data="$(ls -dt "$bin"/build/espeak-rs-sys-*/out/share/espeak-ng-data | head -n 1)"
cp -r "$data" "$lib/espeak-ng-data"
find "$lib/espeak-ng-data" -maxdepth 1 -name '*_dict' ! -name en_dict -delete
install -m 644 installer/linux/org.sonorant.Sonorant.desktop "$app/usr/share/applications/"
cp "$bin/sonorant.png" "$app/usr/share/icons/hicolor/256x256/apps/org.sonorant.Sonorant.png"

# ---- .deb and .rpm ----------------------------------------------------------------------------
nfpm="$tools/nfpm-$nfpm_version"
if [ ! -x "$nfpm" ]; then
    curl -fsSL "https://github.com/goreleaser/nfpm/releases/download/v$nfpm_version/nfpm_${nfpm_version}_Linux_x86_64.tar.gz" \
        | tar -xz -C "$tools" nfpm
    mv "$tools/nfpm" "$nfpm"
fi
# nfpm expands variables only in some fields, so fill in VERSION and STAGE here.
sed -e "s|\${VERSION}|$version|g" -e "s|\${STAGE}|$app|g" installer/linux/nfpm.yaml > "$work/nfpm.yaml"
for packager in deb rpm; do
    "$nfpm" package --config "$work/nfpm.yaml" --packager "$packager" --target "$out/"
done

# ---- AppImage ---------------------------------------------------------------------------------
appimagetool="$tools/appimagetool"
if [ ! -x "$appimagetool" ]; then
    curl -fsSL -o "$appimagetool" \
        https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x "$appimagetool"
fi
install -m 755 installer/linux/AppRun "$app/AppRun"
install -m 644 installer/linux/org.sonorant.Sonorant.desktop "$app/"
cp "$bin/sonorant.png" "$app/org.sonorant.Sonorant.png"
ln -s org.sonorant.Sonorant.png "$app/.DirIcon"
# Extract-and-run: no FUSE needed (containers, WSL).
ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 "$appimagetool" --no-appstream "$app" "$out/Sonorant-$version-x86_64.AppImage"

# Files written from a docker container belong to root; hand them back.
if [ -n "${HOST_IDS:-}" ]; then
    chown -R "$HOST_IDS" "$out" vendor
fi
echo "Packages in installer/output:"
ls -la "$out" | grep -E "\.(AppImage|deb|rpm)$"
