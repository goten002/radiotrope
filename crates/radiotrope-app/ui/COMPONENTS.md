# Shared Radiotrope UI

Import `Theme` and `Defaults` from `defaults.slint`. Components are exported from
`components.slint`, `buttons.slint`, `styled-line-edit.slint`, and `scroll-bar.slint`.
Use Title Case for button labels and dialog titles, and sentence case for field
labels, status text, and help. Use “…” only for ongoing activity.

## Theme

| Token | Use for |
| --- | --- |
| `surface`, `panel`, `card`, `elevated` | List, app chrome, dialog, and inset surfaces, respectively. |
| `hairline`, `scrim` | Separators and modal backdrops. |
| `field-border` | The edge of a text field or dropdown: 3:1 against the card. |
| `shadow`, `shadow-soft` | Modal/elevated and small surface shadows, respectively. |
| `popup-surface`, `popup-border`, `popup-shadow`, `popup-shadow-blur`, `popup-shadow-offset` | The appearance of an anchored menu, tooltip, or stats bubble. Prefer `PopupCard`. |
| `text-primary`, `text-secondary`, `text-tertiary`, `menu-text` | Main text, supporting text, low-emphasis text, and idle menu labels. |
| `accent-default`, `accent-swatches`, `accent-swatch-names`, `accent-base` | Default color, preset colors and spoken names, and the saved user color. |
| `accent-fill`, `accent-fill-hover`, `accent-fill-pressed` | Normal, hovered, and pressed fills of filled controls (Play, primary buttons, a picked option): the color as picked, moved only where it would vanish into the layers. |
| `on-accent`, `on-accent-hover`, `on-accent-pressed` | Text and icons over the matching accent fill. Use matching foreground and fill bindings for each state. |
| `accent-graphic`, `on-accent-graphic` | Accent marks that carry meaning (stars, checks, playing bars and the row rail, spinners, the radio in a logo tile, slider and progress fills, switches, the visualizer, active edges), and what is drawn on them. The accent itself where it reaches 3:1, else darkened (light theme) or lightened (dark theme) only as far as it takes. |
| `accent-text` | Links and accent text on neutral layers or accent tints, and an icon that is part of that text. 4.5:1. Underline hovered or focused links. |
| `accent-tint`, `accent-tint-strong` | Background of the current or selected row and of accent chips, and the same while hovered or highlighted. |
| `accent-glow` | Decorative top and side glows only. |
| `accent`, `accent-hover`, `accent-pressed`, `accent-soft`, `selected-bg`, `selection-mark` | Earlier names of `accent-fill` (and its states), `accent-tint` (twice) and `accent-graphic`. Don't use them in new code. |
| `logo-tile` | Behind a station logo or the radio in its place. Use `StationLogo`. |
| `danger`, `success`, `warning` | Decorative status colors. |
| `danger-text` / `danger-tint`, `success-text` / `success-tint`, `warning-text` / `warning-tint`, `neutral-text` / `neutral-tint` | Status foreground and background pairs on app or dialog surfaces. |
| `danger-armed`, `danger-pressed` | Tints for two-press confirmation and press feedback. |
| `hover-bg`, `pressed-bg` | Interaction surfaces for rows and secondary controls. |
| `disabled-bg`, `disabled-border`, `disabled-text` | Unavailable controls: a fill and edge quieter than an enabled control's. Use secondary text for read-only content. |
| `focus-ring`, `focus-ring-width`, `focus-ring-offset` | The contrast-safe focus indicator and its geometry. |
| `selection-bg`, `selection-fg` | Selected text in an editor. |
| `viz-bg`, `viz-track` | The visualizer well and track. |
| `font-2xs`, `font-xs`, `font-sm`, `font-md` | Scales (10px), captions (11px), details (12px), and field labels (12px). 10px is the smallest text. |
| `font-base`, `font-lg`, `font-xl` | Body text, section headings, and stats totals. |
| `font-title`, `font-station`, `font-hero`, `font-timer`, `font-code` | Dialog titles, station names, the app name, timer readouts, and pairing codes. |
| `weight-regular`, `weight-semibold` | Body text and emphasized text, including all form labels. |
| `space-2`, `space-4`, `space-6`, `space-8`, `space-12`, `space-16`, `space-20`, `space-24` | Shared spacing, in pixels. |
| `icon-sm`, `icon-compact`, `icon-md`, `icon-lg` | Inline/close (12px), compact (14px), standard (16px), and playback (20px) glyphs. |
| `control-button`, `control-field`, `control-touch` | Regular button (34px), text field (36px), and touch action (44px) heights. |
| `row-menu`, `row-dense`, `row-media`, `row-touch` | Menu (28px), dense list (48px), station list (56px), and touch list (64px) row heights. |
| `radius-sm`, `radius-md`, `radius-lg`, `dialog-radius` | Control, popup, and large corners, plus the dialog shape set by the rounded-corner preference. Derive pills and circles from their height. |
| `dialog-padding`, `dialog-gap`, `viewport-inset` | Dialog content padding, section gaps, and the space reserved at the viewport edge. |
| `fast`, `normal`, `bars-period`, `bars-period-step`, `pulse-period`, `spin-period`, `tune-period` | Control transitions, playback bars, status pulse, spinner, and visualizer tuning time. |
| `theme-mode`, `system-dark`, `dark-mode`, `panel-gradient`, `rounded-corners` | Theme preferences and the resolved palette. |
| `text-on(fill)`, `status-text(tone)` | Accessible text over an opaque custom fill or a status tint, respectively. For status roles prefer the named foreground and tint. |

