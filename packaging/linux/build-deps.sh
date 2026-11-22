#!/bin/sh
# Install what the Linux release build needs on Ubuntu 22.04 or Debian 12
# (the release container is ubuntu:22.04). Run as root. Rust comes from
# rustup separately.
#
#   packaging/linux/build-deps.sh
set -eu

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
    build-essential cmake pkg-config make git curl ca-certificates jq file \
    libasound2-dev libfontconfig1-dev \
    libxkbcommon0 libxkbcommon-x11-0 \
    desktop-file-utils appstream zsync xz-utils

# No OpenSSL: HTTPS is rustls (aws-lc-rs, built by cmake and the C compiler).
# No libfdk-aac: the programs load the system's at run time, if it is there.
# libxkbcommon*: not linked, loaded at run time by winit; the AppImage
# bundles them, so they must be on the build machine.
# desktop-file-utils, appstream: appimagetool checks the desktop file and
# the metainfo with them. zsync: the AppImage's .zsync update file.
