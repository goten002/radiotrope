#!/usr/bin/env bash
# Build Radiotrope-<ver>-<arch>.flatpak, a single-file bundle, from the
# manifest. CI runs it in the ghcr.io/flathub-infra/flatpak-github-actions
# image (privileged); locally it needs flatpak and flatpak-builder.
#
# Usage: packaging/flatpak/build.sh [OUT_DIR]     (default: target/dist)
#
# The Sdk, its rust-stable extension and the runtime are installed for the
# user from Flathub. (Updating the ones preinstalled system-wide in the CI
# image fails with "Invalid cross-device link".)
set -euo pipefail

cd "$(dirname "$0")/../.."
OUT=$(realpath -m "${1:-target/dist}")
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1)
ARCH=$(uname -m)
APP_ID=app.radiotrope.Radiotrope
FLATHUB=https://dl.flathub.org/repo/flathub.flatpakrepo
WORK=target/flatpak
BUNDLE="$OUT/Radiotrope-$VERSION-$ARCH.flatpak"

packaging/flatpak/cargo-sources.sh
mkdir -p "$OUT" "$WORK"
flatpak --user remote-add --if-not-exists flathub "$FLATHUB"
flatpak-builder --user --install-deps-from=flathub --disable-rofiles-fuse \
    --force-clean --state-dir="$WORK/state" --repo="$WORK/repo" \
    "$WORK/build-dir" "packaging/flatpak/$APP_ID.yml"
flatpak build-bundle --runtime-repo="$FLATHUB" "$WORK/repo" "$BUNDLE" "$APP_ID"
echo "Built $BUNDLE"
