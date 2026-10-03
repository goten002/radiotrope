//! The screen: station and playback on top, the favorites in the middle,
//! Stop/Play, the song and volume at the bottom
//!
//! Uses only characters Consolas and Cascadia Mono have, so it looks the
//! same in the Windows console as in Linux terminals.

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use radiotrope::stream::StreamType;

use crate::library::Station;
use crate::player::{Phase, Player};
use crate::visual::{Spectrum, COLUMNS};

/// The app's default accent (Theme.accent-default)
pub const DEFAULT_ACCENT: (u8, u8, u8) = (0xf7, 0x93, 0x1e);

/// Below this width the visualizer and the format line make way
const WIDE: u16 = 60;
/// Below this width the volume bar shows as a number only
const VOLUME_BAR_MIN_WIDTH: u16 = 50;
/// Cells in the volume bar
const VOLUME_CELLS: usize = 8;
/// Width of the station-name column in the list
const NAME_COLUMN: usize = 30;
/// Narrowest country column worth showing
const MIN_COUNTRY: usize = 10;

/// How many colours the terminal shows
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    /// 24-bit colour
    True,
    /// The xterm 256-colour palette
    Indexed,
    /// The 16 basic colours
    Basic,
    /// No colour (`NO_COLOR`): bold and reverse only
    None,
}

impl ColorMode {
    /// What the terminal says it supports
    pub fn detect() -> Self {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return ColorMode::None;
        }
        let colorterm = var("COLORTERM").to_lowercase();
        if colorterm.contains("truecolor") || colorterm.contains("24bit") {
            return ColorMode::True;
        }
        // Windows Terminal, and the Windows 10+ console, take 24-bit colour
        if cfg!(windows) || std::env::var_os("WT_SESSION").is_some() {
            return ColorMode::True;
        }
        let term = var("TERM");
        if term.contains("256color") {
            ColorMode::Indexed
        } else if term == "dumb" {
            ColorMode::None
        } else {
            ColorMode::Basic
        }
    }
}

/// The colours of the screen, from the accent
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub mode: ColorMode,
    pub accent: Color,
    pub error: Color,
    pub dim: Color,
}

impl Palette {
    pub fn new(mode: ColorMode, accent: (u8, u8, u8)) -> Self {
        Self {
            mode,
            accent: rgb(mode, accent, Color::Yellow),
            error: rgb(mode, (0xff, 0x5a, 0x5a), Color::LightRed),
            dim: if mode == ColorMode::None {
                Color::Reset
            } else {
                Color::DarkGray
            },
        }
    }

    /// A colour between the accent and its light tint (`t` 0-1)
    fn accent_at(&self, accent: (u8, u8, u8), t: f32) -> Color {
        match self.mode {
            ColorMode::True | ColorMode::Indexed => rgb(
                self.mode,
                mix(accent, (255, 255, 255), 0.45 * t),
                Color::Yellow,
            ),
            _ => self.accent,
        }
    }

    fn fg(&self, color: Color) -> Style {
        if self.mode == ColorMode::None {
            Style::default()
        } else {
            Style::default().fg(color)
        }
    }
}

fn mix(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// `color` as the terminal can show it
fn rgb(mode: ColorMode, (r, g, b): (u8, u8, u8), basic: Color) -> Color {
    match mode {
        ColorMode::True => Color::Rgb(r, g, b),
        ColorMode::Indexed => Color::Indexed(xterm_index(r, g, b)),
        ColorMode::Basic => basic,
        ColorMode::None => Color::Reset,
    }
}

/// The nearest colour of the xterm 6x6x6 cube or grey ramp
fn xterm_index(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |v: u8| {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, &l)| (l as i32 - v as i32).abs())
            .map(|(i, _)| i as u8)
            .unwrap_or(0)
    };
    let (ri, gi, bi) = (level(r), level(g), level(b));
    let cube = 16 + 36 * ri + 6 * gi + bi;
    let cube_rgb = (
        LEVELS[ri as usize],
        LEVELS[gi as usize],
        LEVELS[bi as usize],
    );
    let avg = (r as u32 + g as u32 + b as u32) / 3;
    let grey_i = ((avg.saturating_sub(8)) / 10).min(23) as u8;
    let grey_v = 8 + 10 * grey_i as u32;
    let dist = |(x, y, z): (u32, u32, u32)| {
        let d = |a: u32, b: u8| (a as i32 - b as i32).pow(2);
        d(x, r) + d(y, g) + d(z, b)
    };
    let cube_d = dist((cube_rgb.0 as u32, cube_rgb.1 as u32, cube_rgb.2 as u32));
    let grey_d = dist((grey_v, grey_v, grey_v));
    if grey_d < cube_d {
        232 + grey_i
    } else {
        cube
    }
}

