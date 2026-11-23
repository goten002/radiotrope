# Colour review: one accent role per meaning

Feedback 6. The owner's screenshots (125%, default orange accent) showed the
same thing drawn in different colours: the radio that stands in for a missing
logo was bright orange in the header and dark brown in the favorites list.
This review lists every such case, in the dark and the light theme, and the
role system in `ui/defaults.slint` that removes them.

Screens were drawn with the snapshot tool at scale 2 and at 1.25 (the owner's
zoom), before and after, in both themes. Ratios are WCAG 2 contrast ratios;
text needs 4.5:1 and graphics need 3:1.

## What was inconsistent

Colours are for the default orange `#f7931e`. "Light" and "dark" are the themes.

| # | Where | Before | Why it is wrong |
| --- | --- | --- | --- |
| 1 | Radio in a logo tile, header | `accent` raw: `#f7931e` in both themes | 2.0:1 on the light tile, under 3:1. A different colour from the same radio in a row. |
| 2 | Radio in a logo tile, favorites and Search rows | `selection-mark` (= `accent-text`): light `#7c490f`, dark `#faba6f` | A text colour used for a graphic. Dark brown in light, a paler orange in dark: neither matches the header. |
| 3 | Logo tile shape | Header: its own rectangle, 6px corners, shadow. Rows: `StationLogo`, 4px corners. | Two implementations of one thing. The radio was 39% of the tile in the header and 45% in a row. |
| 4 | Star (header, Search) | `selection-mark`: light `#7c490f`, dark `#faba6f` | Text colour on a graphic. In light it read as brown beside an orange slider. |
| 5 | Row rail, spinner, checkmarks, the drag outline | `selection-mark` | Same as 4. The rail was dark brown while the bars beside the name were orange. |
| 6 | Playing bars, volume slider, switches, EQ sliders | `accent-graphic`: light `#c97718`, dark `#f7931e` | The right token, but 2.8:1 on the current row's tint in light: it was only checked on plain layers. |
| 7 | Visualizer (accent palette) | `accent` raw | 2.0:1 on its light tile. Invisible with a white accent in light and a black one in dark. |
| 8 | Status chip background | 14% of `accent-text` | A brown wash in light (`#ebdbca`) beside the orange wash of the current row (`#f3e6db`): two tints for one meaning. |
| 9 | Agents chip background | 14% / 22% of `accent-text` | Same as 8. |
| 10 | Current row, highlighted | `accent-base` at 24% / 18%, written out in the row | A literal instead of a token. |
| 11 | Large status icon (`StatusPanel`), empty-favorites star, Scheduler play icon, Wi-Fi icon, agent and phone icons | `accent-text` | Text colour on stand-alone graphics. |
| 12 | Wi-Fi password field edge | `accent-text` | Text colour on an edge. |
| 13 | Wi-Fi connected row | `accent-soft` with its alpha replaced by 8% | A third tint strength. |
| 14 | `accent-text` in dark | `#faba6f` | Paler than it needs to be: it was checked against a wash of itself. |
| 15 | White accent in light, near-black accent in dark | Play button, header radio and visualizer the colour of the panel | The control is there but can't be seen. |

Not accent, checked and left alone: the status colours (danger, success,
warning and their text and tint pairs), the neutral text tiers, the focus
ring and the field border. They already have one token per meaning and pass
in `tests/theme_contrast.rs`.

## The roles

One meaning, one token, the same rule in both themes. The table is also at
the top of `ui/defaults.slint`.

