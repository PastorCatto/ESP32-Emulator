#!/usr/bin/env bash
# Fetch Espressif's QEMU fork into vendor/.
#
# The emulator prefers this copy over anything on PATH, so the version we test
# against wins over whatever a developer happens to have installed.
#
# Usage: scripts/fetch-qemu.sh [xtensa|riscv32|both]

set -euo pipefail

RELEASE="esp-develop-9.2.2-20260417"
VERSION="esp_develop_9.2.2_20260417"
BASE="https://github.com/espressif/qemu/releases/download/${RELEASE}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR="${REPO_ROOT}/vendor"

# Xtensa covers ESP32 and ESP32-S3; riscv32 covers ESP32-C3.
ARCHES="${1:-xtensa}"
[ "$ARCHES" = "both" ] && ARCHES="xtensa riscv32"

detect_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Linux)  case "$arch" in
              x86_64)         echo "x86_64-linux-gnu" ;;
              aarch64|arm64)  echo "aarch64-linux-gnu" ;;
              *) echo "unsupported Linux arch: $arch" >&2; exit 1 ;;
            esac ;;
    Darwin) case "$arch" in
              x86_64)         echo "x86_64-apple-darwin" ;;
              arm64)          echo "aarch64-apple-darwin" ;;
              *) echo "unsupported macOS arch: $arch" >&2; exit 1 ;;
            esac ;;
    MINGW*|MSYS*|CYGWIN*) echo "x86_64-w64-mingw32" ;;
    *) echo "unsupported OS: $os" >&2; exit 1 ;;
  esac
}

TARGET="$(detect_target)"
echo "Host target: ${TARGET}"
mkdir -p "$VENDOR"

for arch in $ARCHES; do
  name="qemu-${arch}-softmmu-${VERSION}-${TARGET}.tar.xz"
  archive="${VENDOR}/${name}"

  echo "Downloading ${name}"
  curl -fSL --progress-bar -o "$archive" "${BASE}/${name}"

  # The archive contains a top-level qemu/ directory. Unpack to a staging area
  # so a failed extraction cannot leave a half-replaced vendor/qemu behind.
  staging="${VENDOR}/.staging-${arch}"
  rm -rf "$staging"
  mkdir -p "$staging"
  tar -xf "$archive" -C "$staging"

  if [ ! -d "${staging}/qemu" ]; then
    echo "unexpected archive layout: no qemu/ directory inside ${name}" >&2
    exit 1
  fi

  # Merge rather than replace, so fetching riscv32 does not delete xtensa.
  mkdir -p "${VENDOR}/qemu"
  cp -R "${staging}/qemu/." "${VENDOR}/qemu/"
  rm -rf "$staging" "$archive"
done

echo
echo "Installed into ${VENDOR}/qemu"
ls "${VENDOR}/qemu/bin" | sed 's/^/  /'
echo
echo "Verify with: ESP32_EMULATOR_REQUIRE_QEMU=1 cargo test -p qemuctl"
