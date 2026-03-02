#!/usr/bin/env bash
# cross-compile.sh — Build NaviPod for Raspberry Pi Zero W (armv6l)
#
# Prerequisites on your Linux dev machine:
#   sudo apt install gcc-arm-linux-gnueabihf
#   rustup target add arm-unknown-linux-gnueabihf
#
# The Pi Zero W uses ARMv6 with hardware float (armv6l).
# The closest Rust target is arm-unknown-linux-gnueabihf.

set -e

TARGET="arm-unknown-linux-gnueabihf"
BINARY="target/${TARGET}/release/navipod"

echo "→ Building for ${TARGET}..."

CARGO_TARGET_ARM_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc \
    cargo build --target "${TARGET}" --release

echo "✓ Build complete: ${BINARY}"
echo ""

read -rp "Deploy to Raspberry Pi? [y/N] " deploy
if [[ "${deploy}" =~ ^[Yy]$ ]]; then
    read -rp "Pi username: " pi_user
    read -rp "Pi IP address: " pi_ip
    echo "→ Copying binary..."
    scp "${BINARY}" "${pi_user}@${pi_ip}:~/navipod"
    echo "→ Copying UI assets..."
    scp -r ui/ "${pi_user}@${pi_ip}:~/"
    echo "✓ Deployed to ${pi_user}@${pi_ip}"
fi
