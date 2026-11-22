//! The Statistics dialog: the engine's figures as text

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::ComponentHandle;

use radiotrope::audio::health::{FailureReason, HealthState};
use radiotrope::audio::{HissLevel, LevelPhase, SharedStats, StreamStats};
use radiotrope::stream::StreamType;
use radiotrope_app::text;
use radiotrope_app::text::{status, MISSING, SEPARATOR};

use crate::app::state::AppSnapshot;
use crate::context::lock;
use crate::viz_ui::viz_wanted;
use crate::App;

/// Reads the engine's statistics, once it has sent them
pub(crate) type EngineStats = std::rc::Rc<dyn Fn() -> Option<SharedStats>>;

/// Wire the Statistics dialog: it is filled every 200 ms while open.
/// Returns the engine's statistics, for the play time the poll shows.
pub(crate) fn setup(
    ui: &App,
    shared_state: &Arc<Mutex<AppSnapshot>>,
    stats_rx: crossbeam_channel::Receiver<SharedStats>,
    viz_timer: &std::rc::Rc<slint::Timer>,
) -> EngineStats {
    // The engine's SharedStats, once it has sent them
    let engine_stats = {
        let shared_stats: std::cell::RefCell<Option<SharedStats>> = Default::default();
        std::rc::Rc::new(move || -> Option<SharedStats> {
            let mut shared_stats = shared_stats.borrow_mut();
            if shared_stats.is_none() {
                *shared_stats = stats_rx.try_recv().ok();
            }
            shared_stats.clone()
        })
    };
    // SharedStats → statistics dialog properties
    let show_stats = {
        let ui_weak = ui.as_weak();
        let engine_stats = engine_stats.clone();
        let state = shared_state.clone();
        std::rc::Rc::new(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            // Stopped: clear what the last station left behind, so the
            // dialog doesn't keep showing it as live. Connecting still
            // polls (the buffer fills, the status reads Connecting)
            if !ui.get_is_playing() && !ui.get_is_loading() {
                clear_stats_ui(&ui);
                return;
            }
            let Some(shared_stats) = engine_stats() else {
                return;
            };
            // try_lock: skip this tick if engine holds shared_stats
            let Ok(s) = shared_stats.try_lock() else {
                return;
            };
            let stats_copy = s.clone();
            drop(s);
            update_stats_ui(&ui, &stats_copy);
            let (leveler, hum, hiss) = {
                let s = lock(&state);
                (
                    leveler_text(
                        s.match_volume,
                        s.level.as_deref().map(|r| (r.phase(), r.gain_db())),
                    ),
                    hum_text(s.hum_filter, s.hum.as_ref().and_then(|h| h.hz())),
                    hiss_text(s.hiss),
                )
            };
            ui.set_stat_leveler(leveler.into());
            ui.set_stat_hum(hum.into());
            ui.set_stat_hiss(hiss.into());
        })
    };
    // Every 200 ms while the dialog is open; it stops itself once closed
    let stats_dialog_timer = std::rc::Rc::new(slint::Timer::default());
    {
        let ui_weak = ui.as_weak();
        let show_stats = show_stats.clone();
        let this = std::rc::Rc::downgrade(&stats_dialog_timer);
        stats_dialog_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(200),
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                if !ui.get_stats_open() {
                    if let Some(timer) = this.upgrade() {
                        timer.stop();
                    }
                    return;
                }
                show_stats();
            },
        );
    }
    // The visualizer was shown or hidden, or the statistics dialog opened
    // or closed: start what feeds them
    {
        let ui_weak = ui.as_weak();
        let viz_timer = viz_timer.clone();
        let stats_dialog_timer = stats_dialog_timer.clone();
        ui.on_live_views_changed(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if viz_wanted(&ui) && !viz_timer.running() {
                viz_timer.restart();
            }
            if ui.get_stats_open() && !stats_dialog_timer.running() {
                // Right away, not with the last station's figures
                show_stats();
                stats_dialog_timer.restart();
            }
        });
    }
    engine_stats
}

