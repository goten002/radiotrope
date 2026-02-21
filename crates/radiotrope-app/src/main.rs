mod app;
mod mcp;

slint::include_modules!();

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use crossbeam_channel::bounded;
use slint::{Model, ModelRc, VecModel};

use radiotrope::audio::health::HealthState;
use radiotrope::audio::{AudioAnalysis, PlaybackState, SharedStats, StreamStats};
use radiotrope::stream::StreamType;

use app::controller::AppController;
use app::state::AppSnapshot;

/// Radiotrope — Internet radio player
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Enable MCP server on stdio (for AI agent integration)
    #[arg(long)]
    mcp: bool,
}

fn main() {
    let args = Args::parse();

    // Shared command channel + state
    let (cmd_tx, cmd_rx) = bounded(64);
    let shared_state = Arc::new(Mutex::new(AppSnapshot::default()));

    // Channel for the engine's analysis Arc (one-shot handshake)
    let (analysis_tx, analysis_rx) = bounded::<Arc<Mutex<AudioAnalysis>>>(1);

    // Channel for the engine's SharedStats (one-shot handshake)
    let (stats_tx, stats_rx) = bounded::<SharedStats>(1);

    // If --mcp, spawn MCP stdio server on a background thread
    if args.mcp {
        let mcp_tx = cmd_tx.clone();
        let mcp_state = shared_state.clone();
        std::thread::Builder::new()
            .name("mcp-stdio".into())
            .spawn(move || {
                mcp::server::run(mcp_tx, mcp_state);
            })
            .expect("Failed to spawn MCP thread");
    }

    // Create Slint UI
    let ui = App::new().unwrap();

    // Wire Slint callbacks → cmd_tx
    let play_tx = cmd_tx.clone();
    ui.on_play_url(move |url| {
        let _ = play_tx.send(app::state::AppCommand::Play(url.to_string()));
    });

    let stop_tx = cmd_tx.clone();
    ui.on_stop_clicked(move || {
        let _ = stop_tx.send(app::state::AppCommand::Stop);
    });

    let vol_tx = cmd_tx.clone();
    ui.on_volume_changed(move |vol| {
        let _ = vol_tx.send(app::state::AppCommand::SetVolume(vol));
    });

    let mute_tx = cmd_tx.clone();
    let mute_state = shared_state.clone();
    ui.on_mute_clicked(move || {
        let is_muted = mute_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_muted;
        if is_muted {
            let _ = mute_tx.send(app::state::AppCommand::Unmute);
        } else {
            let _ = mute_tx.send(app::state::AppCommand::Mute);
        }
    });

    // TODO: set up system tray when mcp_mode is true

    // Spawn controller on its own thread
    let ctrl_state = shared_state.clone();
    let ctrl_tx = cmd_tx.clone();
    std::thread::Builder::new()
        .name("controller".into())
        .spawn(move || {
            let mut ctrl = AppController::new(cmd_rx, ctrl_tx, ctrl_state, analysis_tx, stats_tx);
            ctrl.run();
        })
        .expect("Failed to spawn controller thread");

    // Wait for engine to initialize and send us the analysis Arc + SharedStats
    let analysis = analysis_rx.recv_timeout(Duration::from_secs(5)).ok();
    let shared_stats = stats_rx.recv_timeout(Duration::from_secs(5)).ok();

    // Visualization timer — 30ms (~33 FPS)
    let _viz_timer = slint::Timer::default();
    if let Some(analysis) = analysis {
        let ui_weak = ui.as_weak();
        // Pre-allocate the spectrum model once; update in-place each tick
        let spectrum_model = std::rc::Rc::new(VecModel::from(vec![0.0f32; radiotrope::config::audio::SPECTRUM_BANDS]));
        let spectrum_rc = ModelRc::from(spectrum_model.clone());
        ui.set_spectrum(spectrum_rc);
        _viz_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(30),
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                // try_lock: skip this tick if engine/analyzer holds the lock
                let Ok(a) = analysis.try_lock() else { return };
                let (vu_l, vu_r, spectrum) = (a.vu_left, a.vu_right, a.spectrum);
                drop(a);
                ui.set_vu_left(vu_l);
                ui.set_vu_right(vu_r);
                // Update model in-place — no allocation
                for (i, &val) in spectrum.iter().enumerate() {
                    spectrum_model.set_row_data(i, val);
                }
            },
        );
    }

    // Poll SharedStats → statistics dialog properties (200ms)
    let _stats_timer = slint::Timer::default();
    if let Some(shared_stats) = shared_stats {
        let ui_weak = ui.as_weak();
        _stats_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(200),
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                // try_lock: skip this tick if engine holds shared_stats
                let Ok(s) = shared_stats.try_lock() else { return };
                let stats_copy = s.clone();
                drop(s);
                update_stats_ui(&ui, &stats_copy);
            },
        );
    }

    // Poll shared state → Slint properties (runs on UI thread via Timer)
    let ui_weak = ui.as_weak();
    let poll_state = shared_state.clone();
    let _timer = slint::Timer::default();
    _timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(200),
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            // try_lock: skip this tick if controller holds the lock
            let Ok(s) = poll_state.try_lock() else { return };
            // Copy all data under lock, then drop before touching UI
            let station_name: slint::SharedString =
                s.station_name.as_deref().unwrap_or("Radiotrope").into();
            let codec_info: slint::SharedString = format_codec_line(&s).into();
            let status_text: slint::SharedString = s.status_text.as_ref().into();
            let is_error = s.is_error;
            let is_loading = s.is_resolving || s.status_text == "Connecting...";
            let is_playing = s.playback == PlaybackState::Playing;
            let now_playing: slint::SharedString = if !s.title.is_empty() {
                if !s.artist.is_empty() {
                    format!("{} - {}", s.artist, s.title).into()
                } else {
                    s.title.as_str().into()
                }
            } else {
                Default::default()
            };
            let volume = s.volume;
            let is_muted = s.is_muted;
            let station_url: Option<slint::SharedString> =
                s.station_url.as_deref().map(Into::into);
            drop(s);

            // Set UI properties without holding any lock
            ui.set_station_name(station_name);
            ui.set_codec_info(codec_info);
            ui.set_status_text(status_text);
            ui.set_is_error(is_error);
            ui.set_is_playing(is_playing);
            ui.set_now_playing_title(now_playing);
            if !ui.get_volume_dragging() {
                ui.set_volume(volume);
            }
            ui.set_is_muted(is_muted);
            if let Some(url) = station_url {
                ui.set_station_url(url);
            }
            ui.set_is_loading(is_loading);
        },
    );

    // Run Slint event loop (blocks main thread)
    ui.run().unwrap();

    // UI closed — tell controller to shut down
    let _ = cmd_tx.send(app::state::AppCommand::Shutdown);
}