Keep raw status colors for decoration and use the matching `*-text` token
for status text. The table at the top of `defaults.slint` says which accent
token each meaning takes. `tests/theme_contrast.rs` checks the text tokens
(4.5:1) and `accent-graphic`, `field-border` and the focus ring (3:1) in both
themes, for every accent preset and for white and near-black accents; run it
after changing a color.

## Defaults

| Token | Use for |
| --- | --- |
| `window-width`, `window-height`, `window-min-width`, `window-min-height` | Initial desktop size and resize constraints. |
| `resize-border`, `window-radius` | Custom window chrome and grab area. |
| `dialog-width-compact`, `dialog-width`, `dialog-width-form`, `dialog-width-wide` | Shared dialog widths of 360, 440, 460, and 540px. |
| `about-width`, `agents-width`, `accent-color-width`, `equalizer-width`, `recording-width`, `remote-control-width`, `schedule-width`, `sleep-timer-width`, `station-browser-width` | Dialogs with specific content-width needs. |
| `import-favorites-width`, `keyboard-shortcuts-width`, `keyboard-shortcuts-height` | Import and Keyboard Shortcuts measurements. |
| `filter-picker-height`, `station-browser-height` | Fixed card heights for scrollable result views. |
| `info-tip-width`, `dropdown-max-height`, `dropdown-min-popup-width` | Help bubble width cap and dropdown viewport dimensions. |
| `accent-picker-height`, `pairing-qr-size` | Color picker and QR display dimensions. |
| `remote-devices-chrome`, `schedule-empty-height` | Available space calculations for dialog lists. |
| `sleep-readout-width`, `sleep-step-minutes`, `sleep-big-step-minutes`, `sleep-max-minutes` | Sleep Timer readout and duration controls. |
| `volume-width`, `volume-track`, `volume-knob`, `volume-knob-active`, `volume-grab` | Volume slider geometry. |
| `hover-delay`, `hold-menu-delay`, `hold-drag-delay`, `copied-time`, `value-hint-time`, `repeat-click-guard`, `motion-step`, `drag-scroll-step` | Tooltip delay, gestures, feedback, repeat protection, and animation steps. |

Existing dialog-specific width aliases (`filter-picker-width`,
`pairing-code-width`, `edit-favorite-width`, `network-stream-width`,
`statistics-width`) remain usable. Use the shared width classes for new
dialogs.

## Components

| Component | Use for |
| --- | --- |
| `DialogShell` | Modal card with a scrollable body and fixed header and footer. Set `card-height` for a viewport-driven dialog. Page Up / Page Down scroll the body; while the shell itself has the focus (a read-only dialog), the arrows, Home and End do too. |
| `DialogFooter` | Destructive action on the left, then Cancel and primary on the right. Set `close-only` for information or result dialogs, or `full-width` for touch actions. |
| `StatusPanel` | Empty, loading, error, and result states with an icon, short title, optional explanation, and one optional secondary action. The `danger` and `success` variants pick their own icon; the title is announced when it changes. |
| `PopupCard` | A bordered, shadowed surface for menus and bubbles. Fades in when it appears. |
| `InfoTip` | Short help text shown via keyboard, hover, or tap and confined to the dialog body. |
| `PopupBounds` | Window-space limits for an anchored popup or help bubble: the dialog body, plus the window's top and bottom for dropdown lists. |
| `Dropdown` | A single selection with a checked choice, an anchored scrollable popup, and keyboard navigation. The list opens downward, may cover the dialog footer, shows whole rows, and opens upward only when fewer than four rows fit below. |
| `ChoiceRow` | A menu item or stacked choice with an optional icon, trailing text, and check slot. |
| `MenuSeparator` | The line between related groups of menu rows. |
| `Chip`, `ChipVariant` | Compact neutral, accent, danger, success, or warning status. Set `clickable` (adds a thin edge) or `closable` for interactive status. |
| `AccentButton`, `SecondaryButton`, `DangerButton` | Primary, supporting, and destructive actions. |
| `IconButton`, `Icon` | A named icon-only action or a decorative glyph. |
| `ButtonKeys`, `FocusRing` | Space/Enter handlers and visible keyboard focus for custom controls. |
| `Field`, `StyledLineEdit` | A label and single-line editor with an optional Clear button. The field shows an accent edge while it has the caret and the full focus ring when reached with Tab. |
| `Toggle`, `Picker`, `PickerOption`, `MultiPickerOption`, `PickerChoice` | An on/off setting or a small set of segmented choices. `MultiPickerOption` is for options where several can be on (read as checkboxes). |
| `ScrollBar` | Vertical thumb, track paging, and drag handling for an existing Flickable: a quiet thumb at rest, the track once hovered. Handle keyboard scrolling in the viewport. |
| `KeyCap`, `KeyCaps` | A keyboard key drawn as a raised cap (`keycap.slint`), and a shortcut's key combinations: "+" between keys pressed together, an optional `separator` such as "or" between alternatives. |
| `SectionHeader`, `InfoRow`, `LinkRow`, `CreditLink` | Section labels, label/value pairs, and accessible links. A `selectable` InfoRow is a named, read-only Tab stop with a focus ring. |
| `Motion`, `NowPlayingBars` | The app's animation clock and a playing indicator. |
| `AgentsChip`, `AgentsBubble`, `ListenersBubble` | Connection count and details on the main window. The chip is a button: pass `label` ("2 agents using the player") and `description` (the names); its list opens on hover and on keyboard focus. |

