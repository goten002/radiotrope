mod app;
mod mcp;

slint::include_modules!();

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use crossbeam_channel::bounded;
use slint::{ModelRc, VecModel};

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
        let is_muted = mute_state.lock().unwrap_or_else(|e| e.into_inner()).is_muted;
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
    let analysis = analysis_rx
        .recv_timeout(Duration::from_secs(5))
        .ok();
    let shared_stats = stats_rx.recv_timeout(Duration::from_secs(5)).ok();

    // Visualization timer — 30ms (~33 FPS)
    let _viz_timer = slint::Timer::default();
    if let Some(analysis) = analysis {
        let ui_weak = ui.as_weak();
        _viz_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(30),
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                let a = analysis.lock().unwrap_or_else(|e| e.into_inner());
                ui.set_vu_left(a.vu_left);
                ui.set_vu_right(a.vu_right);
                let spectrum_vec: Vec<f32> = a.spectrum.to_vec();
                drop(a);
                ui.set_spectrum(ModelRc::new(VecModel::from(spectrum_vec)));
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
                let s = shared_stats.lock().unwrap_or_else(|e| e.into_inner());
                update_stats_ui(&ui, &s);
            },
        );
    }

    // Poll shared state → Slint properties (runs on UI thread via Timer)
    let ui_weak = ui.as_weak();
    let poll_state = shared_state.clone();
    let _timer = slint::Timer::default();
    _timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        let s = poll_state.lock().unwrap_or_else(|e| e.into_inner());
        ui.set_station_name(s.station_name.as_deref().unwrap_or("Radiotrope").into());
        ui.set_codec_info(format_codec_line(&s).into());
        ui.set_status_text(s.status_text.as_ref().into());
        let is_loading = s.is_resolving || s.status_text == "Connecting...";

        // Playback state
        ui.set_is_playing(s.playback == PlaybackState::Playing);

        // Now-playing title from ICY metadata
        let now_playing = if !s.title.is_empty() {
            if !s.artist.is_empty() {
                format!("{} - {}", s.artist, s.title)
            } else {
                s.title.clone()
            }
        } else {
            String::new()
        };
        ui.set_now_playing_title(now_playing.into());

        // Volume & mute — controller is source of truth; skip volume during drag
        if !ui.get_volume_dragging() {
            ui.set_volume(s.volume);
        }
        ui.set_is_muted(s.is_muted);

        // Remember station URL for play-clicked
        if let Some(ref url) = s.station_url {
            ui.set_station_url(url.as_str().into());
        }

        drop(s);
        ui.set_is_loading(is_loading);
    });

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
        let br = ci.bitrate.map(|b| format!("{b} kbps")).unwrap_or_else(|| "--".into());
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