/// What the screen shows besides the player
pub struct View {
    pub favorites: Vec<Station>,
    pub list: ListState,
    pub palette: Palette,
    pub accent: (u8, u8, u8),
    pub spectrum: Spectrum,
}

impl View {
    pub fn new(favorites: Vec<Station>, palette: Palette, accent: (u8, u8, u8)) -> Self {
        let mut list = ListState::default();
        if !favorites.is_empty() {
            list.select(Some(0));
        }
        Self {
            favorites,
            list,
            palette,
            accent,
            spectrum: Spectrum::new(),
        }
    }

    pub fn cursor(&self) -> Option<usize> {
        self.list.selected().filter(|&i| i < self.favorites.len())
    }

    /// Move the cursor by `delta` rows, stopping at the ends
    pub fn move_cursor(&mut self, delta: isize) {
        if self.favorites.is_empty() {
            return;
        }
        let last = self.favorites.len() as isize - 1;
        let now = self.cursor().unwrap_or(0) as isize;
        self.list
            .select(Some((now + delta).clamp(0, last) as usize));
    }

    pub fn select(&mut self, index: usize) {
        if index < self.favorites.len() {
            self.list.select(Some(index));
        }
    }

    pub fn index_of(&self, url: &str) -> Option<usize> {
        self.favorites.iter().position(|f| f.url == url)
    }

    /// New favorites from the app; the cursor stays on its station
    pub fn replace_favorites(&mut self, favorites: Vec<Station>) {
        let at = self.cursor().map(|i| self.favorites[i].url.clone());
        self.favorites = favorites;
        let index = at
            .and_then(|url| self.index_of(&url))
            .or_else(|| {
                self.list
                    .selected()
                    .map(|i| i.min(self.favorites.len().saturating_sub(1)))
            })
            .filter(|_| !self.favorites.is_empty());
        self.list.select(index);
    }
}