pub(crate) fn update_stats_ui(ui: &App, s: &StreamStats) {
    // Stream section
    ui.set_stat_health(format_health(&s.health_state).into());
    ui.set_stat_playtime(format_playtime(s.play_started_at).into());

    // Codec section
    let (codec, bitrate, sample_rate, channels) = if let Some(ref ci) = s.codec_info {
        let codec_str = match s.stream_type {
            Some(StreamType::Hls) => format!("{} (HLS)", ci.codec_name),
            Some(StreamType::Direct) | None => ci.codec_name.clone(),
        };
        let br = ci
            .bitrate
            .map(|b| format!("{b} kbps"))
            .unwrap_or_else(|| MISSING.into());
        let sr = if ci.sample_rate > 0 {
            text::sample_rate(ci.sample_rate)
        } else {
            MISSING.into()
        };
        let ch = match ci.channels {
            0 => MISSING.into(),
            1 => "Mono".into(),
            2 => "Stereo".into(),
            n => format!("{n} ch"),
        };
        (codec_str, br, sr, ch)
    } else {
        (
            MISSING.into(),
            MISSING.into(),
            MISSING.into(),
            MISSING.into(),
        )
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
        MISSING.into()
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

/// Statistics dialog with nothing playing
pub(crate) fn clear_stats_ui(ui: &App) {
    ui.set_stat_health(status::STOPPED.into());
    for set in [
        App::set_stat_playtime,
        App::set_stat_codec,
        App::set_stat_bitrate,
        App::set_stat_sample_rate,
        App::set_stat_channels,
        App::set_stat_received,
        App::set_stat_segments,
        App::set_stat_throughput,
        App::set_stat_buffer_level,
        App::set_stat_buffer_capacity,
        App::set_stat_frames_played,
        App::set_stat_decode_errors,
        App::set_stat_underruns,
        App::set_stat_leveler,
        App::set_stat_hum,
        App::set_stat_hiss,
    ] {
        set(ui, MISSING.into());
    }
}

/// The Volume Leveler on the Statistics dialog: what it does to the
/// station playing ("−6.5 dB"), "Learning" while it measures a station
/// heard for the first time, "Off", or "—" before the station's audio
pub(crate) fn leveler_text(on: bool, readout: Option<(LevelPhase, f32)>) -> String {
    if !on {
        return "Off".into();
    }
    match readout {
        Some((LevelPhase::Learning, _)) => "Learning".into(),
        Some((LevelPhase::Settled, gain_db)) => level_text(gain_db),
        _ => MISSING.into(),
    }
}

/// The Hiss Reducer's strength on the Statistics dialog
pub(crate) fn hiss_text(level: HissLevel) -> &'static str {
    match level {
        HissLevel::Off => "Off",
        HissLevel::Light => "Light",
        HissLevel::Strong => "Strong",
    }
}

/// The Statistics dialog's "Hum filter": the mains frequency the Hum
/// Filter is taking out ("50 Hz", "60 Hz"), "Listening" while it is on
/// and hears no hum, or "Off"
pub(crate) fn hum_text(filter_on: bool, hz: Option<u32>) -> String {
    match (filter_on, hz) {
        (false, _) => "Off".into(),
        (true, Some(hz)) => format!("{hz} Hz"),
        (true, None) => "Listening".into(),
    }
}

/// Stream status shown on the Statistics dialog's first tile
pub(crate) fn format_health(state: &HealthState) -> String {
    match state {
        HealthState::WaitingForAudio => status::CONNECTING.into(),
        HealthState::Healthy => status::PLAYING.into(),
        HealthState::Stalled => "Stalled".into(),
        HealthState::Failed(reason) => {
            let why = match reason {
                FailureReason::NoAudioOutput => "no audio",
                FailureReason::StreamStall => "stopped sending",
                FailureReason::ProbeFailed => "unknown format",
            };
            format!("Failed{SEPARATOR}{why}")
        }
    }
}

/// How long the station has played: "4:05", "1:02:03", or "—"
pub(crate) fn format_playtime(started: Option<Instant>) -> String {
    match started {
        Some(t) => text::clock(t.elapsed().as_secs()),
        None => MISSING.into(),
    }
}

pub(crate) fn format_bytes(bytes: u64) -> String {
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

pub(crate) fn format_number(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// "−6.5 dB": what the Volume Leveler does to the station, to half a dB
pub(crate) fn level_text(gain_db: f32) -> String {
    text::signed_db((gain_db * 2.0).round() / 2.0, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_level_reads_to_half_a_db() {
        assert_eq!(level_text(-6.4), "\u{2212}6.5 dB");
        assert_eq!(level_text(3.1), "+3.0 dB");
        assert_eq!(level_text(0.1), "0 dB");
        assert_eq!(level_text(-0.2), "0 dB");
        assert_eq!(
            leveler_text(false, Some((LevelPhase::Settled, -6.4))),
            "Off"
        );
        assert_eq!(
            leveler_text(true, Some((LevelPhase::Learning, 0.0))),
            "Learning"
        );
        assert_eq!(
            leveler_text(true, Some((LevelPhase::Settled, -6.4))),
            "\u{2212}6.5 dB"
        );
        assert_eq!(
            leveler_text(true, Some((LevelPhase::None, 0.0))),
            "\u{2014}"
        );
        assert_eq!(leveler_text(true, None), "\u{2014}");
    }

    #[test]
    fn health_uses_the_playback_words() {
        assert_eq!(
            format_health(&HealthState::WaitingForAudio),
            "Connecting\u{2026}"
        );
        assert_eq!(format_health(&HealthState::Healthy), "Playing");
        assert_eq!(
            format_health(&HealthState::Failed(FailureReason::NoAudioOutput)),
            "Failed \u{b7} no audio"
        );
        assert!(!format_health(&HealthState::Failed(FailureReason::ProbeFailed)).contains('('));
        assert_eq!(format_playtime(None), "\u{2014}");
    }

    #[test]
    fn hum_reads_off_listening_or_the_mains_frequency() {
        assert_eq!(hum_text(false, Some(50)), "Off");
        assert_eq!(hum_text(true, None), "Listening");
        assert_eq!(hum_text(true, Some(50)), "50 Hz");
        assert_eq!(hum_text(true, Some(60)), "60 Hz");
    }
}
