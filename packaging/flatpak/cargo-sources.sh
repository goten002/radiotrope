#!/usr/bin/env bash
# Write packaging/flatpak/cargo-sources.json: every crate in Cargo.lock as a
# flatpak-builder source, so the Flatpak builds offline. Made fresh for each
# build (it is a tool output, not tracked), with flatpak-builder-tools'
# flatpak-cargo-generator, pinned by commit and checksum.
#
# Usage: packaging/flatpak/cargo-sources.sh     (needs python3 with venv)
set -euo pipefail

cd "$(dirname "$0")/../.."
COMMIT=9d86d336a39b9af38b86f114b6ce5820fe260f91
SHA256=0a2db6be87d75910facef28ab46d4d6460802e8419ab850d0caa6a364d26b380
WORK=target/flatpak-cargo-generator
mkdir -p "$WORK"

if [ ! -f "$WORK/flatpak-cargo-generator.py" ]; then
    curl -fsSL --retry 3 -o "$WORK/flatpak-cargo-generator.py" \
        "https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/$COMMIT/cargo/flatpak-cargo-generator.py"
fi
echo "$SHA256  $WORK/flatpak-cargo-generator.py" | sha256sum -c --quiet -

PY=$WORK/venv/bin/python
# Only the venv's packages (the Flatpak build image sets PYTHONPATH)
unset PYTHONPATH
if ! "$PY" -I -c "import aiohttp, tomlkit, yaml" 2>/dev/null; then
    python3 -m venv --clear "$WORK/venv"
    # The generator's own requirements (its pyproject.toml at that commit)
    "$PY" -m pip install --quiet --disable-pip-version-check \
        'aiohttp>=3.9.5,<4' 'PyYAML>=6.0.2,<7' 'tomlkit>=0.13.3,<1'
    "$PY" -I -c "import aiohttp, tomlkit, yaml"
fi
"$PY" -I "$WORK/flatpak-cargo-generator.py" Cargo.lock -o packaging/flatpak/cargo-sources.json
echo "Wrote packaging/flatpak/cargo-sources.json"
