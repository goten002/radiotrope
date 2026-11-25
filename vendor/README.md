# vendor

Patched copies of dependencies, used through `[patch.crates-io]` in the root `Cargo.toml`. Each keeps its upstream license.

| Directory | Upstream | Change | Remove when |
|---|---|---|---|
| `i-slint-renderer-femtovg` | [i-slint-renderer-femtovg 1.19.0](https://crates.io/crates/i-slint-renderer-femtovg/1.19.0), as published on crates.io without `Cargo.lock` and `Cargo.toml.orig` (GPL-3.0-only OR Slint Royalty-free 2.0 OR Slint Software 3.0, see its `LICENSES/`) | Two changes, below | Both are in the Slint that is used: delete the directory and the `[patch.crates-io]` entry, then run `cargo update -p i-slint-renderer-femtovg` |

## Changes to `i-slint-renderer-femtovg`

1. **Glyphs on whole device pixels** (`itemrenderer.rs`, `draw_glyph_run`): the fix from [slint-ui/slint#15003](https://github.com/slint-ui/slint/pull/15003), so parts of letters are no longer drawn one pixel off ([slint-ui/slint#15000](https://github.com/slint-ui/slint/issues/15000)). Upstream from Slint 1.19.1.

2. **The text layout cache is kept when the graphics context goes** (`lib.rs`, `clear_graphics_context` and `set_window_adapter`; the lines marked `Radiotrope:`). Upstream calls `text_layout_cache.clear_all()` in both. The first runs whenever the winit backend suspends the renderer, which on Wayland is every hide of the window (to the tray): Wayland can't hide a window, so it is destroyed.

   Why that is a bug: a `Text`'s `preferred-width` depends on its text only through its cache entry (the entry's tracker is what tells the binding that the text changed). Dropping the entries drops the trackers without telling the bindings, so every width computed before the hide is never computed again when the text changes later. The status chip kept the width of "Playing 2:34" for "Connecting…" (cut to "Connecting.") and for an error (cut to "Can't play…"), and was far too wide for "Stopped".

   The entries hold shaped text, no graphics resources, so nothing needs them gone; a new scale factor still clears them (`clear_if_scale_factor_changed`), and a component that is destroyed takes its own with it (`free_graphics_resources`).

   Not fixed upstream as of Slint 1.19.1 and `master` on 2026-11-25 (the Skia renderer has the same call in its `suspend`). When Slint is updated, check `clear_graphics_context` before dropping this copy. `cargo test -p radiotrope-app --bin radiotrope labels` fails without the change: it hides and shows a window drawn by FemtoVG and looks at the chips (`crates/radiotrope-app/src/labels_femtovg_test.rs`).
