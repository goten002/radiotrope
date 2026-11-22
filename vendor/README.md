# vendor

Patched copies of dependencies, used through `[patch.crates-io]` in the root `Cargo.toml`. Each keeps its upstream license.

| Directory | Upstream | Change | Remove when |
|---|---|---|---|
| `i-slint-renderer-femtovg` | [i-slint-renderer-femtovg 1.19.0](https://crates.io/crates/i-slint-renderer-femtovg/1.19.0), as published on crates.io without `Cargo.lock` and `Cargo.toml.orig` (GPL-3.0-only OR Slint Royalty-free 2.0 OR Slint Software 3.0, see its `LICENSES/`) | The fix from [slint-ui/slint#15003](https://github.com/slint-ui/slint/pull/15003) in `itemrenderer.rs` (`draw_glyph_run`): glyph runs are drawn on whole device pixels, so parts of letters are no longer drawn one pixel off ([slint-ui/slint#15000](https://github.com/slint-ui/slint/issues/15000)) | Slint >= 1.19.1 or 1.20 is used: delete the directory and the `[patch.crates-io]` entry, then run `cargo update -p i-slint-renderer-femtovg` |
