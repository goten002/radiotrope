//! Radiotrope CLI — terminal internet radio player

use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::*;

#[derive(Parser)]
#[command(name = "radiotrope", about = "Terminal internet radio player", version)]
struct Cli {
    /// Stream URL to play
    url: String,
}

use radiotrope::audio::health::HealthState;
use radiotrope::audio::types::{AudioAnalysis, AudioEvent, CodecInfo};
use radiotrope::audio::{AudioEngine, SharedStats};
use radiotrope::stream::types::StreamType;
use radiotrope::stream::StreamResolver;

struct App {
    station_name: Option<String>,
    stream_type: StreamType,
    stream_url: String,
    now_playing: String,
    codec_info: Option<CodecInfo>,
    spectrum: Vec<u64>,
    frames_played: u64,
    decode_errors: u64,
    bytes_received: u64,
    health_state: HealthState,
    buffer_target: usize,
    buffer_level: usize,
    is_buffering: bool,
    throughput_kbps: f64,
    underrun_count: u32,
    last_underrun_at: Option<Instant>,
    prev_underrun_count: u32,
    hls_segments: u64,
    play_started_at: Option<Instant>,
    volume: f32,
    muted: bool,
    status: String,
    running: bool,
}

impl App {
    fn new(url: &str) -> Self {
        Self {
            station_name: None,
            stream_type: StreamType::Direct,
            stream_url: url.to_string(),
            now_playing: String::new(),
            codec_info: None,
            spectrum: vec![0; 16],
            frames_played: 0,
            decode_errors: 0,
            bytes_received: 0,
            health_state: HealthState::WaitingForAudio,
            buffer_target: 0,
            buffer_level: 0,
            is_buffering: false,
            throughput_kbps: 0.0,
            underrun_count: 0,
            last_underrun_at: None,
            prev_underrun_count: 0,
            hls_segments: 0,
            play_started_at: None,
            volume: 1.0,
            muted: false,
            status: "Resolving...".to_string(),
            running: true,
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let url = cli.url;

    // Resolve stream before entering TUI (prints to stderr on failure)
    eprintln!("Resolving {}...", url);
    let resolved = match StreamResolver::resolve(&url) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    let engine = match AudioEngine::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Audio error: {}", e);
            std::process::exit(1);
        }
    };

    let mut app = App::new(&url);
    app.station_name = resolved.info.station_name.clone();
    app.stream_type = resolved.info.stream_type;

    let format_hint = resolved.info.format_hint.clone();
    let bitrate = resolved.info.bitrate;
    let metadata_rx = resolved.metadata_rx;
    let bytes_received_counter = resolved.bytes_received.clone();
    let segments_counter = resolved.segments_downloaded.clone();

    engine.play_with_stats(
        resolved.reader,
        format_hint,
        bitrate,
        resolved.bytes_received,
    );
    app.status = "Buffering...".to_string();

    let analysis = engine.analysis();
    let shared_stats = engine.shared_stats();
    let event_rx = engine.event_receiver().clone();

    // Suppress stderr during TUI — ALSA/PulseAudio and other libs write
    // diagnostic messages to stderr which corrupt the ratatui display.
    let saved_stderr = unsafe { libc::dup(2) };
    {
        let devnull = std::fs::File::open("/dev/null")?;
        unsafe { libc::dup2(devnull.as_raw_fd(), 2) };
    }

    // Enter TUI
    terminal::enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let tick_rate = Duration::from_millis(33); // ~30fps
    let mut last_tick = Instant::now();
    let mut saved_volume: f32 = 1.0;

