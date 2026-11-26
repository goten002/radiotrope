# macOS icons

Drawn for Radiotrope from its own logo (`assets/logo/radiotrope-logo.png`)
and approved by the owner (November 2026); same licence as the rest of the
repository. Nothing here comes from a third party.

| File | What | Used by |
|------|------|---------|
| `radiotrope-macos-1024.png` | The app icon: the logo on a white rounded tile, 824 px on a 1024 px canvas with margin and shadow (Apple's layout). The light tile was the owner's choice over a dark one | `packaging/macos/bundle.sh` scales it into every size of `Radiotrope.icns` |
| `tray-template.png` | The menu bar glyph, 64 x 64, black on transparent: a rounded triangle with the logo's fold cut out. A template image: macOS draws its shape black, white or tinted. A draft the owner may replace: copy another PNG over it | `crates/radiotrope-app/src/tray.rs`, on macOS only |

Linux and Windows keep `../icon-*.png` and `../radiotrope.ico`.
How to change either: `packaging/macos/README.md`, "Icons".
