#!/usr/bin/env bash
# Build the macOS release files, on a Mac (Apple Silicon or Intel):
#
#   radiotrope-<ver>-macos-universal.dmg   Radiotrope.app and a link to /Applications
#   radiotrope-<ver>-macos-universal.zip   Radiotrope.app alone (Homebrew, later self-update)
#
# Usage: packaging/macos/bundle.sh [OUT_DIR]     (default: target/dist)
#
# The three programs are built for arm64 and x86_64 and joined with lipo
# (one download for every Mac), and all go inside the bundle:
#
#   Radiotrope.app/Contents/
#     Info.plist, PkgInfo
#     MacOS/radiotrope, radiotrope-cli, radiotrope-probe   (universal)
#     Resources/Radiotrope.icns, update-mode, LICENSE, THIRD-PARTY-LICENSES
#     Resources/man/man1/*.1, Resources/completions/{bash,zsh,fish}/
#
# The Homebrew cask (phase 3) links the CLI, the probe, the man pages and the
# completions from there into the PATH.
#
# Nothing is signed with a certificate yet: the bundle gets an ad-hoc
# signature (arm64 code must carry one to run), so Gatekeeper still stops a
# downloaded copy (README, "macOS (experimental)").
#
# Needs Xcode's command line tools (lipo, codesign, iconutil, hdiutil),
# Rust with both targets (`rustup target add aarch64-apple-darwin
# x86_64-apple-darwin`) and CMake (`brew install cmake`).
#
#   SKIP_BUILD=1          reuse target/<target>/release and target/generated
#   ARCHS="aarch64"       build only some of: aarch64 x86_64 (a quick local
#                         bundle; the files are then named after that arch)
#   FORMATS="dmg"         make only some of: dmg zip
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT=$PWD
mkdir -p "${1:-target/dist}"
OUT=$(cd "${1:-target/dist}" && pwd)
ARCHS=${ARCHS:-aarch64 x86_64}
FORMATS=${FORMATS:-dmg zip}
PROGRAMS="radiotrope radiotrope-cli radiotrope-probe"
APP_ID=app.radiotrope.Radiotrope

# The oldest macOS the programs run on: the compiler's and the linker's
# target, and LSMinimumSystemVersion. Raising it is an owner decision
# (macOS review, section 5).
MIN_MACOS=12.0
export MACOSX_DEPLOYMENT_TARGET=$MIN_MACOS

VERSION=$(cargo pkgid -p radiotrope-app | sed 's/.*[#@]//')
# CFBundleVersion and CFBundleShortVersionString are three numbers: 0.2.0-rc.1
# shows as 0.2.0 in Finder (the file names keep the whole version)
PLIST_VERSION=${VERSION%%-*}

if [ "$ARCHS" = "aarch64 x86_64" ] || [ "$ARCHS" = "x86_64 aarch64" ]; then
    FLAVOUR=universal
else
    FLAVOUR=${ARCHS// /-}
fi
NAME=radiotrope-$VERSION-macos-$FLAVOUR

wants() { [[ " $FORMATS " == *" $1 "* ]]; }
die() { echo "error: $*" >&2; exit 1; }
# Mach-O names for the Rust ones
lipo_arch() { case $1 in aarch64) echo arm64 ;; *) echo "$1" ;; esac; }

[ "$(uname -s)" = Darwin ] || die "this script runs on macOS (lipo, codesign, iconutil, hdiutil)"

echo "Radiotrope $VERSION for macOS $MIN_MACOS+ ($ARCHS): $FORMATS -> $OUT"

# 1. The programs, man pages and completions
if [ -z "${SKIP_BUILD:-}" ]; then
    for arch in $ARCHS; do
        cargo build --release --locked --target "$arch-apple-darwin" \
            -p radiotrope-app -p radiotrope-cli -p radiotrope-probe
    done
    cargo run --locked --quiet -p xtask -- man-and-completions target/generated
fi

# 2. One universal file per program, checked: architectures, minimum macOS,
#    nothing linked from outside the system (no Homebrew, no FDK)
LIPO=target/lipo
rm -rf "$LIPO"
mkdir -p "$LIPO"
for p in $PROGRAMS; do
    slices=()
    for arch in $ARCHS; do
        f=target/$arch-apple-darwin/release/$p
        [ -f "$f" ] || die "$f is missing: build the release first"
        slices+=("$f")
    done
    lipo -create -output "$LIPO/$p" "${slices[@]}"

    want=$(for arch in $ARCHS; do lipo_arch "$arch"; done | sort | xargs)
    got=$(lipo -archs "$LIPO/$p" | tr ' ' '\n' | sort | xargs)
    [ "$got" = "$want" ] || die "$p has the architectures '$got', not '$want'"

    for arch in $ARCHS; do
        a=$(lipo_arch "$arch")
        minos=$(vtool -arch "$a" -show-build "$LIPO/$p" | awk '$1 == "minos" { print $2 }')
        [ "$minos" = "$MIN_MACOS" ] || die "$p ($a) asks for macOS '$minos', not $MIN_MACOS"
    done

    libs=$(otool -L "$LIPO/$p" | tail -n +2 | awk '{ print $1 }' | sort -u)
    if grep -i fdk <<<"$libs"; then
        die "$p links FDK AAC; it must only load it at run time"
    fi
    foreign=$(grep -Ev '^(/usr/lib/|/System/Library/)' <<<"$libs" || true)
    [ -z "$foreign" ] || die "$p links libraries a Mac doesn't have: $foreign"
    echo "$p: $got, macOS $MIN_MACOS+, $(wc -l <<<"$libs" | xargs) system libraries"
