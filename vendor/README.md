# vendor

Patched copies of dependencies, used through `[patch.crates-io]` in the root `Cargo.toml`. Each keeps its upstream license.

| Directory | Upstream | Change | Remove when |
|---|---|---|---|
| `i-slint-renderer-femtovg` | [i-slint-renderer-femtovg 1.19.0](https://crates.io/crates/i-slint-renderer-femtovg/1.19.0), as published on crates.io without `Cargo.lock` and `Cargo.toml.orig` (GPL-3.0-only OR Slint Royalty-free 2.0 OR Slint Software 3.0, see its `LICENSES/`) | Two changes, below | Both are in the Slint that is used: delete the directory and the `[patch.crates-io]` entry, then run `cargo update -p i-slint-renderer-femtovg` |
| `muda` | [muda 0.21.2](https://crates.io/crates/muda/0.21.2), as published on crates.io without `Cargo.lock` and `Cargo.toml.orig` (Apache-2.0 OR MIT, see its `LICENSE-*`) | One change, below | muda calls every handler itself, or only one of tray-icon and Slint uses muda: delete the directory and the `[patch.crates-io]` entry, then run `cargo update -p muda` |

## Change to `muda`

**Every handler set for menu events is called** (`src/menu_event.rs`; the lines marked `Radiotrope:`). Upstream keeps the handler of the first `MenuEvent::set_event_handler` call in a `OnceLock` and silently drops every later one. Two libraries here use muda and each sets a handler: tray-icon's menu, through `tray.rs`, and Slint's winit backend for the native menu bar on macOS (`ui/platform/macos/window.slint`). The tray is made first, so Slint's handler was dropped and no item of the macOS menu bar did anything: not Quit, not a check mark (found by `scripts/macos-menu-check.sh` in CI, which chooses the items of the real menu bar).

With the change the first handler is kept as before and later ones are added to a list; an event goes to all of them. Each tells its own items by their ids (the tray's `Action::from_menu`, Slint's `window|generation|entry|type`) and ignores the rest. A program that sets one handler behaves as with upstream's muda. That is this app on Linux and Windows: only the tray sets one there (Slint would set its own for a `MenuBar` or a native context menu, and the app has neither on those systems).

The copy also has `#![allow(unused_imports, dead_code)]` at the top of `src/lib.rs`: as a path dependency its warnings are shown (cargo hides those of crates from the registry), and on Linux, where muda is built without a toolkit, it has a few.

## Changes to `i-slint-renderer-femtovg`

1. **Glyphs on whole device pixels** (`itemrenderer.rs`, `draw_glyph_run`): the fix from [slint-ui/slint#15003](https://github.com/slint-ui/slint/pull/15003), so parts of letters are no longer drawn one pixel off ([slint-ui/slint#15000](https://github.com/slint-ui/slint/issues/15000)). Upstream from Slint 1.19.1.

2. **The text layout cache is kept when the graphics context goes** (`lib.rs`, `clear_graphics_context` and `set_window_adapter`; the lines marked `Radiotrope:`). Upstream calls `text_layout_cache.clear_all()` in both. The first runs whenever the winit backend suspends the renderer, which on Wayland is every hide of the window (to the tray): Wayland can't hide a window, so it is destroyed.

   Why that is a bug: a `Text`'s `preferred-width` depends on its text only through its cache entry (the entry's tracker is what tells the binding that the text changed). Dropping the entries drops the trackers without telling the bindings, so every width computed before the hide is never computed again when the text changes later. The status chip kept the width of "Playing 2:34" for "Connecting…" (cut to "Connecting.") and for an error (cut to "Can't play…"), and was far too wide for "Stopped".

   The entries hold shaped text, no graphics resources, so nothing needs them gone; a new scale factor still clears them (`clear_if_scale_factor_changed`), and a component that is destroyed takes its own with it (`free_graphics_resources`).

   Not fixed upstream as of Slint 1.19.1 and `master` on 2026-11-25 (the Skia renderer has the same call in its `suspend`). When Slint is updated, check `clear_graphics_context` before dropping this copy. `cargo test -p radiotrope-app --bin radiotrope labels` fails without the change: it hides and shows a window drawn by FemtoVG and looks at the chips (`crates/radiotrope-app/src/labels_femtovg_test.rs`).
