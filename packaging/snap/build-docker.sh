#!/usr/bin/env bash
# Build the snap in Docker, without LXD, Multipass or snapd on this machine:
# snapcraft runs in destructive mode inside Canonical's core24 snapcraft
# image, on a copy of the committed and uncommitted files (not target/).
#
#   packaging/snap/build-docker.sh [OUT_DIR]      (default: target/dist)
#
# Installing and running the .snap needs snapd: do that on a desktop, or see
# .github/workflows/package-snap.yml for the CI check.
#
#   SNAP_DOCKER_LIMITS="--cpus=3 --memory=8g"   resources for the container
#   SNAP_CACHE_VOLUME=radiotrope-snap-cache     keeps rustup, cargo's
#                                               downloads and the compiled
#                                               crates between builds
#                                               (empty: no volume)
set -euo pipefail

cd "$(dirname "$0")/../.."
OUT=$(realpath -m "${1:-target/dist}")
IMAGE=ghcr.io/canonical/snapcraft:8_core24
LIMITS=${SNAP_DOCKER_LIMITS---cpus=3 --memory=8g}
VOLUME=${SNAP_CACHE_VOLUME-radiotrope-snap-cache}
mkdir -p "$OUT"

volume_args=()
if [ -n "$VOLUME" ]; then
    volume_args=(-v "$VOLUME:/cache" -e RUSTUP_HOME=/cache/rustup -e CARGO_HOME=/cache/cargo
        -e CARGO_TARGET_DIR=/cache/target)
fi

# The tracked and untracked files, minus what git ignores (target/ and such)
# shellcheck disable=SC2086 # LIMITS holds several options
git ls-files -z --cached --others --exclude-standard \
    | tar --null -T - -cf - \
    | docker run --rm -i $LIMITS "${volume_args[@]}" \
        -e "HOST_UID=$(id -u)" -e "HOST_GID=$(id -g)" \
        -v "$OUT:/out" --entrypoint bash "$IMAGE" -c '
set -eu
mkdir -p /src && tar -xf - -C /src
cd /src
mkdir -p snap && cp packaging/snap/snapcraft.yaml snap/
# The gnome extension takes its launcher from snapcraft'"'"'s own files, which
# this image keeps elsewhere than the snapcraft snap does
if [ ! -d /usr/share/snapcraft/extensions/desktop ]; then
    for ext in /usr/lib/python3*/site-packages/extensions; do
        rm -rf /usr/share/snapcraft/extensions
        mkdir -p /usr/share/snapcraft && ln -s "$ext" /usr/share/snapcraft/extensions
    done
fi
# Part scripts call craftctl, which this image keeps off the PATH
export PATH="$PATH:/usr/libexec/snapcraft"
apt-get update -qq
snapcraft pack --destructive-mode --verbosity=brief
cp ./*.snap /out/
chown "$HOST_UID:$HOST_GID" /out/*.snap
'
ls -l "$OUT"/*.snap
