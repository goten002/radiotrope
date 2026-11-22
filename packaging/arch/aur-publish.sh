#!/usr/bin/env bash
# Push the AUR packages `radiotrope` (PKGBUILD) and `radiotrope-bin`
# (PKGBUILD-bin) for the version in Cargo.toml. Run by release.yml's `aur`
# job in an archlinux:base-devel container, after the release v<version> is
# published: the checksums come from the tag's source archive and the
# release's tarballs.
#
# Needs: AUR_SSH_KEY (a private key registered with the AUR account that
# maintains both packages), git, openssh, pacman-contrib (updpkgsums).
# DRY_RUN=1 does everything but the push.
set -euo pipefail

cd "$(dirname "$0")/../.."
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1)
case "$VERSION" in
*-*)
    echo "$VERSION is a pre-release: the AUR gets stable releases only" >&2
    exit 1
    ;;
esac

# makepkg and updpkgsums refuse to run as root
if [ "$(id -u)" = 0 ]; then
    id builder >/dev/null 2>&1 || useradd -m builder
    as_builder() { su builder -c "$*"; }
else
    as_builder() { sh -c "$*"; }
fi

WORK=$(mktemp -d)
chmod 777 "$WORK"
trap 'rm -rf "$WORK"' EXIT

mkdir -p ~/.ssh
chmod 700 ~/.ssh
if [ -n "${AUR_SSH_KEY:-}" ]; then
    printf '%s\n' "$AUR_SSH_KEY" > ~/.ssh/aur
    chmod 600 ~/.ssh/aur
fi
# The AUR's ed25519 host key, as listed on https://aur.archlinux.org (home
# page, "SSH fingerprints"); a changed key stops the push
cat >> ~/.ssh/known_hosts <<'EOF'
aur.archlinux.org ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEuBKrPzbawxA/k2g6NcyV5jmqwJ2s+zpgZGZ7tpLIcN
EOF
# The clones belong to the builder user
git config --global --add safe.directory "*"
export GIT_SSH_COMMAND="ssh -i $HOME/.ssh/aur -o IdentitiesOnly=yes"

# The -bin checksums must be the ones in the release's SHA256SUMS
check_bin_sums() {
    curl -fsSL -o "$WORK/SHA256SUMS" \
        "https://github.com/goten002/radiotrope/releases/download/v$VERSION/SHA256SUMS"
    local arch want
    for arch in x86_64 aarch64; do
        want=$(awk -v f="radiotrope-$VERSION-linux-$arch.tar.gz" '$2 == f { print $1 }' "$WORK/SHA256SUMS")
        if [ -z "$want" ] || ! grep -q "$want" "$1"; then
            echo "radiotrope-bin: the $arch checksum doesn't match the release's SHA256SUMS" >&2
            exit 1
        fi
    done
}

# publish <file in packaging/arch> <AUR package name>
publish() {
    local src=$1 name=$2 dir="$WORK/$2"
    git clone "ssh://aur@aur.archlinux.org/$name.git" "$dir"
    cp "packaging/arch/$src" "$dir/PKGBUILD"
    chown -R builder: "$dir" 2>/dev/null || true
    sed -i "s/^pkgver=.*/pkgver=$VERSION/; s/^pkgrel=.*/pkgrel=1/" "$dir/PKGBUILD"
    # Real checksums in place of SKIP (downloads the sources)
    as_builder "cd '$dir' && updpkgsums && makepkg --printsrcinfo > .SRCINFO"
    if [ "$name" = radiotrope-bin ]; then
        check_bin_sums "$dir/PKGBUILD"
    fi
    git -C "$dir" add PKGBUILD .SRCINFO
    if git -C "$dir" diff --cached --quiet; then
        echo "$name: already at $VERSION"
        return
    fi
    git -C "$dir" -c user.name="George Alexiou" -c user.email="goten002@users.noreply.github.com" \
        commit -m "Update to $VERSION"
    git -C "$dir" show --stat HEAD
    if [ -n "${DRY_RUN:-}" ]; then
        echo "$name: DRY_RUN, not pushed"
    else
        git -C "$dir" push origin HEAD:master
    fi
}

publish PKGBUILD radiotrope
publish PKGBUILD-bin radiotrope-bin

echo "AUR updated to $VERSION"
