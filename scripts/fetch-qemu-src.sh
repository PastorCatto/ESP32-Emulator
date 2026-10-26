#!/usr/bin/env bash
# Fetch the Espressif QEMU fork's source into vendor/src, at the same release
# scripts/fetch-qemu.sh downloads binaries for. qemu/build.sh applies our
# device models to it and builds.
#
# Usage: scripts/fetch-qemu-src.sh

set -euo pipefail

TAG="esp-develop-9.2.2-20260417"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="${REPO_ROOT}/vendor/src"

if [ -f "${SRC}/hw/xtensa/esp32s3.c" ]; then
  echo "QEMU source already present at ${SRC}"
  exit 0
fi

mkdir -p "${REPO_ROOT}/vendor"
staging="${REPO_ROOT}/vendor/.staging-src"
rm -rf "$staging"

# Shallow: the history is hundreds of megabytes and nothing here needs it.
git clone --depth 1 --branch "$TAG" https://github.com/espressif/qemu "$staging"

rm -rf "$SRC"
mv "$staging" "$SRC"
echo "Installed QEMU source ($TAG) into ${SRC}"