## Migration

- Move existing dialog actions out of the body and bind `cancel-label`,
  `primary-label`, and `destructive-label` on the shell. Wire the corresponding
  `*-requested` callbacks. Cancel defaults to `close-requested`. Use
  `close-footer: true` for an informational footer and `full-width-actions` for
  touch dialogs. Give forms explicit initial focus through `focus-input()`.
- Use a `StatusPanel` title with short, sentence-case status copy. Set `busy`
  for loading and `variant: ChipVariant.danger` for an error. Bind
  `action-label: "Try Again"` and `action => { … }` where retry is supported.
  Use the same SecondaryButton treatment for an inline retry.
- For dropdowns set `label` to the field name, bind `items` and `selected`,
  and handle `changed(index)` for changes from the user. Press Enter or Space
  to open, use the arrows to move, Enter to choose, and Esc to return to the
  field. Bind `popup-bounds` directly to the enclosing shell’s `popup-bounds`
  when dialogs can overlap. Outside a shell, provide window-space bounds with
  `dialog-id: 0`. Set `popup-min-width` if translations need additional room.
- For choice lists use `selected` for the committed value. When a surrounding
  scope handles navigation, bind `highlighted` for the keyboard target and set
  `tab-stop: false`. Set `checkable: false` for menu actions with no check slot.
- Bind `armed` on DangerButton or `danger-armed` on IconButton/ChoiceRow
  during the second step of a destructive action. Supply `armed-label` on a
  DangerButton or `destructive-armed-label` on the footer to keep its width
  steady and use the same visible and spoken “Press Again to Remove” label.
- Use `ChipVariant.neutral` for Ready and Stopped, `ChipVariant.accent` for
  connecting and playing, and `ChipVariant.danger` for errors.
- Set `clearable: true` for optional form fields and search queries. Clear
  appears only while the enabled field has a value, returns focus to the
  field, and emits `cleared()`. Leave it off for required form fields.
  Share dismissal and initial-focus behavior at the dialog level.
- Name every control for screen readers. Pass `label` to `StyledLineEdit`
  (the same text as its Field label; without one the placeholder is used)
  and to `Picker` (the setting's name). Use `MultiPickerOption` where several
  options can be on at once.
- Mark fields by text, not color: set `optional: true` on the `Field` of a
  field that may stay empty (it shows "Optional" after the label), and
  `required: true` on the `StyledLineEdit` of one that must be filled (it is
  said after the name). All field labels have one color.
- Use `Theme.accent-graphic` for an accent mark or fill that carries meaning
  and sits on a plain layer; keep `Theme.accent` for button fills and
  decoration, and `Theme.accent-text` for text and icons.
- Pointer targets are at least 24px. Where the drawing is smaller (`InfoTip`,
  `Chip`, `AgentsChip`), enlarge the `TouchArea`, not the drawing.

## Icons

One icon per meaning. Outline icons everywhere; solid glyphs only for play,
stop, the record dot and the filled star.

| Meaning | Icon |
| --- | --- |
| Error | `circle-alert.svg` |
| Success | `circle-check.svg` |
| Edit | `edit.svg` |
| Delete or remove | `trash.svg` |
| Reset | `reset.svg` |
| Agent | `bot.svg` |
| Calendar | `calendar.svg` |
| Dropdown arrow | `chevron-down.svg` (turned while open) |

`StatusPanel` picks the error and success icons from its `variant`. Don't use
the older twins (`circle-exclamation`, `pencil`, `trash-can`, `rotate-left`,
`robot`, `calendar-days`); they are being removed.
