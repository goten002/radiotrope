//! The screen: station and playback on top, the favorites in the middle,
//! Stop/Play, the song and volume at the bottom
//!
//! Uses only characters Consolas and Cascadia Mono have, so it looks the
//! same in the Windows console as in Linux terminals.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use radiotrope::stream::StreamType;
use radiotrope_library::data::favorites::FavoriteEdit;
use radiotrope_library::text::{self, clock};

use crate::favorites::{self, FavoritesWorker, Reply};
use crate::input::{self, Confirmation, Effect};
use crate::library::Station;
use crate::player::{Phase, Player, NOTICE_TIME};
use crate::search::Search;
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

/// The colours of the screen, from the accent. The accent is only for
/// fills and marks (the button, the volume bar, the spectrum, `▌`, `★`,
/// `●`): as text it is hard to read on a light background. Text is in the
/// terminal's own colours, with the dim attribute for secondary text (a
/// fixed grey vanishes in some themes, Solarized Dark among them).
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub mode: ColorMode,
    pub accent: Color,
    pub error: Color,
}

impl Palette {
    pub fn new(mode: ColorMode, accent: (u8, u8, u8)) -> Self {
        Self {
            mode,
            accent: rgb(mode, accent, Color::Yellow),
            error: rgb(mode, (0xff, 0x5a, 0x5a), Color::LightRed),
        }
    }

    /// Secondary text: numbers, countries, hints, lines
    fn secondary(&self) -> Style {
        Style::default().add_modifier(Modifier::DIM)
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
    pub search: Search,
    pub confirmation: Option<Confirmation>,
    pub saving_favorites: bool,
    /// Why the last edit failed, shown until the next key or a few seconds
    pub favorites_error: Option<String>,
    edit_error_at: Option<Instant>,
    /// Why the favorites file can't be read, shown while that lasts
    follow_error: Option<String>,
    /// The help screen, with how far it is scrolled
    pub help: Option<u16>,
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
            search: Search::default(),
            confirmation: None,
            saving_favorites: false,
            favorites_error: None,
            edit_error_at: None,
            follow_error: None,
            help: None,
        }
    }

    /// The favorites file couldn't be read at start
    pub fn favorites_unreadable(&mut self, error: String) {
        self.follow_error = Some(error);
    }

    /// Clear an edit's error (a key was pressed)
    pub fn dismiss_edit_error(&mut self) {
        if !self.saving_favorites {
            self.favorites_error = None;
        }
    }

    /// Clear an edit's error that has been showing long enough
    fn expire_edit_error(&mut self) {
        if self
            .edit_error_at
            .is_some_and(|at| at.elapsed() > NOTICE_TIME)
        {
            self.favorites_error = None;
            self.edit_error_at = None;
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
                    .or(Some(0))
                    .map(|i| i.min(self.favorites.len().saturating_sub(1)))
            })
            .filter(|_| !self.favorites.is_empty());
        self.list.select(index);
    }

    pub fn highlighted(&self) -> Option<&Station> {
        if self.search.open {
            self.search.selected()
        } else {
            self.cursor().map(|i| &self.favorites[i])
        }
    }

    pub fn begin_edit(&mut self, edit: FavoriteEdit) -> Effect {
        self.favorites_error = None;
        self.saving_favorites = true;
        Effect::Edit(edit)
    }

    pub fn poll_favorites(&mut self, worker: &FavoritesWorker) {
        use crossbeam_channel::TryRecvError;
        loop {
            match worker.replies.try_recv() {
                Ok(reply) => self.apply_favorites(reply),
                Err(TryRecvError::Disconnected) if self.saving_favorites => {
                    self.apply_favorites(Reply::Edited(Err("Favorites worker unavailable".into())));
                    break;
                }
                Err(_) => break,
            }
        }
    }

    pub fn apply_favorites(&mut self, reply: Reply) {
        match reply {
            Reply::Edited(result) => {
                self.saving_favorites = false;
                match result {
                    Ok(favorites) => {
                        self.favorites_error = None;
                        self.follow_error = None;
                        self.replace_favorites(favorites);
                    }
                    Err(error) => {
                        log::warn!("Can't change favorites: {error}");
                        self.favorites_error = Some(error);
                        self.edit_error_at = Some(Instant::now());
                    }
                }
            }
            Reply::Followed(result) => match result {
                Ok(favorites) => {
                    // The file is readable again: an edit's error is old news
                    self.follow_error = None;
                    self.favorites_error = None;
                    self.replace_favorites(favorites);
                }
                Err(error) => self.follow_error = Some(error),
            },
        }
    }
}

