#!/usr/bin/env bash
# Build the Linux release files for this machine's architecture:
#
#   radiotrope-<ver>-linux-<arch>.tar.gz       the programs and their files, to unpack anywhere
#   radiotrope_<ver>-1_<amd64|arm64>.deb       Debian, Ubuntu (cargo-deb)
#   radiotrope-<ver>-1.<arch>.rpm              Fedora, openSUSE (cargo-generate-rpm)
#   Radiotrope-<ver>-<arch>.AppImage (+.zsync) any distribution (packaging/appimage/build.sh)
#
# Usage: packaging/linux/dist.sh [OUT_DIR]     (default: target/dist)
#
# Release builds run this in an ubuntu:22.04 container (glibc 2.35), so the
# files run on that and anything newer. Needs packaging/linux/build-deps.sh,
# Rust, cargo-deb and cargo-generate-rpm (versions in package-linux.yml).
#
#   SKIP_BUILD=1          reuse what is in target/release and target/generated
#   FORMATS="tar deb"     build only some of: tar deb rpm appimage
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT=$PWD
OUT=$(realpath -m "${1:-target/dist}")
FORMATS=${FORMATS:-tar deb rpm appimage}
VERSION=$(cargo pkgid -p radiotrope-app | sed 's/.*[#@]//')
ARCH=$(uname -m)
# Same timestamps in the archives for the same commit
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || date +%s)}
export SOURCE_DATE_EPOCH

wants() { [[ " $FORMATS " == *" $1 "* ]]; }

echo "Radiotrope $VERSION for $ARCH: $FORMATS -> $OUT"
mkdir -p "$OUT"

# The three programs (release profile, --locked), man pages, completions
if [ -z "${SKIP_BUILD:-}" ]; then
    make all
fi

# The installed tree under /usr. The deb and rpm metadata in
# crates/radiotrope-app/Cargo.toml take their files from here.
STAGE=target/dist-stage
rm -rf "$STAGE"
make install DESTDIR="$ROOT/$STAGE" PREFIX=/usr
gzip -9nf "$STAGE"/usr/share/man/man1/*.1
install -Dm644 README.md LICENSE THIRD-PARTY-LICENSES -t "$STAGE/usr/share/doc/radiotrope"

if wants tar; then
    name="radiotrope-$VERSION-linux-$ARCH"
    rm -rf target/dist-tar
    # bin/ and share/ at the top, so the whole tree can be copied into
    # ~/.local or /usr/local as it is
    make install DESTDIR="$ROOT/target/dist-tar/$name" PREFIX=
    gzip -9nf "target/dist-tar/$name"/share/man/man1/*.1
    install -m644 README.md LICENSE THIRD-PARTY-LICENSES "target/dist-tar/$name/"
    tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$SOURCE_DATE_EPOCH" \
        -C target/dist-tar -cf - "$name" | gzip -9n > "$OUT/$name.tar.gz"
fi

if wants deb; then
    cargo deb -p radiotrope-app --no-build --output "$OUT"
fi

if wants rpm; then
    # builtin: the requirements are the libraries' sonames from the ELF
    # headers, which Fedora and openSUSE both resolve
    cargo generate-rpm -p crates/radiotrope-app --auto-req builtin --output "$OUT/"
fi

if wants appimage; then
    packaging/appimage/build.sh "$OUT"
fi

echo
ls -l "$OUT"