done

# 3. The bundle
STAGE=$OUT/stage
APP=$STAGE/Radiotrope.app
rm -rf "$STAGE"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

sed -e "s/@VERSION@/$PLIST_VERSION/g" -e "s/@MIN_MACOS@/$MIN_MACOS/g" \
    packaging/macos/Info.plist > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist"
[ "$(plutil -extract CFBundleIdentifier raw "$APP/Contents/Info.plist")" = "$APP_ID" ] ||
    die "Info.plist has the wrong bundle identifier"
printf 'APPL????' > "$APP/Contents/PkgInfo"

for p in $PROGRAMS; do
    install -m755 "$LIPO/$p" "$APP/Contents/MacOS/$p"
done

# A bundle doesn't update itself yet: About says when a newer version is out
echo notify > "$APP/Contents/Resources/update-mode"
install -m644 LICENSE THIRD-PARTY-LICENSES "$APP/Contents/Resources/"

GENERATED=target/generated
if [ -d "$GENERATED/man" ]; then
    mkdir -p "$APP/Contents/Resources/man/man1" \
        "$APP/Contents/Resources/completions/bash" \
        "$APP/Contents/Resources/completions/zsh" \
        "$APP/Contents/Resources/completions/fish"
    for p in $PROGRAMS; do
        install -m644 "$GENERATED/man/$p.1" "$APP/Contents/Resources/man/man1/"
        install -m644 "$GENERATED/completions/bash/$p" "$APP/Contents/Resources/completions/bash/"
        install -m644 "$GENERATED/completions/zsh/_$p" "$APP/Contents/Resources/completions/zsh/"
        install -m644 "$GENERATED/completions/fish/$p.fish" "$APP/Contents/Resources/completions/fish/"
    done
else
    echo "warning: no $GENERATED/man: the bundle has no man pages or completions" >&2
fi

# The icon, from assets/icons. There is no 1024 px picture yet: 512@2x is the
# 512 one scaled up until the owner draws a macOS-shaped icon.
ICONSET=target/Radiotrope.iconset
rm -rf "$ICONSET"
mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
    cp "assets/icons/icon-$s.png" "$ICONSET/icon_${s}x${s}.png"
done
cp assets/icons/icon-32.png "$ICONSET/icon_16x16@2x.png"
cp assets/icons/icon-64.png "$ICONSET/icon_32x32@2x.png"
cp assets/icons/icon-256.png "$ICONSET/icon_128x128@2x.png"
cp assets/icons/icon-512.png "$ICONSET/icon_256x256@2x.png"
sips -z 1024 1024 assets/icons/icon-512.png --out "$ICONSET/icon_512x512@2x.png" >/dev/null
iconutil -c icns -o "$APP/Contents/Resources/Radiotrope.icns" "$ICONSET"

# 4. Ad-hoc signature of the programs and the bundle, which seals Info.plist
#    and Resources (--deep: the CLI and the probe too). Phase 3 replaces this
#    with the Developer ID signature (entitlements.plist, hardened runtime,
#    inner programs first) and notarization.
codesign --force --deep --sign - "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"

# 5. The downloads
if wants zip; then
    rm -f "$OUT/$NAME.zip"
    # ditto, not zip: it keeps the permissions and the signature's extended
    # attributes the way Finder and Homebrew unpack them
    ditto -c -k --sequesterRsrc --keepParent "$APP" "$OUT/$NAME.zip"
fi

if wants dmg; then
    DMGROOT=$OUT/dmg-root
    rm -rf "$DMGROOT" "$OUT/$NAME.dmg"
    mkdir -p "$DMGROOT"
    ditto "$APP" "$DMGROOT/Radiotrope.app"
    ln -s /Applications "$DMGROOT/Applications"
    # hdiutil now and then fails with "Resource busy" on CI machines: retry
    for attempt in 1 2 3 4 5; do
        if hdiutil create -volname Radiotrope -srcfolder "$DMGROOT" -fs HFS+ \
            -format UDZO -imagekey zlib-level=9 -ov "$OUT/$NAME.dmg"; then
            break
        fi
        [ "$attempt" -lt 5 ] || die "hdiutil create failed"
        echo "hdiutil create failed (attempt $attempt), trying again"
        sleep $((attempt * 5))
    done
    hdiutil verify "$OUT/$NAME.dmg"
    rm -rf "$DMGROOT"
fi

echo
du -sh "$APP"
ls -l "$OUT"
