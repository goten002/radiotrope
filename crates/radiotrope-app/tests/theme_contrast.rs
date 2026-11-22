//! Contrast of the theme's colours (WCAG 2), for both themes and every
//! accent preset.
//!
//! The colour values are read from `ui/defaults.slint`, so a changed grey or
//! a new preset is checked without touching this file. The computed tokens
//! (`accent-text`, `accent-graphic`, `on-accent`) are worked out here the
//! same way `Theme` does it; keep the two in step when changing that maths.

use std::collections::HashMap;

const DEFAULTS: &str = include_str!("../ui/defaults.slint");

/// Text needs 4.5:1, graphics and control edges 3:1.
const TEXT: f32 = 4.5;
const GRAPHIC: f32 = 3.0;

type Rgb = [u8; 3];

fn hex(text: &str) -> Option<(Rgb, f32)> {
    let digits = text.strip_prefix('#')?;
    let channel = |at: usize| u8::from_str_radix(digits.get(at..at + 2)?, 16).ok();
    let alpha = match digits.len() {
        6 => 1.0,
        8 => f32::from(channel(6)?) / 255.0,
        _ => return None,
    };
    Some(([channel(0)?, channel(2)?, channel(4)?], alpha))
}

fn linear(channel: u8) -> f32 {
    let v = f32::from(channel) / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn luminance(c: Rgb) -> f32 {
    0.2126 * linear(c[0]) + 0.7152 * linear(c[1]) + 0.0722 * linear(c[2])
}

fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn quantize(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Slint's `a.mix(b, share)` for opaque colours: `share` of `a`.
fn mix(a: Rgb, b: Rgb, share: f32) -> Rgb {
    std::array::from_fn(|i| {
        quantize(f32::from(a[i]) / 255.0 * share + f32::from(b[i]) / 255.0 * (1.0 - share))
    })
}

/// `fg` at `alpha` drawn over `bg`.
fn over(fg: Rgb, alpha: f32, bg: Rgb) -> Rgb {
    mix(fg, bg, alpha)
}

/// Slint's `c.darker(factor)`: the HSV value divided by `1 + factor`, which
/// scales every channel alike.
fn darker(c: Rgb, factor: f32) -> Rgb {
    c.map(|channel| quantize(f32::from(channel) / 255.0 / (1.0 + factor)))
}

const BLACK: Rgb = [0, 0, 0];
const WHITE: Rgb = [255, 255, 255];

fn text_on(fill: Rgb) -> Rgb {
    if contrast(BLACK, fill) >= contrast(WHITE, fill) {
        BLACK
    } else {
        WHITE
    }
}

/// `Theme.readable` and `Theme.visible`: `c` if it passes, else `c` moved
/// toward `to` just far enough, by halving the step six times.
fn moved_until(c: Rgb, to: Rgb, passes: impl Fn(Rgb) -> bool) -> Rgb {
    if passes(c) {
        return c;
    }
    let mut lo = 0.0;
    for step in [0.5, 0.25, 0.125, 0.0625, 0.03125, 0.015625] {
        if !passes(mix(c, to, 1.0 - lo - step)) {
            lo += step;
        }
    }
    mix(c, to, 1.0 - lo - 0.015625)
}

/// The plain colour tokens of one theme, by name.
struct Palette {
    dark: bool,
    colors: HashMap<String, (Rgb, f32)>,
}

impl Palette {
    /// Reads every `out property <color> name: value;` whose value is a
    /// colour, another token, or `dark-mode ? one : other` of those.
    fn read(dark: bool) -> Self {
        let mut colors: HashMap<String, (Rgb, f32)> = HashMap::new();
        for line in DEFAULTS.lines() {
            let Some(rest) = line.trim().strip_prefix("out property <color> ") else {
                continue;
            };
            let Some((name, value)) = rest.split_once(':') else {
                continue;
            };
            let value = value.trim().trim_end_matches(';').trim();
            let chosen = match value.strip_prefix("dark-mode ?") {
                Some(both) => {
                    let Some((when_dark, when_light)) = both.split_once(" : ") else {
                        continue;
                    };
                    if dark { when_dark } else { when_light }.trim()
                }
                None => value,
            };
            let color = hex(chosen).or_else(|| colors.get(chosen).copied());
            if let Some(color) = color {
                colors.insert(name.trim().to_owned(), color);
            }
        }
        Self { dark, colors }
    }

    fn get(&self, name: &str) -> Rgb {
        let (color, alpha) = self
            .colors
            .get(name)
            .unwrap_or_else(|| panic!("no colour token `{name}` in defaults.slint"));
        assert!(*alpha == 1.0, "`{name}` is translucent; use `tint`");
        *color
    }

    fn name(&self) -> &'static str {
        if self.dark {
            "dark"
        } else {
            "light"
        }
    }

    /// `Theme.accent-text` for an accent.
    fn accent_text(&self, accent: Rgb) -> Rgb {
        let pressed = self.get("pressed-bg");
        let chip_base = self.get(if self.dark { "card" } else { "surface" });
        let further = |a: Rgb, b: Rgb| {
            if self.dark {
                luminance(a) >= luminance(b)
            } else {
                luminance(a) <= luminance(b)
            }
        };
        let reads_well = |c: Rgb| {
            let accent_limit = mix(accent, pressed, 0.26);
            let chip_limit = mix(c, chip_base, 0.26);
            let composite = if further(accent_limit, chip_limit) {
                accent_limit
            } else {
                chip_limit
            };
            let limit = if further(composite, pressed) {
                composite
            } else {
                pressed
            };
            contrast(c, limit) >= 4.5
                && if self.dark {
                    luminance(c) > luminance(limit)
                } else {
                    luminance(c) < luminance(limit)
                }
        };
        if self.dark {
            moved_until(accent, WHITE, reads_well)
        } else {
            moved_until(darker(accent, 0.5), BLACK, reads_well)
        }
    }

    /// `Theme.accent-graphic` for an accent.
    fn accent_graphic(&self, accent: Rgb) -> Rgb {
        let limit = self.get(if self.dark { "card" } else { "elevated" });
        let to = if self.dark { WHITE } else { BLACK };
        moved_until(accent, to, |c| contrast(c, limit) >= 3.0)
    }
}

/// The accent presets and their names, from `accent-swatches`.
fn presets() -> Vec<(String, Rgb)> {
    let list = |property: &str| {
        let start = DEFAULTS
            .find(property)
            .unwrap_or_else(|| panic!("no `{property}` in defaults.slint"));
        let body = &DEFAULTS[start..];
        let open = body.find('[').expect("list start") + 1;
        let close = open + body[open..].find(']').expect("list end");
        body[open..close]
            .split(',')
            .map(|item| item.trim().trim_matches('"').to_owned())
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>()
    };
    let colors = list("accent-swatches");
    let names = list("accent-swatch-names");
    assert_eq!(colors.len(), names.len(), "a name for every preset");
    assert!(colors.len() >= 10, "all presets were read");
    names
        .into_iter()
        .zip(colors)
        .map(|(name, color)| {
            let (color, _) = hex(&color).unwrap_or_else(|| panic!("preset `{color}`"));
            (name, color)
        })
        .collect()
}

/// Collects every failed pair, so one run lists them all.
#[derive(Default)]
struct Report(Vec<String>);

impl Report {
    fn check(&mut self, what: &str, fg: Rgb, bg: Rgb, needed: f32) {
        let ratio = contrast(fg, bg);
        if ratio < needed {
            self.0.push(format!(
                "{what}: #{:02x}{:02x}{:02x} on #{:02x}{:02x}{:02x} is {ratio:.2}:1, needs {needed}:1",
                fg[0], fg[1], fg[2], bg[0], bg[1], bg[2]
            ));
        }
    }

    fn finish(self) {
        assert!(self.0.is_empty(), "low contrast:\n{}", self.0.join("\n"));
    }
}

const LAYERS: [&str; 4] = ["surface", "panel", "card", "elevated"];

#[test]
fn text_tokens_reach_4_5_on_every_plain_layer() {
    let mut report = Report::default();
    for dark in [true, false] {
        let theme = Palette::read(dark);
        for text in [
            "text-primary",
            "text-secondary",
            "text-tertiary",
            "menu-text",
        ] {
            for layer in LAYERS {
                report.check(
                    &format!("{} {text} on {layer}", theme.name()),
                    theme.get(text),
                    theme.get(layer),
                    TEXT,
                );
            }
        }
    }
    report.finish();
}

#[test]
fn status_text_reaches_4_5_on_its_own_tints() {
    let mut report = Report::default();
    for dark in [true, false] {
        let theme = Palette::read(dark);
        for status in ["danger", "success", "warning"] {
            let text = theme.get(&format!("{status}-text"));
            for layer in LAYERS {
                // Resting, armed and pressed tints
                for alpha in [0.14, 0.22, 0.30] {
                    let tint = over(theme.get(status), alpha, theme.get(layer));
                    report.check(
                        &format!(
                            "{} {status}-text on its {alpha} tint over {layer}",
                            theme.name()
                        ),
                        text,
                        tint,
                        TEXT,
                    );
                }
            }
        }
        for layer in ["panel", "card"] {
            let tint = over(theme.get("text-secondary"), 0.14, theme.get(layer));
            report.check(
                &format!("{} neutral-text on its tint over {layer}", theme.name()),
                theme.get("neutral-text"),
                tint,
                TEXT,
            );
        }
    }
    report.finish();
}

#[test]
fn accent_text_reaches_4_5_for_every_preset() {
    let mut report = Report::default();
    for dark in [true, false] {
        let theme = Palette::read(dark);
        // `accent-soft`: the tint of a selected or current row
        let soft = if dark { 0.16 } else { 0.12 };
        for (name, accent) in presets() {
            let text = theme.accent_text(accent);
            let label = format!("{} {name} accent-text", theme.name());
            for layer in LAYERS {
                report.check(&format!("{label} on {layer}"), text, theme.get(layer), TEXT);
                report.check(
                    &format!("{label} on accent-soft over {layer}"),
                    text,
                    over(accent, soft, theme.get(layer)),
                    TEXT,
                );
            }
            // Text and icons on the accent fills of a button
            let hover = darker(accent, 0.15);
            for (state, fill) in [
                ("accent", accent),
                ("accent-hover", hover),
                ("accent-pressed", darker(hover, 0.1)),
            ] {
                report.check(
                    &format!("{} {name} on-accent on {state}", theme.name()),
                    text_on(fill),
                    fill,
                    TEXT,
                );
            }
        }
    }
    report.finish();
}

#[test]
fn graphics_and_edges_reach_3_for_every_preset() {
    let mut report = Report::default();
    for dark in [true, false] {
        let theme = Palette::read(dark);
        for (name, accent) in presets() {
            let graphic = theme.accent_graphic(accent);
            for layer in LAYERS {
                report.check(
                    &format!("{} {name} accent-graphic on {layer}", theme.name()),
                    graphic,
                    theme.get(layer),
                    GRAPHIC,
                );
            }
            // The knob of a switch that is on
            report.check(
                &format!(
                    "{} {name} on-accent-graphic on accent-graphic",
                    theme.name()
                ),
                text_on(graphic),
                graphic,
                GRAPHIC,
            );
        }
        // The muted speaker in the bottom bar, at rest and hovered
        for icon in ["danger", "danger-text"] {
            report.check(
                &format!("{} {icon} (muted speaker) on panel", theme.name()),
                theme.get(icon),
                theme.get("panel"),
                GRAPHIC,
            );
        }
        report.check(
            &format!("{} field-border on card", theme.name()),
            theme.get("field-border"),
            theme.get("card"),
            GRAPHIC,
        );
        // A switch that is off: its outline and knob
        report.check(
            &format!("{} text-tertiary (switch off) on card", theme.name()),
            theme.get("text-tertiary"),
            theme.get("card"),
            GRAPHIC,
        );
        for layer in LAYERS {
            report.check(
                &format!("{} focus-ring on {layer}", theme.name()),
                theme.get("focus-ring"),
                theme.get(layer),
                GRAPHIC,
            );
        }
    }
    report.finish();
}

#[test]
fn the_default_orange_is_darkened_only_in_the_light_theme() {
    let orange = presets()[0].1;
    assert_eq!(Palette::read(true).accent_graphic(orange), orange);
    let light = Palette::read(false);
    let graphic = light.accent_graphic(orange);
    assert_ne!(graphic, orange);
    // Only as far as it takes: it lands just past 3:1
    assert!(contrast(graphic, light.get("elevated")) < 3.3);
}