    while app.running {
        // Draw
        terminal.draw(|f| draw_ui(f, &app))?;

        // Poll input
        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            app.running = false;
                        }
                        KeyCode::Char('m') => {
                            if app.muted {
                                app.muted = false;
                                app.volume = saved_volume;
                            } else {
                                saved_volume = app.volume;
                                app.muted = true;
                                app.volume = 0.0;
                            }
                            engine.set_volume(app.volume);
                        }
                        KeyCode::Char('+') | KeyCode::Char('=') => {
                            app.volume = (app.volume + 0.05).min(2.0);
                            app.muted = false;
                            engine.set_volume(app.volume);
                        }
                        KeyCode::Char('-') => {
                            app.volume = (app.volume - 0.05).max(0.0);
                            if app.volume == 0.0 {
                                app.muted = true;
                            }
                            engine.set_volume(app.volume);
                        }
                        _ => {}
                    }
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();

            // Poll engine events
            while let Ok(event) = event_rx.try_recv() {
                match event {
                    AudioEvent::Playing(info) => {
                        app.codec_info = Some(info);
                        app.status = "Playing".to_string();
                        if app.play_started_at.is_none() {
                            app.play_started_at = Some(Instant::now());
                        }
                    }
                    AudioEvent::Stopped => {
                        app.status = "Stopped".to_string();
                        app.running = false;
                    }
                    AudioEvent::Error(_) => {
                        app.status = "Error".to_string();
                    }
                    AudioEvent::ProbeTimeout => {
                        app.status = "Probe timeout (ADTS?)".to_string();
                    }
                    AudioEvent::NoAudioTimeout => {
                        app.status = "No audio output".to_string();
                    }
                    AudioEvent::Buffering(pct) => {
                        if pct < 100 {
                            app.status = format!("Buffering... {}%", pct);
                        } else {
                            app.status = "Playing".to_string();
                        }
                    }
                    _ => {}
                }
            }

            // Poll metadata
            if let Some(ref rx) = metadata_rx {
                while let Ok(meta) = rx.try_recv() {
                    let parts: Vec<&str> = [meta.artist.as_deref(), meta.title.as_deref()]
                        .iter()
                        .filter_map(|s| *s)
                        .collect();
                    if !parts.is_empty() {
                        app.now_playing = parts.join(" - ");
                    }
                }
            }

            // Read analysis data
            update_analysis(&analysis, &mut app);

            // Read shared stats
            update_stats(
                &shared_stats,
                &bytes_received_counter,
                &segments_counter,
                &mut app,
            );
        }
    }

    // Stop and drop engine while still in alternate screen
    // (rodio prints "Dropping OutputStream..." to stderr on drop)
    engine.stop();
    drop(engine);

    // Restore terminal
    terminal::disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    // Restore stderr
    if saved_stderr >= 0 {
        unsafe {
            libc::dup2(saved_stderr, 2);
            libc::close(saved_stderr);
        }
    }

    Ok(())
}

fn update_analysis(analysis: &Arc<Mutex<AudioAnalysis>>, app: &mut App) {
    if let Ok(a) = analysis.lock() {
        for (i, &band) in a.spectrum.iter().enumerate().take(16) {
            app.spectrum[i] = (band * 100.0).clamp(0.0, 100.0) as u64;
        }
    }
}

fn update_stats(
    shared_stats: &SharedStats,
    bytes_counter: &Option<Arc<std::sync::atomic::AtomicU64>>,
    segments_counter: &Option<Arc<std::sync::atomic::AtomicU64>>,
    app: &mut App,
) {
    if let Ok(stats) = shared_stats.lock() {
        app.frames_played = stats.frames_played;
        app.decode_errors = stats.decode_errors;
        app.health_state = stats.health_state;
        app.buffer_target = stats.buffer_target_bytes;
        app.buffer_level = stats.buffer_level_bytes;
        app.is_buffering = stats.is_buffering;
        app.throughput_kbps = stats.throughput_kbps;
        if stats.underrun_count > app.prev_underrun_count {
            app.last_underrun_at = Some(Instant::now());
            app.prev_underrun_count = stats.underrun_count;
        }
        app.underrun_count = stats.underrun_count;
        if stats.play_started_at.is_some() && app.play_started_at.is_none() {
            app.play_started_at = stats.play_started_at;
        }
    }
    if let Some(ref counter) = bytes_counter {
        app.bytes_received = counter.load(Ordering::Relaxed);
    }
    if let Some(ref counter) = segments_counter {
        app.hls_segments = counter.load(Ordering::Relaxed);
    }
}