| Token | Meaning | Rule |
| --- | --- | --- |
| `surface` | The list area behind the rows | |
| `panel` | Header and bottom bar | |
| `card` | Dialogs, menus and bubbles | |
| `elevated` | Wells inside a layer: inputs, tracks, inset boxes | |
| `logo-tile` | Behind a station logo or the radio in its place | A well (= `elevated`), like the visualizer tile at the other end of the header |
| `hairline` | 1px separators, and the edge of every tile | Every logo tile has one, so a logo with a white or black background keeps its outline |
| `accent-fill` (`-hover`, `-pressed`) | Filled controls: Play, primary buttons, a picked option | The colour as picked. Moved only if it would vanish into the layers (under 1.5:1) |
| `on-accent` (`-hover`, `-pressed`) | Text and icons on those fills | Black or white, whichever reads better; 4.5:1 |
| `accent-graphic` | Marks that carry meaning: stars, checks, playing bars, the row rail, spinners, the radio in a logo tile, slider and progress fills, switches, the visualizer, active edges | The accent where it has 3:1, else moved toward white (dark) or black (light) just far enough |
| `on-accent-graphic` | What is drawn on an `accent-graphic` fill (a switch's knob) | Black or white |
| `accent-text` | Accent-coloured text, and an icon that is part of that text (a chip's dot, "Copied") | The same, to 4.5:1 |
| `accent-tint` | Background of the current or selected row and of accent chips | 16% (dark) or 12% (light) of `accent-graphic` |
| `accent-tint-strong` | The same, hovered or highlighted | 24% / 18% |
| `accent-glow` | Decoration only: the glow on panels and dialogs | |

`accent-graphic` and `accent-text` hold their ratio on every layer they are
drawn on: the four layers, the logo tile, the visualizer tile, hovered and
pressed rows, and both tints. The tints are a wash of `accent-graphic` rather
than of the raw accent, so they still show for a white or a black accent.

One rule decides between the two: a mark that stands alone is a graphic; an
icon inside a label takes the label's colour.

### Values

| Accent | Theme | `accent-fill` | `on-accent` | `accent-graphic` | `accent-text` |
| --- | --- | --- | --- | --- | --- |
| Orange | dark | `#f7931e` | `#000000` | `#f7931e` | `#f8a441` |
| Orange | light | `#f7931e` | `#000000` | `#b26a16` | `#7f4c0f` |
| Purple | dark | `#9141ac` | `#ffffff` | `#ae73c2` | `#c69dd4` |
| Purple | light | `#9141ac` | `#ffffff` | `#9141ac` | `#7a3791` |
| White | dark | `#ffffff` | `#000000` | `#ffffff` | `#ffffff` |
| White | light | `#c3c3c3` | `#000000` | `#7c7c7c` | `#585858` |
| Near-black | dark | `#434343` | `#ffffff` | `#888888` | `#ababab` |
| Near-black | light | `#0a0a0a` | `#ffffff` | `#0a0a0a` | `#0a0a0a` |

In the dark theme the default orange is one colour for fills and graphics,
and its text is a shade lighter. In the light theme it is three steps of the
same hue: the fill as picked, graphics darker, text darker again. That is the
least the contrast rules allow: orange on white is 2.3:1.

## What changed

- **One `StationLogo`** (`ui/station-row.slint`) for the header, favorites
  rows and Search rows: `logo-tile` background, hairline edge, 6px corners,
  the radio in `accent-graphic` at 44% of the tile. Only the size differs
  (40px in a row, 76px in the header), and the header's has its soft shadow.
- Findings 1 to 13 moved to the role in the table. Finding 14: `accent-text`
  is now checked against the tint it is really on. Finding 15: `accent-fill`
  and the tints stay visible for any colour.
- `tests/theme_contrast.rs` works the roles out as `Theme` does and checks
  them in both themes for the ten presets and for white, near-black, black,
  the grey of each theme's own layers, yellow and navy.

## Left for the owner or a later package

- **Tone of light-theme graphics.** With the default orange the star, bars,
  slider and switches are now `#b26a16` in the light theme (the slider and
  switches were `#c97718`, which fell to 2.8:1 on the current row). This is
  a brown-orange next to the bright orange Play button. It is what 3:1 costs;
  judge it by eye.
- **Achromatic accents.** With a white accent in light, or a near-black one
  in dark, the current station's name is a mid grey: readable (4.5:1) but
  quieter than its neighbours. The rail, the tint and the bars still mark it.
- **Files outside this package** keep the earlier token names, which now
  point at the roles above, so they draw correctly: `accent`, `accent-hover`,
  `accent-pressed` (`buttons.slint`, `virtual-keyboard.slint`, `menu-bar.slint`,
  `app.slint`), `selected-bg` and `selection-mark` (`filter-dropdown.slint`,
  `menu-bar.slint`, `scroll-bar.slint`, `virtual-keyboard.slint`). Renaming
  them is mechanical. One real difference remains: the focused edge of a text
  field (`styled-line-edit.slint`) uses `accent-text` where an edge should use
  `accent-graphic`, so in the light theme it is a darker brown than the active
  edge of a dropdown.
- **Visualizer with logo colours.** Colours sampled from a station's logo are
  content and are drawn as sampled; only the accent palette follows the roles.