/// Draw everything
pub fn draw(frame: &mut Frame, view: &mut View, player: &Player) {
    let area = frame.area();
    let p = view.palette;
    view.expire_edit_error();
    if area.width < 24 || area.height < 6 {
        let text = Paragraph::new("Window too small").wrap(Wrap { trim: true });
        frame.render_widget(text, area);
        return;
    }

    // The key help under the box, when there is room
    let (box_area, help_area) = if area.height >= 12 || (view.search.open && area.height >= 10) {
        let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
        (rows[0], Some(rows[1]))
    } else {
        (area, None)
    };

    let block = Block::default()
        .title(Span::styled(" Radiotrope ", p.secondary()))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(p.secondary());
    let inner = block.inner(box_area);
    frame.render_widget(block, box_area);

    let wide = inner.width >= WIDE;
    let header_rows: u16 = if wide { 3 } else { 2 };
    // Header, line, list (at least one row), line, bottom
    let body_rows = if view.search.open { 3 } else { 1 };
    let with_list = inner.height >= header_rows + 3 + body_rows;
    let compact_body = !with_list && inner.height > header_rows + 1;
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
        draw_body(frame, view, player, rows[2], help_area.is_some());
        separator(frame.buffer_mut(), box_area, rows[3].y, p);
        draw_bottom(frame, view, player, rows[4]);
    } else {
        if compact_body {
            draw_body(frame, view, player, rows[1], help_area.is_some());
        }
        draw_bottom(frame, view, player, rows[2]);
    }
    if let Some(help) = help_area {
        draw_help(frame, view, player, help);
    }
    if let Some(scroll) = view.help {
        draw_help_screen(frame, view, area, scroll);
    }
}

/// What to say about the favorites under the list, if anything: the
/// text and whether it is an error
fn favorites_notice(view: &View) -> Option<(String, bool)> {
    if let Some(error) = &view.favorites_error {
        return Some((edit_error(error), true));
    }
    // With no favorites shown the empty list explains it
    if view.follow_error.is_some() && !view.favorites.is_empty() {
        return Some((
            "Can't read your favorites. Showing the last ones read.".into(),
            true,
        ));
    }
    view.saving_favorites
        .then(|| ("Saving favorites\u{2026}".into(), false))
}

/// The favorites file can't be read because its contents are damaged (not
/// because of the disk), which the app can repair
fn damaged(error: &str) -> bool {
    error.starts_with("Invalid favorite")
}

/// An edit's error for the screen: the parser's words mean nothing to a user
fn edit_error(error: &str) -> String {
    if damaged(error) {
        "Can't change favorites: the file is damaged. Open the Radiotrope app to restore it.".into()
    } else {
        format!("Can't change favorites: {}", lower_first(error))
    }
}

fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        // Keep "HTTP …" and the like
        (Some(a), Some(b)) if a.is_uppercase() && !b.is_uppercase() => a
            .to_lowercase()
            .chain(text[a.len_utf8()..].chars())
            .collect(),
        _ => text.to_string(),
    }
}

