#!/usr/bin/env bash
# Build Radiotrope-<ver>-<arch>.AppImage and its .zsync update file from
# what `make` built (target/release, target/generated).
#
# Usage: packaging/appimage/build.sh [OUT_DIR]     (default: target/dist)
#
# Normally called by packaging/linux/dist.sh, in the ubuntu:22.04 release
# container: an AppImage runs on systems with the glibc it was built
# against or newer.
#
# - linuxdeploy copies in the libraries the programs link, except those the
#   AppImage excludelist says must come from the system (glibc, ALSA,
#   fontconfig, GL, X11, Wayland...). libxkbcommon is loaded at run time by
#   winit, so linuxdeploy can't see it: it is passed by hand.
# - appimagetool packs it with the static type2 runtime (no libfuse2 needed
#   on the host) and embeds the update information, so AppImageUpdate, Gear
#   Lever and AppImageLauncher can update from the GitHub releases.
# - The system tray needs nothing here: it is plain D-Bus (StatusNotifierItem).
#
# The tools are pinned by version and checksum. To move to newer ones, change
# the version and the sums together.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT=$PWD
OUT=$(realpath -m "${1:-target/dist}")
VERSION=$(cargo pkgid -p radiotrope-app | sed 's/.*[#@]//')
ARCH=$(uname -m)
APP_ID=app.radiotrope.Radiotrope

LINUXDEPLOY_VERSION=1-alpha-20251107-1
APPIMAGETOOL_VERSION=1.9.1
RUNTIME_VERSION=20251108
case "$ARCH" in
x86_64)
    LINUXDEPLOY_SHA256=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d
    APPIMAGETOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
    RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
    ;;
aarch64)
    LINUXDEPLOY_SHA256=620095110d693282b8ebeb244a95b5e911cf8f65f76c88b4b47d16ae6346fcff
    APPIMAGETOOL_SHA256=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158
    RUNTIME_SHA256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
    ;;
*)
    echo "No AppImage tools for $ARCH" >&2
    exit 1
    ;;
esac

TOOLS=target/appimage-tools
mkdir -p "$TOOLS" "$OUT"

# fetch URL FILE SHA256: download once, check the sum every time
fetch() {
    if [ ! -f "$TOOLS/$2" ]; then
        curl -fsSL --retry 3 -o "$TOOLS/$2.part" "$1"
        mv "$TOOLS/$2.part" "$TOOLS/$2"
    fi
    echo "$3  $TOOLS/$2" | sha256sum -c --quiet -
    chmod +x "$TOOLS/$2"
}
fetch "https://github.com/linuxdeploy/linuxdeploy/releases/download/$LINUXDEPLOY_VERSION/linuxdeploy-$ARCH.AppImage" \
    "linuxdeploy-$LINUXDEPLOY_VERSION-$ARCH.AppImage" "$LINUXDEPLOY_SHA256"
fetch "https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_VERSION/appimagetool-$ARCH.AppImage" \
    "appimagetool-$APPIMAGETOOL_VERSION-$ARCH.AppImage" "$APPIMAGETOOL_SHA256"
fetch "https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_VERSION/runtime-$ARCH" \
    "runtime-$RUNTIME_VERSION-$ARCH" "$RUNTIME_SHA256"

# appimagetool reads the architecture from ARCH
export ARCH
# The tools are AppImages themselves; containers have no FUSE
export APPIMAGE_EXTRACT_AND_RUN=1

APPDIR=$ROOT/target/AppDir
rm -rf "$APPDIR"
make install DESTDIR="$APPDIR" PREFIX=/usr
install -Dm644 README.md LICENSE THIRD-PARTY-LICENSES -t "$APPDIR/usr/share/doc/radiotrope"

# Libraries loaded at run time (dlopen), which linuxdeploy can't find itself
extra_libs=()
for lib in libxkbcommon.so.0 libxkbcommon-x11.so.0; do
    path=$(ldconfig -p | awk -v lib="$lib" '$1 == lib && /x86-64|AArch64|aarch64/ { print $NF; exit }')
    if [ -z "$path" ]; then
        echo "$lib is not installed (see packaging/linux/build-deps.sh)" >&2
        exit 1
    fi
    extra_libs+=(--library "$path")
done

# linuxdeploy wants an icon file named like the desktop file's Icon=
icon_dir=$(mktemp -d)
trap 'rm -rf "$icon_dir"' EXIT
cp assets/icons/icon-256.png "$icon_dir/$APP_ID.png"

"$TOOLS/linuxdeploy-$LINUXDEPLOY_VERSION-$ARCH.AppImage" \
    --appdir "$APPDIR" \
    --executable "$APPDIR/usr/bin/radiotrope" \
    --executable "$APPDIR/usr/bin/radiotrope-cli" \
    --executable "$APPDIR/usr/bin/radiotrope-probe" \
    --desktop-file "$APPDIR/usr/share/applications/$APP_ID.desktop" \
    --icon-file "$icon_dir/$APP_ID.png" \
    --custom-apprun packaging/appimage/AppRun \
    "${extra_libs[@]}"

name="Radiotrope-$VERSION-$ARCH.AppImage"
update_info="gh-releases-zsync|goten002|radiotrope|latest|Radiotrope-*-$ARCH.AppImage.zsync"
# appimagetool writes the .zsync next to the AppImage when zsyncmake is
# installed; work in OUT so both land there
(
    cd "$OUT"
    rm -f "$name" "$name.zsync"
    "$ROOT/$TOOLS/appimagetool-$APPIMAGETOOL_VERSION-$ARCH.AppImage" \
        --runtime-file "$ROOT/$TOOLS/runtime-$RUNTIME_VERSION-$ARCH" \
        --updateinformation "$update_info" \
        "$APPDIR" "$name"
    test -f "$name.zsync" || { echo "no $name.zsync: is zsync installed?" >&2; exit 1; }
)
echo "Built $OUT/$name"