/// Draw everything
pub fn draw(frame: &mut Frame, view: &mut View, player: &Player) {
    let area = frame.area();
    let p = view.palette;
    if area.width < 24 || area.height < 6 {
        let text = Paragraph::new("Radiotrope: make the window bigger");
        frame.render_widget(text, area);
        return;
    }

    // The key help under the box, when there is room
    let (box_area, help_area) = if area.height >= 12 {
        let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
        (rows[0], Some(rows[1]))
    } else {
        (area, None)
    };

    let block = Block::default()
        .title(Span::styled(" Radiotrope ", p.fg(p.dim)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(p.fg(p.dim));
    let inner = block.inner(box_area);
    frame.render_widget(block, box_area);

    let wide = inner.width >= WIDE;
    let header_rows: u16 = if wide { 3 } else { 2 };
    // Header, line, list (at least one row), line, bottom
    let with_list = inner.height > header_rows + 3;
    let rows = if with_list {
        Layout::vertical([
            Constraint::Length(header_rows),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner)
    } else {
        Layout::vertical([
            Constraint::Length(header_rows),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner)
    };

    draw_header(frame, view, player, rows[0], wide);
    if with_list {
        separator(frame.buffer_mut(), box_area, rows[1].y, p);
        draw_list(frame, view, player, rows[2]);
        separator(frame.buffer_mut(), box_area, rows[3].y, p);
        draw_bottom(frame, view, player, rows[4]);
    } else {
        draw_bottom(frame, view, player, rows[2]);
    }
    if let Some(help) = help_area {
        draw_help(frame, view, help);
    }
}

/// A line across the box at row `y`, joined to its sides
fn separator(buf: &mut Buffer, box_area: Rect, y: u16, p: Palette) {
    let style = p.fg(p.dim);
    let right = box_area.right() - 1;
    for x in box_area.left()..=right {
        let symbol = if x == box_area.left() {
            "├"
        } else if x == right {
            "┤"
        } else {
            "─"
        };
        buf[(x, y)].set_symbol(symbol).set_style(style);
    }
}

fn draw_header(frame: &mut Frame, view: &View, player: &Player, area: Rect, wide: bool) {
    let p = view.palette;
    let viz_width = if wide { COLUMNS as u16 + 2 } else { 0 };
    let text_area = Rect {
        width: area.width.saturating_sub(viz_width),
        ..area
    };
    let width = text_area.width.saturating_sub(1) as usize;

    let name = match &player.station {
        Some(s) if !s.name.is_empty() => s.name.clone(),
        Some(s) => host(&s.url).to_string(),
        None => "Pick a station".to_string(),
    };
    let mut lines = vec![Line::from(vec![
        Span::raw(" "),
        Span::styled(fit(&name, width), Style::default().bold()),
    ])];
    if wide {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(fit(&format_line(player), width), p.fg(p.dim)),
        ]));
    }
    lines.push(status_line(player, p, width));
    frame.render_widget(Paragraph::new(lines), text_area);

    if wide {
        let viz = Rect {
            x: area.right().saturating_sub(COLUMNS as u16 + 1),
            y: area.y,
            width: COLUMNS as u16,
            height: area.height,
        };
        draw_spectrum(frame.buffer_mut(), view, viz);
    }
}

/// `MP3 · 128 kbps · 44.1 kHz · Stereo`, or what is known of it
fn format_line(player: &Player) -> String {
    let hls = player.stream_type == Some(StreamType::Hls);
    let mut parts = Vec::new();
    match &player.codec {
        Some(codec) if hls => parts.push(format!("{} (HLS)", codec.codec_name)),
        Some(codec) => parts.push(codec.codec_name.clone()),
        None if hls => parts.push("HLS".to_string()),
        None => {}
    }
    if let Some(kbps) = player.bitrate.filter(|k| *k > 0) {
        parts.push(format!("{kbps} kbps"));
    }
    if let Some(codec) = &player.codec {
        if codec.sample_rate > 0 {
            let khz = codec.sample_rate as f32 / 1000.0;
            parts.push(if khz.fract() == 0.0 {
                format!("{khz:.0} kHz")
            } else {
                format!("{khz:.1} kHz")
            });
        }
        parts.push(match codec.channels {
            1 => "Mono".to_string(),
            2 => "Stereo".to_string(),
            n => format!("{n} channels"),
        });
    }
    parts.join(" · ")
}

fn status_line(player: &Player, p: Palette, width: usize) -> Line<'static> {
    let color = if player.is_error {
        p.error
    } else if player.phase == Phase::Stopped {
        p.dim
    } else {
        p.accent
    };
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    let mut tail = String::new();
    if let Some(started) = player.started_at {
        tail.push_str("  ");
        tail.push_str(&clock(started.elapsed()));
    }
    let rec = player
        .recording
        .as_ref()
        .map(|r| format!("● REC {}", clock(r.duration)));
    let rec_width = rec.as_ref().map_or(0, |r| r.width() + 4);
    let room = width.saturating_sub(used + tail.width() + rec_width);
    let status = fit(&format!("● {}", player.status), room);
    used += status.width();
    spans.push(Span::styled(status, p.fg(color)));
    if !tail.is_empty() {
        used += tail.width();
        spans.push(Span::styled(tail, p.fg(p.dim)));
    }
    if let Some(rec) = rec {
        if used + rec_width <= width {
            spans.push(Span::raw("    "));
            spans.push(Span::styled(rec, p.fg(p.error).bold()));
        }
    }
    Line::from(spans)
}

