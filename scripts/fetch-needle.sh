#!/bin/sh
# Downloads libneedle3.so (the Whistle speech recognition engine, Apache 2.0) into vendor/ so build.rs
# can copy it next to the binary. The engine ships inside Cactus Compute's Python wheel for Linux.
# The download is pinned to a repository commit and checked against a known SHA-256; for another
# version pass its checksum in NEEDLE_SHA256.
set -eu
version="${1:-3.2.0}"
commit="93e88ede867e4311c39151285e74df513e4b835b"
case "$version" in
    3.2.0) sha256="0e8a3bce4e52968ee14e8e9d47098e65ed06dd5b7c18cef3a7fa1375756736f9" ;;
    *) sha256="${NEEDLE_SHA256:?no known checksum for Needle $version: set NEEDLE_SHA256}" ;;
esac
root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
part="$root/vendor/libneedle3.so.part"
trap 'rm -rf "$work" "$part"' EXIT
curl -fsSL -o "$work/needle.whl" \
    "https://huggingface.co/Cactus-Compute/needle3/resolve/$commit/python/cactus_needle-$version-py3-none-manylinux2014_x86_64.whl"
echo "$sha256  $work/needle.whl" | sha256sum -c - >/dev/null
# A wheel is a zip archive. Extract next to the target first, so a failure leaves no empty file.
mkdir -p "$root/vendor"
unzip -p "$work/needle.whl" needle/libneedle3.so > "$part"
[ -s "$part" ] || { echo "error: libneedle3.so missing from the wheel" >&2; exit 1; }
mv "$part" "$root/vendor/libneedle3.so"
echo "vendor/libneedle3.so ready (Needle engine $version)"