fn draw_ui(f: &mut Frame, app: &App) {
    let area = f.area();

    let outer = Block::default()
        .title(format!(" Radiotrope v{} ", env!("CARGO_PKG_VERSION")))
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded);
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let chunks = Layout::vertical([
        Constraint::Length(5),  // top row: metadata + spectrum
        Constraint::Length(10), // stats
        Constraint::Length(3),  // help bar
    ])
    .split(inner);

    // Top row: metadata on left, spectrum on right
    let top_cols = Layout::horizontal([Constraint::Percentage(85), Constraint::Percentage(15)])
        .split(chunks[0]);

    draw_metadata(f, app, top_cols[0]);
    draw_spectrum(f, app, top_cols[1]);
    draw_stats(f, app, chunks[1]);
    draw_help(f, app, chunks[2]);
}

fn draw_metadata(f: &mut Frame, app: &App, area: Rect) {
    let station = app
        .station_name
        .as_deref()
        .unwrap_or_else(|| extract_host(&app.stream_url));
    let now = if app.now_playing.is_empty() {
        "---"
    } else {
        &app.now_playing
    };
    let max_url_len = area.width.saturating_sub(9) as usize; // "  URL: " + padding
    let url_display = truncate_str(&app.stream_url, max_url_len);
    let text = vec![
        Line::from(vec![
            Span::styled("  Station: ", Style::default().fg(Color::DarkGray)),
            Span::styled(station, Style::default().fg(Color::White).bold()),
        ]),
        Line::from(vec![
            Span::styled("  Now Playing: ", Style::default().fg(Color::DarkGray)),
            Span::styled(now, Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::styled("  URL: ", Style::default().fg(Color::DarkGray)),
            Span::styled(url_display, Style::default().fg(Color::DarkGray)),
        ]),
    ];
    f.render_widget(Paragraph::new(text), area);
}

fn extract_host(url: &str) -> &str {
    url.split("//")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .unwrap_or(url)
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else if max > 3 {
        format!("{}...", &s[..max - 3])
    } else {
        s[..max].to_string()
    }
}

fn draw_spectrum(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Spectrum ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));

    let sparkline = Sparkline::default()
        .block(block)
        .data(&app.spectrum)
        .max(100)
        .style(Style::default().fg(Color::Cyan));

    f.render_widget(sparkline, area);
}