fn draw_body(frame: &mut Frame, view: &mut View, player: &Player, area: Rect, help_line: bool) {
    let p = view.palette;
    if let Some(confirmation) = &view.confirmation {
        let question = format!("Remove {} from favorites?", confirmation.name);
        let width = area.width.saturating_sub(1) as usize;
        let mut lines = vec![Line::from(vec![
            Span::raw(" "),
            Span::styled(fit(&question, width), Style::default().bold()),
        ])];
        // The keys are in the help line, or here when there is none
        if !help_line {
            let keys = "Enter/y remove  Esc/n cancel";
            if area.height >= 2 {
                lines.push(Line::styled(
                    format!(" {}", fit(keys, width)),
                    p.secondary(),
                ));
            } else {
                lines = vec![Line::raw(format!(
                    " {}",
                    fit(&format!("y/n \u{b7} {question}"), width)
                ))];
            }
        }
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }
    let mut body = area;
    let notice = favorites_notice(view);
    if view.search.open {
        // In search the notice takes the status row, so results keep theirs
        let playing = player.station.as_ref().filter(|_| player.is_active());
        draw_search(frame, view, playing, notice, body);
        return;
    }
    if let Some((notice, is_error)) = notice {
        body.height -= 1;
        let row = Rect::new(area.x, body.bottom(), area.width, 1);
        let style = if is_error {
            p.fg(p.error)
        } else {
            p.secondary()
        };
        frame.render_widget(
            Paragraph::new(format!(
                " {}",
                fit(&notice, area.width.saturating_sub(1) as usize)
            ))
            .style(style),
            row,
        );
        if body.height == 0 {
            return;
        }
    }
    draw_list(frame, view, player, body);
}

fn draw_search(
    frame: &mut Frame,
    view: &mut View,
    playing: Option<&Station>,
    notice: Option<(String, bool)>,
    area: Rect,
) {
    let p = view.palette;
    let search = &mut view.search;
    // One column of margin, as everywhere else in the box
    let field = Rect::new(area.x + 1, area.y, area.width.saturating_sub(1), 1);
    let label = "Find: ";
    let text_width = (field.width as usize).saturating_sub(label.len());
    let mut start = search.caret;
    let mut caret_column = 0;
    for (index, grapheme) in search.query[..search.caret].grapheme_indices(true).rev() {
        let next = caret_column + grapheme.width();
        if next >= text_width {
            break;
        }
        start = index;
        caret_column = next;
    }
    if !search.editing {
        start = 0;
    }
    let field_text = format!("{label}{}", fit(&search.query[start..], text_width));
    let style = if search.editing {
        Style::default().bold()
    } else {
        p.secondary()
    };
    frame.render_widget(Paragraph::new(field_text).style(style), field);
    if search.editing {
        frame.set_cursor_position((field.x + (label.len() + caret_column) as u16, field.y));
    }
    if area.height < 2 {
        return;
    }
    let status_row = Rect::new(field.x, area.bottom() - 1, field.width, 1);
    let (status, status_style) = match notice {
        Some((text, true)) => (text, p.fg(p.error)),
        Some((text, false)) => (text, p.secondary()),
        None if search.error.is_some() => (search.status(), p.fg(p.error)),
        None => (search.status(), p.secondary()),
    };
    frame.render_widget(
        Paragraph::new(fit(&status, status_row.width as usize)).style(status_style),
        status_row,
    );
    let results = Rect::new(area.x, area.y + 1, area.width, area.height - 2);
    let cursor = search.list.selected().filter(|_| !search.editing);
    let width = (area.width as usize).saturating_sub(4);
    let items: Vec<_> = search
        .rows
        .iter()
        .enumerate()
        .map(|(i, station)| {
            let playing = playing.is_some_and(|current| current.url == station.url);
            let saved = favorites::find_match(&view.favorites, station).is_some();
            let country = station.country.as_deref().unwrap_or("");
            // The country takes what the name leaves, and a third at least
            let name_room = station.name.width().min(width);
            let country_width = country
                .width()
                .min(width.saturating_sub(name_room + 1).max(width / 3));
            let name_width = width.saturating_sub(country_width + 1);
            let name = pad(&fit(&station.name, name_width), name_width);
            let row = row_style(cursor == Some(i));
            ListItem::new(Line::from(vec![
                Span::styled(if playing { "\u{258c}" } else { " " }, p.fg(p.accent)),
                Span::styled(
                    if saved { "\u{2605} " } else { "  " },
                    row.patch(p.fg(p.accent)),
                ),
                Span::styled(name, if playing { row.bold() } else { row }),
                Span::styled(" ", row),
                Span::styled(
                    pad(&fit(country, country_width), country_width),
                    secondary_in(row, p),
                ),
            ]))
        })
        .collect();
    frame.render_stateful_widget(List::new(items), results, &mut search.list);
}