/// The spectrum, as tall as the header, in eighth blocks, from the accent at the bass
/// to its light tint at the treble
fn draw_spectrum(buf: &mut Buffer, view: &View, area: Rect) {
    const EIGHTHS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let p = view.palette;
    let levels = view.spectrum.levels();
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    for (i, &level) in levels.iter().enumerate().take(area.width as usize) {
        let t = i as f32 / (COLUMNS - 1).max(1) as f32;
        let style = p.fg(p.accent_at(view.accent, t));
        let eighths = (level.clamp(0.0, 1.0) * (rows * 8) as f32).round() as usize;
        for row in 0..rows {
            // Row 0 is the top
            let from_bottom = rows - 1 - row;
            let fill = eighths.saturating_sub(from_bottom * 8).min(8);
            let x = area.x + i as u16;
            let y = area.y + row as u16;
            buf[(x, y)].set_symbol(EIGHTHS[fill]).set_style(style);
        }
    }
}

fn draw_list(frame: &mut Frame, view: &mut View, player: &Player, area: Rect) {
    let p = view.palette;
    if view.favorites.is_empty() {
        let width = area.width.saturating_sub(1) as usize;
        let lines = vec![
            Line::from(""),
            Line::from(format!(" {}", fit("No favorites yet.", width))),
            Line::from(Span::styled(
                format!(" {}", fit("Add stations in the Radiotrope app,", width)),
                p.fg(p.dim),
            )),
            Line::from(Span::styled(
                format!(" {}", fit("or play one: radiotrope-cli <URL>", width)),
                p.fg(p.dim),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }

    let width = area.width as usize;
    let number_width = view.favorites.len().to_string().len().max(2);
    // Rail, number, two spaces
    let fixed = 1 + number_width + 3;
    let mut name_width = NAME_COLUMN.min(width.saturating_sub(fixed + 1)).max(1);
    let mut country_width = width.saturating_sub(fixed + name_width + 1);
    // Too narrow for a readable country: the name takes the row
    if country_width < MIN_COUNTRY {
        name_width = width.saturating_sub(fixed + 1).max(1);
        country_width = 0;
    }
    let cursor = view.cursor();
    let playing_url = player
        .station
        .as_ref()
        .filter(|_| player.is_active())
        .map(|s| s.url.as_str());

    let items: Vec<ListItem> = view
        .favorites
        .iter()
        .enumerate()
        .map(|(i, station)| {
            let selected = cursor == Some(i);
            let playing = playing_url == Some(station.url.as_str());
            let row = if selected && p.mode != ColorMode::None {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::default()
            };
            let dim = if selected {
                row
            } else {
                row.patch(p.fg(p.dim))
            };
            let name_style = if playing {
                row.patch(p.fg(p.accent)).bold()
            } else if selected && p.mode == ColorMode::None {
                row.reversed()
            } else {
                row
            };
            let name = pad(&fit(&station.name, name_width), name_width);
            let mut spans = vec![
                Span::styled(if playing { "▌" } else { " " }, p.fg(p.accent)),
                Span::styled(format!(" {:>number_width$}  ", i + 1), dim),
                Span::styled(name, name_style),
            ];
            if country_width > 1 {
                let country = station.country.as_deref().unwrap_or("");
                spans.push(Span::styled(
                    pad(
                        &format!(" {}", fit(country, country_width - 1)),
                        country_width + 1,
                    ),
                    dim,
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    frame.render_stateful_widget(List::new(items), area, &mut view.list);
}

fn draw_bottom(frame: &mut Frame, view: &View, player: &Player, area: Rect) {
    let p = view.palette;
    let width = area.width as usize;

    let volume = if player.muted {
        "Muted".to_string()
    } else {
        format!("{:>3}%", (player.volume * 100.0).round() as u32)
    };
    let bar_cells = if area.width >= VOLUME_BAR_MIN_WIDTH {
        VOLUME_CELLS
    } else {
        0
    };
    let right_width = volume.width() + if bar_cells > 0 { bar_cells + 1 } else { 0 } + 1;

    let button = if player.is_active() { " ■ " } else { " ► " };
    let button_style = if p.mode == ColorMode::None {
        Style::default().reversed()
    } else {
        Style::default().bg(p.accent).fg(Color::Black).bold()
    };
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(button, button_style),
        Span::raw(" "),
    ];
    let used = 5;
    let room = width.saturating_sub(used + right_width + 2);

    let middle = if let Some(notice) = &player.notice {
        let color = if notice.is_error { p.error } else { p.accent };
        Span::styled(fit(&notice.text, room + 2), p.fg(color))
    } else {
        let song = song(player);
        if song.is_empty() {
            Span::raw("")
        } else {
            spans.push(Span::styled("♪ ", p.fg(p.accent)));
            Span::raw(fit(&song, room))
        }
    };
    spans.push(middle);
    let left = Line::from(spans);
    frame.render_widget(Paragraph::new(left), area);

    let mut right = Vec::new();
    if bar_cells > 0 {
        let filled = if player.muted {
            0
        } else {
            (player.volume * bar_cells as f32).round() as usize
        };
        right.push(Span::styled("█".repeat(filled), p.fg(p.accent)));
        right.push(Span::styled("░".repeat(bar_cells - filled), p.fg(p.dim)));
        right.push(Span::raw(" "));
    }
    right.push(Span::raw(volume));
    let right_area = Rect {
        x: area.right().saturating_sub(right_width as u16),
        width: (right_width as u16).min(area.width),
        ..area
    };
    frame.render_widget(Paragraph::new(Line::from(right)), right_area);
}

/// `Artist - Title`, or whichever of the two the station sends
fn song(player: &Player) -> String {
    match (player.artist.is_empty(), player.title.is_empty()) {
        (false, false) => format!("{} - {}", player.artist, player.title),
        (true, false) => player.title.clone(),
        (false, true) => player.artist.clone(),
        (true, true) => String::new(),
    }
}

fn draw_help(frame: &mut Frame, view: &View, area: Rect) {
    let p = view.palette;
    let keys: &[(&str, &str)] = &[
        ("↑↓", "pick"),
        ("Enter", "play"),
        ("Space", "stop"),
        ("r", "record"),
        ("+-", "volume"),
        ("m", "mute"),
        ("q", "quit"),
    ];
    let mut spans = Vec::new();
    let mut used = 0;
    for (key, what) in keys {
        let w = key.width() + what.width() + 3;
        if used + w > area.width as usize {
            break;
        }
        used += w;
        spans.push(Span::styled(format!(" {key}"), p.fg(p.accent).bold()));
        spans.push(Span::styled(format!(" {what} "), p.fg(p.dim)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// `m:ss`, or `h:mm:ss` from an hour
pub fn clock(time: Duration) -> String {
    let secs = time.as_secs();
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The host of `url`, for a station without a name
fn host(url: &str) -> &str {
    url.split("//")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .filter(|h| !h.is_empty())
        .unwrap_or(url)
}

/// Cut `text` to `width` terminal columns, ending in "…" when cut
pub fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// `text` padded with spaces to `width` columns
fn pad(text: &str, width: usize) -> String {
    let w = text.width();
    if w >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - w))
    }
}

/// How often to draw: smoothly while the spectrum moves, rarely otherwise
pub fn frame_interval(view: &View, player: &Player) -> Duration {
    if player.phase == Phase::Playing || !view.spectrum.is_still() {
        Duration::from_millis(33)
    } else {
        Duration::from_millis(250)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_cut_to_terminal_columns() {
        assert_eq!(fit("Capital FM", 20), "Capital FM");
        assert_eq!(fit("Capital FM London", 10), "Capital F…");
        // Wide characters take two columns
        assert_eq!(fit("日本語ラジオ", 5), "日本…");
        assert_eq!(fit("ραδιόφωνο", 4), "ραδ…");
        assert_eq!(fit("abc", 0), "");
    }

    #[test]
    fn clock_shows_hours_only_when_needed() {
        assert_eq!(clock(Duration::from_secs(5)), "0:05");
        assert_eq!(clock(Duration::from_secs(761)), "12:41");
        assert_eq!(clock(Duration::from_secs(3723)), "1:02:03");
    }

    #[test]
    fn xterm_colours_land_on_the_nearest() {
        assert_eq!(xterm_index(0, 0, 0), 16);
        assert_eq!(xterm_index(255, 255, 255), 231);
        assert_eq!(xterm_index(0xf7, 0x93, 0x1e), 208);
        assert_eq!(xterm_index(128, 128, 128), 244);
    }

    #[test]
    fn host_names_a_station_without_a_name() {
        assert_eq!(
            host("http://stream.example.com:8000/live"),
            "stream.example.com:8000"
        );
        assert_eq!(host("not a url"), "not a url");
    }
}
