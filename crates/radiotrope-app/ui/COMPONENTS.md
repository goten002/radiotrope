# Shared Radiotrope UI

Import `Theme` and `Defaults` from `defaults.slint`. Components are exported from
`components.slint`, `buttons.slint`, `styled-line-edit.slint`, and `scroll-bar.slint`.
Use Title Case for button labels and dialog titles, and sentence case for field
labels, status text, and help. Use “…” only for ongoing activity.

## Theme

| Token | Use for |
| --- | --- |
| `surface`, `panel`, `card`, `elevated` | List, app chrome, dialog, and inset surfaces, respectively. |
| `hairline`, `field-border`, `scrim` | Separators, input outlines, and modal backdrops. |
| `shadow`, `shadow-soft` | Modal/elevated and small surface shadows, respectively. |
| `popup-surface`, `popup-border`, `popup-shadow`, `popup-shadow-blur`, `popup-shadow-offset` | The appearance of an anchored menu, tooltip, or stats bubble. Prefer `PopupCard`. |
| `text-primary`, `text-secondary`, `text-tertiary`, `menu-text` | Main text, supporting text, low-emphasis text, and idle menu labels. |
| `accent-default`, `accent-swatches`, `accent-swatch-names`, `accent-base` | Default color, preset colors and spoken names, and the saved user color. |
| `accent`, `accent-hover`, `accent-pressed` | Normal, hovered, and pressed accent fills. Use raw accent for decoration. |
| `on-accent`, `on-accent-hover`, `on-accent-pressed` | Text and icons over the matching accent fill. Use matching foreground and fill bindings for each state. |
| `accent-text` | Links and accent text on neutral layers or accent tints, including on hover. Underline hovered or focused links. |
| `selection-mark` | Checkmarks and other selection indicators on neutral layers. |
| `accent-soft`, `accent-glow` | Accent-tinted state fills and decorative top glows. |
| `danger`, `success`, `warning`, `info` | Decorative status colors. |
| `danger-text` / `danger-tint`, `success-text` / `success-tint`, `warning-text` / `warning-tint`, `neutral-text` / `neutral-tint` | Status foreground and background pairs on app or dialog surfaces. |
| `danger-armed`, `danger-pressed`, `danger-hover` | Tints for two-press confirmation and press feedback, plus the hover color for a solid danger fill. |
| `hover-bg`, `pressed-bg`, `selected-bg` | Interaction surfaces for rows and secondary controls. |
| `disabled-bg`, `disabled-text` | Unavailable controls. Use secondary text for read-only content. |
| `focus-ring`, `focus-ring-width`, `focus-ring-offset` | The contrast-safe focus indicator and its geometry. |
| `selection-bg`, `selection-fg` | Selected text in an editor. |
| `viz-bg`, `viz-track` | The visualizer well and track. |
| `font-2xs`, `font-xs`, `font-sm`, `font-md` | Scales, captions, details, and field labels. |
| `font-base`, `font-lg`, `font-xl`, `font-2xl` | Body text, section headings, stats totals, and keyboard keys. |
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

Deprecated compatibility names include `play-icon` (use the matching
`on-accent*` token), `popup-bg` (use `popup-surface`), and `menu-hover` /
`menu-bar-hover` (use `pressed-bg` / `hover-bg`). Keep raw status colors for
decoration and migrate status text to the matching `*-text` token.

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
| `remote-devices-chrome`, `schedule-empty-height`, `schedule-list-chrome` | Available space calculations for dialog lists. |
| `sleep-readout-width`, `sleep-step-minutes`, `sleep-big-step-minutes`, `sleep-max-minutes` | Sleep Timer readout and duration controls. |
| `volume-width`, `volume-track`, `volume-knob`, `volume-knob-active`, `volume-grab` | Volume slider geometry. |
| `hover-delay`, `hold-menu-delay`, `hold-drag-delay`, `copied-time`, `value-hint-time`, `repeat-click-guard`, `motion-step`, `drag-scroll-step` | Tooltip delay, gestures, feedback, repeat protection, and animation steps. |

Existing dialog-specific width aliases (`filter-picker-width`,
`pairing-code-width`, `edit-favorite-width`, `network-stream-width`,
`statistics-width`) remain usable. Use the shared width classes for new
dialogs. Use `Theme.row-menu` and `Theme.row-dense` in place of deprecated
`Defaults.dropdown-row-height` and `Defaults.remote-device-height`.

## Components

| Component | Use for |
| --- | --- |
| `DialogShell` | Modal card with a scrollable body and fixed header and footer. Set `card-height` for a viewport-driven dialog. |
| `DialogFooter` | Destructive action on the left, then Cancel and primary on the right. Set `close-only` for information or result dialogs, or `full-width` for touch actions. |
| `StatusPanel` | Empty, loading, and error states with an icon, short title, optional explanation, and one optional secondary action. |
| `PopupCard` | A bordered, shadowed surface for menus and bubbles. |
| `InfoTip` | Short help text shown via keyboard, hover, or tap and confined to the dialog body. |
| `PopupBounds` | Window-space limits for an anchored popup or help bubble. |
| `Dropdown` | A single selection with a checked choice, an anchored scrollable popup, and keyboard navigation. |
| `ChoiceRow` | A menu item or stacked choice with an optional icon, trailing text, and check slot. |
| `MenuSeparator` | The line between related groups of menu rows. |
| `Chip`, `ChipVariant` | Compact neutral, accent, danger, success, or warning status. Set `clickable` or `closable` for interactive status. |
| `AccentButton`, `SecondaryButton`, `DangerButton` | Primary, supporting, and destructive actions. |
| `IconButton`, `Icon` | A named icon-only action or a decorative glyph. |
| `ButtonKeys`, `FocusRing` | Space/Enter handlers and visible keyboard focus for custom controls. |
| `Field`, `StyledLineEdit` | A label and single-line editor with a visible focus ring and optional Clear button. |
| `Toggle`, `Picker`, `PickerOption`, `PickerChoice` | An on/off setting or a small set of segmented choices. |
| `ScrollBar` | Vertical thumb, track paging, and drag handling for an existing Flickable. Handle keyboard scrolling in the viewport. |
| `SectionHeader`, `InfoRow`, `LinkRow`, `CreditLink` | Section labels, label/value pairs, and accessible links. |
| `Motion`, `NowPlayingBars` | The app's animation clock and a playing indicator. |
| `AgentsChip`, `AgentsBubble`, `ListenersBubble` | Connection count and details on the main window. |

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
  connecting and playing, and `ChipVariant.danger` for errors. The legacy
  `Chip.tone` override remains available for existing screens.
- Set `clearable: true` for optional form fields and search queries. Clear
  appears only while the enabled field has a value, returns focus to the
  field, and emits `cleared()`. Leave it off for required form fields.
  Share dismissal and initial-focus behavior at the dialog level.