fn draw_stats(f: &mut Frame, app: &App, area: Rect) {
    // Split into two rows
    let rows = Layout::vertical([
        Constraint::Length(5), // Playback (border + 3 lines + border)
        Constraint::Length(4), // Network (border + 2 lines + border)
    ])
    .split(area);

    // === Left box: Playback ===
    let health_str = match app.health_state {
        HealthState::WaitingForAudio => "Waiting",
        HealthState::Healthy => "Healthy",
        HealthState::Stalled => "Stalled",
        HealthState::Failed(_) => "Failed",
    };
    let health_color = match app.health_state {
        HealthState::Healthy => Color::Green,
        HealthState::Stalled => Color::Yellow,
        HealthState::Failed(_) => Color::Red,
        _ => Color::DarkGray,
    };
    let status_color = match app.status.as_str() {
        "Playing" => Color::Green,
        "Error" | "Probe timeout (ADTS?)" | "No audio output" => Color::Red,
        _ => Color::Yellow,
    };
    let uptime = match app.play_started_at {
        Some(started) => format_uptime(started),
        None => "00:00".to_string(),
    };
    let stream_tag = match app.stream_type {
        StreamType::Hls => " (HLS)",
        StreamType::Direct => "",
    };
    let codec_str = match &app.codec_info {
        Some(info) => format!("{}{}", info.codec_name, stream_tag),
        None => format!("---{}", stream_tag),
    };
    let bitrate_str = match &app.codec_info {
        Some(info) => info
            .bitrate
            .map(|b| format!("{} kbps", b))
            .unwrap_or_else(|| "---".to_string()),
        None => "---".to_string(),
    };
    let sample_str = match &app.codec_info {
        Some(info) => format!("{} Hz", info.sample_rate),
        None => "---".to_string(),
    };
    let channels_str = match &app.codec_info {
        Some(info) => if info.channels == 1 { "Mono" } else { "Stereo" }.to_string(),
        None => "---".to_string(),
    };

    let left_block = Block::default()
        .title(" Playback ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
    let left_text = vec![
        Line::from(vec![
            Span::styled("  Stream: ", Style::default().fg(Color::DarkGray)),
            Span::styled(health_str, Style::default().fg(health_color)),
            Span::raw("  "),
            Span::styled("Playback: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&app.status, Style::default().fg(status_color)),
            Span::raw("  "),
            Span::styled("Uptime: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&uptime, Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  Codec: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&codec_str, Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled("Bitrate: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&bitrate_str, Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled("Sample Rate: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&sample_str, Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  Channels: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&channels_str, Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled("Blocks Played: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_number(app.frames_played),
                Style::default().fg(Color::White),
            ),
            Span::raw("  "),
            Span::styled("Discarded: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", app.decode_errors),
                Style::default().fg(if app.decode_errors > 0 {
                    Color::Red
                } else {
                    Color::White
                }),
            ),
        ]),
    ];
    f.render_widget(Paragraph::new(left_text).block(left_block), rows[0]);

    // === Right box: Network ===
    let throughput_kbps = app.throughput_kbps;
    let underrun_str = if app.underrun_count == 0 {
        "0".to_string()
    } else if let Some(last) = app.last_underrun_at {
        let ago = last.elapsed().as_secs();
        if ago < 2 {
            format!("{} (now)", app.underrun_count)
        } else {
            format!("{} ({}s ago)", app.underrun_count, ago)
        }
    } else {
        format!("{}", app.underrun_count)
    };
    let underrun_color = if app.underrun_count > 0 {
        if app
            .last_underrun_at
            .is_some_and(|t| t.elapsed().as_secs() < 3)
        {
            Color::Red
        } else {
            Color::Yellow
        }
    } else {
        Color::White
    };

    let right_block = Block::default()
        .title(" Network ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
    let mut right_lines = Vec::new();
    if app.stream_type == StreamType::Hls {
        right_lines.push(Line::from(vec![
            Span::styled("  Segments: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", app.hls_segments),
                Style::default().fg(Color::White),
            ),
            Span::raw("  "),
            Span::styled("Read: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_bytes(app.bytes_received),
                Style::default().fg(Color::White),
            ),
            Span::raw("  "),
            Span::styled("Input: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.0} kbps", throughput_kbps),
                Style::default().fg(Color::White),
            ),
        ]));
    } else {
        right_lines.push(Line::from(vec![
            Span::styled("  Received: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format_bytes(app.bytes_received),
                Style::default().fg(Color::White),
            ),
            Span::raw("  "),
            Span::styled("Input: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.0} kbps", throughput_kbps),
                Style::default().fg(Color::White),
            ),
        ]));
    }
    right_lines.push(Line::from(vec![
        Span::styled("  Buffer: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format_bytes(app.buffer_level as u64),
            Style::default().fg(if app.is_buffering {
                Color::Yellow
            } else {
                Color::White
            }),
        ),
        Span::raw("  "),
        Span::styled("Underruns: ", Style::default().fg(Color::DarkGray)),
        Span::styled(&underrun_str, Style::default().fg(underrun_color)),
    ]));
    f.render_widget(Paragraph::new(right_lines).block(right_block), rows[1]);
}

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let vol_display = if app.muted {
        "MUTE".to_string()
    } else {
        format!("{}%", (app.volume * 100.0).round() as u32)
    };

    let help = Line::from(vec![
        Span::styled("  'q' ", Style::default().fg(Color::Yellow)),
        Span::raw("quit  |  "),
        Span::styled("'m' ", Style::default().fg(Color::Yellow)),
        Span::raw("mute  |  "),
        Span::styled("'+'/'-' ", Style::default().fg(Color::Yellow)),
        Span::raw("volume  |  "),
        Span::styled(
            format!("Vol: {}", vol_display),
            Style::default().fg(Color::Cyan).bold(),
        ),
    ]);

    f.render_widget(Paragraph::new(help).alignment(Alignment::Left), area);
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn format_uptime(started: Instant) -> String {
    let secs = started.elapsed().as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

fn format_number(n: u64) -> String {
    if n >= 10_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 100_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else if n >= 10_000 {
        format!("{:.2}K", n as f64 / 1_000.0)
    } else {
        let s = n.to_string();
        let mut result = String::new();
        for (i, c) in s.chars().rev().enumerate() {
            if i > 0 && i % 3 == 0 {
                result.push(',');
            }
            result.push(c);
        }
        result.chars().rev().collect()
    }
}
