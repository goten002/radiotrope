#!/usr/bin/env bash
# Convenience wrapper for building the Radiotrope Yocto image.
# Usage: cd <project-root> && yocto/scripts/setup-build.sh
#
# Prerequisites:
#   - kas installed: pip install kas
#   - podman or docker (for kas-container builds on non-Debian hosts)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

cd "$PROJECT_ROOT"

if command -v kas-container &>/dev/null; then
    BUILD_CMD="kas-container build"
elif command -v kas &>/dev/null; then
    BUILD_CMD="kas build"
else
    echo "Error: neither 'kas-container' nor 'kas' is installed."
    echo "Install with: pip install kas"
    exit 1
fi

echo "Building Radiotrope image for Raspberry Pi 3B+..."
echo "Using: $BUILD_CMD"
echo "This will take a long time on the first build (several hours)."
echo ""

$BUILD_CMD kas-radiotrope-pi3.yml

echo ""
echo "Build complete!"
echo "Flash the image to an SD card:"
echo "  sudo dd if=build/tmp/deploy/images/raspberrypi3-64/radiotrope-image-raspberrypi3-64.rpi-sdimg of=/dev/sdX bs=4M status=progress"
