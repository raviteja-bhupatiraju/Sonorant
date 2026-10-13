#!/bin/sh
# Downloads libneedle3.so (the Whistle speech recognition engine, Apache 2.0) into vendor/ so build.rs
# can copy it next to the binary. The engine ships inside Cactus Compute's Python wheel for Linux.
set -eu
version="${1:-3.2.0}"
root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
curl -fsSL -o "$work/needle.whl" \
    "https://huggingface.co/Cactus-Compute/needle3/resolve/main/python/cactus_needle-$version-py3-none-manylinux2014_x86_64.whl"
# A wheel is a zip archive.
mkdir -p "$root/vendor"
unzip -p "$work/needle.whl" needle/libneedle3.so > "$root/vendor/libneedle3.so"
echo "vendor/libneedle3.so ready (Needle engine $version)"
