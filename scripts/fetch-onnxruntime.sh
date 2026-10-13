#!/bin/sh
# Downloads libonnxruntime.so (CPU build) into vendor/ so build.rs can copy it next to the binary.
# Not needed when the distribution packages ONNX Runtime (e.g. Arch: onnxruntime-cpu), which
# Sonorant finds in /usr/lib.
set -eu
version="${1:-1.30.0}"
root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
curl -fsSL "https://github.com/microsoft/onnxruntime/releases/download/v$version/onnxruntime-linux-x64-$version.tgz" \
    | tar -xz -C "$work"
mkdir -p "$root/vendor"
# The archive holds the library plus version symlinks; copy the real file under the plain name.
cp -L "$work/onnxruntime-linux-x64-$version/lib/libonnxruntime.so" "$root/vendor/libonnxruntime.so"
echo "vendor/libonnxruntime.so ready (ONNX Runtime $version)"
