#!/bin/bash
# macOS: start the app and look at its menu bar as a person would (the
# menus, their items, check marks and keys; scripts/macos-menu-check.swift),
# then choose Quit in the application menu, which must go through the app's
# own exit: the settings are saved. For CI (ci-macos.yml) and for a Mac by
# hand, from the repository's folder after `cargo build`:
#
#   scripts/macos-menu-check.sh
#
# The evidence is left in smoke/: menu.txt (the menus as text), menu-bar.png
# and menu-open.png (pictures of the screen). The app runs with a home
# folder of its own (smoke/home), not the user's.
set -u

mkdir -p smoke/home
evidence="$PWD/smoke"
export HOME="$evidence/home"
support="$HOME/Library/Application Support/radiotrope"

target/debug/radiotrope > smoke/menu-app.log 2>&1 &
pid=$!
sleep 15
if ! kill -0 "$pid" 2>/dev/null; then
  echo "::error::the app did not stay open"
  cat smoke/menu-app.log
  exit 1
fi
screencapture -x smoke/menu-bar.png || true
# Saved again by the exit that Quit must lead to
rm -f "$support/settings.json"

swift scripts/macos-menu-check.swift "$pid" "$evidence"
checked=$?

failed=0
if [ "$checked" -eq 3 ]; then
  # Nothing was chosen: end the app as the earlier steps do
  kill -TERM "$pid"
elif [ "$checked" -ne 0 ]; then
  failed=1
fi
for _ in $(seq 20); do kill -0 "$pid" 2>/dev/null || break; sleep 1; done
if kill -0 "$pid" 2>/dev/null; then
  echo "::error::the app did not quit from its menu"
  kill -KILL "$pid"
  failed=1
elif [ "$checked" -ne 3 ] && [ ! -f "$support/settings.json" ]; then
  echo "::error::Quit in the application menu did not save the settings"
  failed=1
fi
echo "--- the app's output"; tail -50 smoke/menu-app.log
exit $failed
