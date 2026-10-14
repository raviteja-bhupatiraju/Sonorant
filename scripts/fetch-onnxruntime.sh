#!/bin/sh
# Downloads libonnxruntime.so (CPU build) into vendor/ so build.rs can copy it next to the binary.
# Not needed when the distribution packages ONNX Runtime (e.g. Arch: onnxruntime-cpu), which
# Sonorant finds in /usr/lib. The archive is checked against a known SHA-256; for another version
# pass its checksum in ONNXRUNTIME_SHA256.
set -eu
version="${1:-1.30.0}"
case "$version" in
    1.30.0) sha256="a5ed5a3cac51fbb2e90da632ae43d19212faaa20e76484e62bcb7c23ddb3b3fd" ;;
    *) sha256="${ONNXRUNTIME_SHA256:?no known checksum for ONNX Runtime $version: set ONNXRUNTIME_SHA256}" ;;
esac
root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
part="$root/vendor/libonnxruntime.so.part"
trap 'rm -rf "$work" "$part"' EXIT
curl -fsSL -o "$work/onnxruntime.tgz" \
    "https://github.com/microsoft/onnxruntime/releases/download/v$version/onnxruntime-linux-x64-$version.tgz"
echo "$sha256  $work/onnxruntime.tgz" | sha256sum -c - >/dev/null
tar -xzf "$work/onnxruntime.tgz" -C "$work"
mkdir -p "$root/vendor"
# The archive holds the library plus version symlinks; copy the real file under the plain name.
cp -L "$work/onnxruntime-linux-x64-$version/lib/libonnxruntime.so" "$part"
[ -s "$part" ] || { echo "error: libonnxruntime.so is empty" >&2; exit 1; }
mv "$part" "$root/vendor/libonnxruntime.so"
echo "vendor/libonnxruntime.so ready (ONNX Runtime $version)"