/// A list row: reverse video when selected, the same in both lists and in
/// any theme
fn row_style(selected: bool) -> Style {
    if selected {
        Style::default().reversed()
    } else {
        Style::default()
    }
}

/// Secondary text in a row: dim, except in the selected row, where the
/// reverse video already sets it apart
fn secondary_in(row: Style, p: Palette) -> Style {
    if row == Style::default() {
        p.secondary()
    } else {
        row
    }
}

/// A line across the box at row `y`, joined to its sides
fn separator(buf: &mut Buffer, box_area: Rect, y: u16, p: Palette) {
    let style = p.secondary();
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
            Span::styled(fit(&format_line(player), width), p.secondary()),
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
            parts.push(text::sample_rate(codec.sample_rate));
        }
        parts.push(match codec.channels {
            1 => "Mono".to_string(),
            2 => "Stereo".to_string(),
            n => format!("{n} channels"),
        });
    }
    parts.join(text::SEPARATOR)
}

/// `● Playing  1:23`: the dot is grey when stopped, accent while
/// connecting or playing, red on an error, like the app's status chip
fn status_line(player: &Player, p: Palette, width: usize) -> Line<'static> {
    let (dot, words) = if player.is_error {
        (p.fg(p.error), p.fg(p.error))
    } else if player.phase == Phase::Stopped {
        (p.secondary(), p.secondary())
    } else {
        (p.fg(p.accent), Style::default())
    };
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    let mut tail = String::new();
    if let Some(started) = player.started_at.filter(|_| player.shows_clock()) {
        tail.push_str("  ");
        tail.push_str(&clock(started.elapsed().as_secs()));
    }
    let rec = player
        .recording
        .as_ref()
        .map(|r| format!("\u{25cf} REC {}", clock(r.duration.as_secs())));
    let rec_width = rec.as_ref().map_or(0, |r| r.width() + 4);
    let room = width.saturating_sub(used + tail.width() + rec_width);
    if room >= 3 {
        let status = fit(&player.status, room - 2);
        used += status.width() + 2;
        spans.push(Span::styled("\u{25cf} ", dot));
        spans.push(Span::styled(status, words));
    }
    if !tail.is_empty() {
        used += tail.width();
        spans.push(Span::styled(tail, p.secondary()));
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
        let line = |text: &str, style: Style| {
            Line::from(Span::styled(format!(" {}", fit(text, width)), style))
        };
        let lines = match &view.follow_error {
            // The list is empty because the file can't be read: say so,
            // not "No favorites yet"
            Some(error) => vec![
                Line::from(""),
                line("Can't read your favorites.", p.fg(p.error).bold()),
                if damaged(error) {
                    line(
                        "The favorites file is damaged. Open the Radiotrope app to restore it.",
                        Style::default(),
                    )
                } else {
                    line(error, Style::default())
                },
            ],
            None => vec![
                Line::from(""),
                line("No favorites yet.", Style::default()),
                line("Press / to find a station", p.secondary()),
                line("or play one: radiotrope-cli <URL>", p.secondary()),
            ],
        };
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
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
            let playing = playing_url == Some(station.url.as_str());
            let row = row_style(cursor == Some(i));
            let dim = secondary_in(row, p);
            // The playing station is marked by the rail; its name is bold
            // in the terminal's own colour
            let name_style = if playing { row.bold() } else { row };
            let name = pad(&fit(&station.name, name_width), name_width);
            let mut spans = vec![
                Span::styled(if playing { "\u{258c}" } else { " " }, p.fg(p.accent)),
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
    // One column of space before the volume
    let room = width.saturating_sub(used + right_width + 1);

    let middle = if let Some(notice) = &player.notice {
        let style = if notice.is_error {
            p.fg(p.error)
        } else {
            Style::default()
        };
        Span::styled(fit(&notice.text, room), style)
    } else {
        let song = song(player);
        if song.is_empty() {
            Span::raw("")
        } else {
            spans.push(Span::styled("\u{266a} ", p.fg(p.accent)));
            Span::raw(fit(&song, room.saturating_sub(2)))
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
        right.push(Span::styled("░".repeat(bar_cells - filled), p.secondary()));
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

/// The keys for what the screen shows, most important first, always
/// ending in `? help`
fn draw_help(frame: &mut Frame, view: &View, player: &Player, area: Rect) {
    let p = view.palette;
    let playing = player.station.as_ref();
    let keys = input::help_line(view, playing, player.is_active());
    let end = input::help_line_end();
    let width_of = |(key, what): (&str, &str)| key.width() + what.width() + 3;
    let room = (area.width as usize).saturating_sub(width_of(end));
    let mut used = 0;
    let mut spans = Vec::new();
    let push = |spans: &mut Vec<Span>, (key, what): (&str, &str)| {
        spans.push(Span::styled(format!(" {key}"), Style::default().bold()));
        spans.push(Span::styled(format!(" {what} "), p.secondary()));
    };
    for pair in keys {
        // Later ones that are shorter may still fit
        if used + width_of(pair) <= room {
            used += width_of(pair);
            push(&mut spans, pair);
        }
    }
    push(&mut spans, end);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Every key, grouped, over the screen (`?` or F1). Scrolls when the
/// window is too short.
fn draw_help_screen(frame: &mut Frame, view: &mut View, area: Rect, scroll: u16) {
    let key_width = input::KEY_GROUPS
        .iter()
        .flat_map(|(_, keys)| keys.iter())
        .map(|k| k.keys.width())
        .max()
        .unwrap_or(0);
    // A narrow window cuts the descriptions with "…", not mid-word
    let what_room = (area.width as usize).saturating_sub(2 + 3 + key_width + 2);
    let mut lines = Vec::new();
    // No blank lines between groups (the bold names part them), so it
    // all fits in 80x24
    for (group, keys) in input::KEY_GROUPS.iter() {
        lines.push(Line::styled(format!(" {group}"), Style::default().bold()));
        for k in keys.iter() {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("   {}", pad(k.keys, key_width)),
                    Style::default().bold(),
                ),
                Span::raw(format!("  {}", fit(k.what, what_room))),
            ]));
        }
    }
    let content_width = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 2;
    let width = (content_width + 2).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let inner_rows = height.saturating_sub(2);
    let max_scroll = (lines.len() as u16).saturating_sub(inner_rows);
    let scroll = scroll.min(max_scroll);
    view.help = Some(scroll);
    let title = if max_scroll > 0 {
        " Keys \u{b7} \u{2191}\u{2193} scroll \u{b7} any other key closes "
    } else {
        " Keys \u{b7} any key closes "
    };
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let block = Block::default()
        .title(Span::styled(
            fit(title, width.saturating_sub(2) as usize),
            Style::default().bold(),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block).scroll((scroll, 0)), rect);
}

/// The host of `url`, for a station without a name, without any
/// `user:password@` (a password on screen ends up in screenshots)
fn host(url: &str) -> &str {
    url.split("//")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .map(|h| h.rsplit_once('@').map_or(h, |(_, host)| host))
        .filter(|h| !h.is_empty())
        .unwrap_or(url)
}

/// Cut `text` to `width` terminal columns, ending in "…" when cut
pub fn fit(text: &str, width: usize) -> String {
    let sanitized;
    let text = if text.chars().any(char::is_control) {
        sanitized = text.replace(char::is_control, " ");
        sanitized.as_str()
    } else {
        text
    };
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let w = grapheme.width();
        if used + w > width - 1 {
            break;
        }
        out.push_str(grapheme);
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
    use crate::search::Reply;
    use radiotrope_library::providers::SearchResults;
    use ratatui::backend::TestBackend;

    fn render_search(
        terminal: &mut Terminal<TestBackend>,
        view: &mut View,
        playing: Option<&Station>,
    ) -> String {
        terminal
            .draw(|frame| {
                draw_search(frame, view, playing, None, frame.area());
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(terminal.backend().buffer().area.width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn search_renders_hint_loading_empty_and_retry_states() {
        let mut terminal = Terminal::new(TestBackend::new(44, 6)).unwrap();
        let mut view = View::new(
            vec![],
            Palette::new(ColorMode::None, DEFAULT_ACCENT),
            DEFAULT_ACCENT,
        );
        view.search.open();
        let text = render_search(&mut terminal, &mut view, None);
        assert!(text.contains(" Find: ") && text.contains("Press Enter with no text"));
        let request = view.search.submit();
        assert!(render_search(&mut terminal, &mut view, None).contains("Loading stations"));
        view.search.apply(Reply {
            request,
            result: Ok(SearchResults {
                stations: vec![],
                total: None,
                has_more: true,
            }),
        });
        assert!(render_search(&mut terminal, &mut view, None).contains("No stations on this"));
        let request = view.search.move_cursor(10, true).unwrap();
        assert!(render_search(&mut terminal, &mut view, None).contains("Loading more"));
        view.search.apply(Reply {
            request,
            result: Err("offline".into()),
        });
        assert!(render_search(&mut terminal, &mut view, None)
            .contains("Can't reach the station directory"));
        view.search.editing = false;
        assert!(render_search(&mut terminal, &mut view, None)
            .contains("Can't reach the station directory"));
        let request = view.search.retry().unwrap();
        view.search.apply(Reply {
            request,
            result: Ok(SearchResults::empty()),
        });
        assert!(render_search(&mut terminal, &mut view, None).contains("No stations found"));
    }

    #[test]
    fn narrow_search_renders_country_star_playing_and_grapheme_caret() {
        let mut terminal = Terminal::new(TestBackend::new(24, 5)).unwrap();
        let mut saved = Station::new("Jazz", "https://old.test")
            .with_provider("radiotrope", Some("jazz".into()));
        saved.country = Some("Greece".into());
        let mut playing = saved.clone();
        playing.url = "https://new.test".into();
        let plain = Station::new("News\nFM", "https://news.test");
        let mut view = View::new(
            vec![saved],
            Palette::new(ColorMode::None, DEFAULT_ACCENT),
            DEFAULT_ACCENT,
        );
        view.search.open();
        for c in "界e\u{301}".chars() {
            view.search.insert(c);
        }
        let request = view.search.submit();
        view.search.apply(Reply {
            request,
            result: Ok(SearchResults {
                stations: vec![playing.clone(), plain],
                total: None,
                has_more: false,
            }),
        });
        let text = render_search(&mut terminal, &mut view, Some(&playing));
        assert!(text.contains("▌★ Jazz") && text.contains("Greece"));
        assert!(text.contains("News FM") && !text.contains("End of list"));
        assert_eq!(text.matches('★').count(), 1);
        assert_eq!(
            terminal.backend_mut().get_cursor_position().unwrap(),
            Position::new(10, 0)
        );
        for _ in 0..40 {
            view.search.insert('界');
        }
        render_search(&mut terminal, &mut view, None);
        assert!(terminal.backend_mut().get_cursor_position().unwrap().x < 24);
    }

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
        assert_eq!(host("http://me:secret@live.test:8000/x"), "live.test:8000");
    }
}