fn update_stats_ui(ui: &App, s: &StreamStats) {
    // Stream section
    ui.set_stat_health(format_health(&s.health_state).into());
    ui.set_stat_uptime(format_uptime(s.play_started_at).into());

    // Codec section
    let (codec, bitrate, sample_rate, channels) = if let Some(ref ci) = s.codec_info {
        let codec_str = match s.stream_type {
            Some(StreamType::Hls) => format!("{} (HLS)", ci.codec_name),
            Some(StreamType::Direct) => format!("{} (ICY)", ci.codec_name),
            None => ci.codec_name.clone(),
        };
        let br = ci
            .bitrate
            .map(|b| format!("{b} kbps"))
            .unwrap_or_else(|| "--".into());
        let sr = if ci.sample_rate > 0 {
            format!("{} Hz", ci.sample_rate)
        } else {
            "--".into()
        };
        let ch = match ci.channels {
            0 => "--".into(),
            1 => "Mono".into(),
            2 => "Stereo".into(),
            n => format!("{n} ch"),
        };
        (codec_str, br, sr, ch)
    } else {
        ("--".into(), "--".into(), "--".into(), "--".into())
    };
    ui.set_stat_codec(codec.into());
    ui.set_stat_bitrate(bitrate.into());
    ui.set_stat_sample_rate(sample_rate.into());
    ui.set_stat_channels(channels.into());

    // Network section
    ui.set_stat_received(format_bytes(s.bytes_received).into());
    let segments_str = if s.segments_downloaded > 0 {
        format_number(s.segments_downloaded)
    } else {
        "--".into()
    };
    ui.set_stat_segments(segments_str.into());
    ui.set_stat_throughput(format!("{:.0} kbps", s.throughput_kbps).into());

    // Buffer section
    ui.set_stat_buffer_level(format_bytes(s.buffer_level_bytes as u64).into());
    ui.set_stat_buffer_capacity(format_bytes(s.buffer_capacity_bytes as u64).into());
    // Decode section
    ui.set_stat_frames_played(format_number(s.frames_played).into());
    ui.set_stat_decode_errors(format_number(s.decode_errors).into());
    ui.set_stat_underruns(format_number(s.underrun_count as u64).into());
}

fn format_health(state: &HealthState) -> String {
    match state {
        HealthState::WaitingForAudio => "Waiting".into(),
        HealthState::Healthy => "Healthy".into(),
        HealthState::Stalled => "Stalled".into(),
        HealthState::Failed(reason) => format!("Failed ({reason:?})"),
    }
}

fn format_uptime(started: Option<Instant>) -> String {
    let Some(t) = started else {
        return "--".into();
    };
    let secs = t.elapsed().as_secs();
    let m = secs / 60;
    let s = secs % 60;
    if m >= 60 {
        let h = m / 60;
        format!("{h:02}:{:02}:{s:02}", m % 60)
    } else {
        format!("{m:02}:{s:02}")
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn format_number(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

fn format_codec_line(s: &AppSnapshot) -> String {
    if s.codec_name.is_empty() {
        return "Awaiting stream".to_string();
    }
    let mut parts = Vec::new();
    if s.stream_type == "HLS" {
        parts.push(format!("{} (HLS)", s.codec_name));
    } else {
        parts.push(s.codec_name.clone());
    }
    if let Some(br) = s.bitrate.filter(|&b| b > 0) {
        parts.push(format!("{} kbps", br));
    }
    if s.sample_rate > 0 {
        parts.push(format!("{} Hz", s.sample_rate));
    }
    if s.channels > 0 {
        parts.push(match s.channels {
            1 => "Mono".to_string(),
            2 => "Stereo".to_string(),
            n => format!("{} ch", n),
        });
    }
    parts.join(" \u{2022} ")
}
