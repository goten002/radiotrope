//! The player: the station in the header and its logo, the Play, Stop and
//! volume handlers, and the poll that shows the controller's state

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, SharedPixelBuffer, VecModel};

use radiotrope::audio::PlaybackState;
use radiotrope_app::config::ui::{PLAY_TAKE_TIMEOUT, SETTINGS_SAVE_DELAY};
use radiotrope_app::data::favorites::{FavoritesManager, PlayMetadata};
use radiotrope_app::data::types::Station;
use radiotrope_app::network::logo::LogoService;
use radiotrope_app::text::{status, SEPARATOR};
use radiotrope_app::visual::logo_palette;
use radiotrope_app::{text, visual};

use crate::app::delayed_save::DelayedSave;
use crate::app::listening::ListenSession;
use crate::app::picked_station::{PendingPick, PickedStation};
use crate::app::shown_station::ShownStation;
use crate::app::state::AppSnapshot;
use crate::app::ui_sender::UiSender;
use crate::context::{lock, AppContext};
use crate::favorites_list::{refresh_favorites, FAVORITES_SHOWN};
use crate::favorites_ui::track_listening;
use crate::recording_ui::{show_listeners, show_recording_state};
use crate::stats_ui::{format_playtime, EngineStats};
use crate::viz_ui::viz_wanted;
use crate::window_settings::{
    save_settings, show_filters, window_settings, write_settings, FilterSettings, WindowSettings,
};
use crate::{app, schedule_ui, App, Defaults, VizStyle};

/// The header at startup: the visualizer takes its colours from the
/// station logo, and the station of the last run is shown, ready to play
pub(crate) fn setup_header(ui: &App, ctx: &AppContext, last_station: Option<&Station>) {
    let (shared_state, favorites, logo_service) = (&ctx.state, &ctx.favorites, &ctx.logos);
    // Re-sample the visualizer colours whenever the station logo or the
    // theme changes (near-white suits the dark theme, near-black the light)
    {
        let ui_weak = ui.as_weak();
        ui.on_logo_changed(move || {
            if let Some(ui) = ui_weak.upgrade() {
                apply_logo_palette(&ui);
            }
        });
    }

    // Restore last station to UI (so user can hit Play to resume)
    if let Some(station) = last_station {
        ui.set_station_name(station.name.as_str().into());
        ui.set_station_url(station.url.as_str().into());
        let shown = show_station(&station.url);
        if let Some(ref logo_url) = station.logo_url {
            ui.set_station_logo_url(logo_url.as_str().into());
        }
        // Check if it's favorited
        {
            let favs = lock(favorites);
            ui.set_is_station_favorited(favs.is_favorite(&station.url));
        }
        // Try to load cached logo
        if let Some(ref logo_url) = station.logo_url {
            if !logo_url.is_empty() {
                let tmp = Station::new(&station.name, &station.url).with_logo(logo_url);
                if let Some((rgba, w, h)) = logo_service.get_cached_rgba(&tmp) {
                    set_header_logo(ui, rgba, w, h);
                } else {
                    // Not cached (e.g. removed by an older cleanup): fetch it
                    let logo_svc = logo_service.clone();
                    let ui_weak = ui.as_weak();
                    let station_url = station.url.clone();
                    std::thread::Builder::new()
                        .name("last-logo-fetch".into())
                        .spawn(move || {
                            if let Some((rgba, w, h)) = logo_svc.get_rgba(&tmp) {
                                let _ = slint::invoke_from_event_loop(move || {
                                    let Some(ui) = ui_weak.upgrade() else { return };
                                    if is_current_station(shown, &station_url) {
                                        set_header_logo(&ui, rgba, w, h);
                                    }
                                });
                            }
                        })
                        .ok();
                }
            }
        }
        if let Some(ref country) = station.country {
            ui.set_station_country(country.as_str().into());
        }
        // Update shared state so controller knows about the last station
        {
            let mut s = lock(shared_state);
            s.station_name = Some(station.name.clone());
            s.station_url = Some(station.url.clone());
            s.station_logo_url = station.logo_url.clone();
            s.station_country = station.country.clone();
        }
    }
}

/// Wire Play, Open Network Stream, Stop, the volume and Mute
pub(crate) fn setup(ui: &App, ctx: &AppContext) {
    let (ui_tx, shared_state) = (&ctx.tx, &ctx.state);
    let (favorites, logo_service) = (&ctx.favorites, &ctx.logos);
    // Wire Slint callbacks → ui_tx
    let play_tx = ui_tx.clone();
    let play_url_weak = ui.as_weak();
    let play_url_favs = favorites.clone();
    let play_url_logo_svc = logo_service.clone();
    let play_url_state = shared_state.clone();
    ui.on_play_url(move |url| {
        if let Some(ui) = play_url_weak.upgrade() {
            // Replaying the station already in the player (the Play button)
            // keeps its name, logo and country, which a station that isn't a
            // favorite has nowhere else
            let current = ui.get_station_url() == url;
            let keep = |value: slint::SharedString| {
                Some(value.to_string()).filter(|v| current && !v.is_empty())
            };
            play_station_with_metadata(
                &ui,
                &play_tx,
                &play_url_state,
                &play_url_favs,
                &play_url_logo_svc,
                PlayMetadata {
                    url: url.to_string(),
                    name: keep(ui.get_station_name()),
                    logo_url: keep(ui.get_station_logo_url()),
                    country: keep(ui.get_station_country()),
                    provider_id: None,
                },
            );
        }
    });

    // Open Network Stream, with the station details typed in the dialog
    {
        let play_tx = ui_tx.clone();
        let favs = favorites.clone();
        let logo_svc = logo_service.clone();
        let state = shared_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_play_stream(move |url, name, logo_url, country| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let url = url.trim();
            if url.is_empty() {
                return;
            }
            let opt = |value: &str| Some(value.trim().to_string()).filter(|v| !v.is_empty());
            play_station_with_metadata(
                &ui,
                &play_tx,
                &state,
                &favs,
                &logo_svc,
                PlayMetadata {
                    url: url.to_string(),
                    name: opt(&name),
                    logo_url: opt(&logo_url),
                    country: opt(&country),
                    provider_id: None,
                },
            );
        });
    }

    let stop_tx = ui_tx.clone();
    ui.on_stop_clicked(move || {
        stop_tx.send(app::state::AppCommand::Stop);
    });

    let vol_tx = ui_tx.clone();
    ui.on_volume_changed(move |vol| {
        vol_tx.send(app::state::AppCommand::SetVolume(vol));
    });

    let mute_tx = ui_tx.clone();
    let mute_state = shared_state.clone();
    ui.on_mute_clicked(move || {
        let is_muted = lock(&mute_state).is_muted;
        if is_muted {
            mute_tx.send(app::state::AppCommand::Unmute);
        } else {
            mute_tx.send(app::state::AppCommand::Mute);
        }
    });
}

/// What the header and the playback bar show of the controller's state
#[derive(Clone, Debug, PartialEq)]
struct PlayerView {
    station_name: slint::SharedString,
    codec_info: slint::SharedString,
    status_text: slint::SharedString,
    is_error: bool,
    is_loading: bool,
    is_playing: bool,
    alarm_beep: bool,
    /// "Artist - Title", or empty
    now_playing: slint::SharedString,
    volume: f32,
    is_muted: bool,
    station_url: Option<slint::SharedString>,
    /// Given with the Play, by the UI or an agent
    station_logo: Option<String>,
    station_country: Option<String>,
    eq_gains: [f32; 10],
    eq_preamp: f32,
    eq_enabled: bool,
    eq_preset: slint::SharedString,
}

impl PlayerView {
    fn of(s: &AppSnapshot) -> Self {
        Self {
            station_name: s.station_name.as_deref().unwrap_or(UNNAMED_STATION).into(),
            codec_info: format_codec_line(s).into(),
            status_text: s.status_text.as_ref().into(),
            is_error: s.is_error,
            is_loading: s.is_resolving || s.status_text == status::CONNECTING,
            is_playing: s.playback == PlaybackState::Playing,
            alarm_beep: s.alarm_beep,
            now_playing: if !s.title.is_empty() {
                if !s.artist.is_empty() {
                    format!("{} - {}", s.artist, s.title).into()
                } else {
                    s.title.as_str().into()
                }
            } else {
                Default::default()
            },
            volume: s.volume,
            is_muted: s.is_muted,
            station_url: s.station_url.as_deref().map(Into::into),
            station_logo: s.station_logo_url.clone(),
            station_country: s.station_country.clone(),
            eq_gains: s.eq_gains,
            eq_preamp: s.eq_preamp,
            eq_enabled: s.eq_enabled,
            eq_preset: s.eq_preset_name.as_deref().unwrap_or("").into(),
        }
    }

    /// A station picked here that the controller hasn't taken up yet shows
    /// as starting: the state still holds the old one, playing
    fn starting(&mut self, picked: &PickedStation) {
        self.station_name = picked.name.as_deref().unwrap_or(UNNAMED_STATION).into();
        self.codec_info = AWAITING_STREAM.into();
        self.status_text = STARTING_STATUS.into();
        self.is_error = false;
        self.is_loading = true;
        self.is_playing = false;
        self.now_playing = Default::default();
        self.station_url = Some(picked.url.as_str().into());
    }
}

/// What the poll keeps from one tick to the next
struct Poll {
    ctx: AppContext,
    viz_timer: std::rc::Rc<slint::Timer>,
    engine_stats: EngineStats,
    /// Logo of the station restored at startup, which may not be a favorite
    restored_logo: Option<(String, String)>,
    /// Recording notice on screen: its sequence number and when it appeared
    notice: (u64, Instant),
    /// Listening session of the station playing now, for favorite stats
    listen: std::rc::Rc<std::cell::RefCell<Option<ListenSession>>>,
    saver: DelayedSave,
    /// The window's settings, saved a moment after they change rather than
    /// only at exit, which a logoff or a crash never reaches
    settings: app::settings_watch::SettingsWatch<WindowSettings>,
}

impl Poll {
    /// Shared state → Slint properties (runs on the UI thread)
    fn tick(&mut self, ui: &App) {
        let ctx = &self.ctx;

        // Commands that found the controller's queue full go out now
        ctx.tx.flush();

        // Check for external favorites changes (e.g. from MCP); the
        // UI's own changes are drawn as they are made
        if let Ok(f) = ctx.favorites.try_lock() {
            let gen = f.generation();
            drop(f);
            if gen != FAVORITES_SHOWN.get() {
                refresh_favorites(ui, &ctx.favorites, &ctx.logos);
            }
        }

        // try_lock: skip this tick if controller holds the lock
        let Ok(s) = ctx.state.try_lock() else { return };
        let picked = PENDING_PICK.with(|p| {
            p.borrow_mut()
                .waiting(s.play_seq, Instant::now(), PLAY_TAKE_TIMEOUT)
                .cloned()
        });
        // Copy all data under lock, then drop before touching UI
        let mut view = PlayerView::of(&s);
        let filters = FilterSettings::of(&s);
        let recording = s.recording.clone();
        let recording_notice = s.recording_notice.clone();
        let listeners = s
            .listen
            .as_ref()
            .map(|l| l.listener_names())
            .unwrap_or_default();
        let schedule_view = schedule_ui::View::of(&s);
        let settings_now = window_settings(&s, ui);
        drop(s);
        if let Some(settings) = self.settings.note(settings_now, Instant::now()) {
            write_settings(&settings);
        }

        if let Some(picked) = picked {
            view.starting(&picked);
        }

        schedule_ui::show_state(ui, schedule_view);
        show_listeners(ui, &listeners);
        show_recording_state(
            ui,
            recording.as_ref(),
            recording_notice.as_ref(),
            view.is_playing,
            &mut self.notice,
        );

        // Credit listening time to the favorite being played
        // An alarm's beep isn't time on its station
        let playing_url = view
            .station_url
            .as_deref()
            .filter(|_| view.is_playing && !view.alarm_beep);
        track_listening(
            ui,
            &ctx.favorites,
            &self.saver,
            &mut self.listen.borrow_mut(),
            playing_url,
        );

        self.show(ui, view);

        show_filters(ui, &filters);
    }

    /// The player's state into the window, without holding any lock
    fn show(&self, ui: &App, view: PlayerView) {
        ui.set_station_name(view.station_name);
        ui.set_codec_info(view.codec_info);
        ui.set_status_text(view.status_text);
        ui.set_is_error(view.is_error);
        ui.set_is_playing(view.is_playing);
        // The play time beside "Playing", kept current here: the
        // statistics dialog, which shows it too, is fed only while open
        if view.is_playing {
            if let Some(stats) = (self.engine_stats)() {
                if let Ok(s) = stats.try_lock() {
                    let started = s.play_started_at;
                    drop(s);
                    ui.set_stat_playtime(format_playtime(started).into());
                }
            }
        }
        // Playing started, or the window came back from being minimised
        if viz_wanted(ui) && !self.viz_timer.running() {
            self.viz_timer.restart();
        }
        ui.set_now_playing_title(view.now_playing);
        if !ui.get_volume_dragging() {
            ui.set_volume(view.volume);
        }
        ui.set_is_muted(view.is_muted);
        if let Some(url) = view.station_url {
            let url_changed = SHOWN_STATION.with(|s| s.borrow().changed(url.as_str()));
            ui.set_station_url(url.clone());
            // Update favorite star based on current station (while a
            // save holds the favorites, the star stays as it was)
            if let Ok(f) = self.ctx.favorites.try_lock() {
                ui.set_is_station_favorited(f.is_favorite(url.as_str()));
            }

            // When station URL changes (e.g. MCP play), update logo
            // and country
            if url_changed {
                self.show_station_details(ui, &url, view.station_logo, view.station_country);
            }
        }
        ui.set_is_loading(view.is_loading);

        // Sync EQ state (for MCP-driven changes)
        ui.set_eq_enabled(view.eq_enabled);
        ui.set_eq_preset_name(view.eq_preset);
        ui.set_eq_preamp(view.eq_preamp);
        ui.set_eq_band0(view.eq_gains[0]);
        ui.set_eq_band1(view.eq_gains[1]);
        ui.set_eq_band2(view.eq_gains[2]);
        ui.set_eq_band3(view.eq_gains[3]);
        ui.set_eq_band4(view.eq_gains[4]);
        ui.set_eq_band5(view.eq_gains[5]);
        ui.set_eq_band6(view.eq_gains[6]);
        ui.set_eq_band7(view.eq_gains[7]);
        ui.set_eq_band8(view.eq_gains[8]);
        ui.set_eq_band9(view.eq_gains[9]);
    }

    /// The logo and country of a station the player wasn't showing yet.
    /// `station_logo` and `station_country` came with the Play.
    fn show_station_details(
        &self,
        ui: &App,
        url: &str,
        station_logo: Option<String>,
        station_country: Option<String>,
    ) {
        let (poll_favs, poll_logo_svc) = (&self.ctx.favorites, &self.ctx.logos);
        let shown = show_station(url);

        // A favorite's details first, then the restored
        // station's own logo, then what came with the Play
        let (fav_logo, fav_country) = poll_favs
            .lock()
            .ok()
            .and_then(|f| {
                f.get_by_url(url)
                    .map(|fav| (fav.station.logo_url.clone(), fav.station.country.clone()))
            })
            .unwrap_or_default();
        let logo_url = fav_logo
            .or_else(|| {
                self.restored_logo
                    .as_ref()
                    .filter(|(u, _)| u == url)
                    .map(|(_, logo)| logo.clone())
            })
            .or(station_logo)
            .filter(|logo| !logo.is_empty());
        let country = fav_country.or(station_country).unwrap_or_default();
        ui.set_station_logo_url(logo_url.as_deref().unwrap_or("").into());
        ui.set_station_country(country.as_str().into());

        // Try cached logo first (works even without logo_url on favorite)
        let tmp_station = Station::new(ui.get_station_name().as_str(), url);
        if let Some((rgba, w, h)) = poll_logo_svc.get_cached_rgba(&tmp_station) {
            set_header_logo(ui, rgba, w, h);
        } else {
            // Clear stale logo only when no cached replacement is available
            ui.set_current_logo(Default::default());
            // Not cached: fetch it
            if let Some(logo) = logo_url {
                let logo_svc = poll_logo_svc.clone();
                let ui_weak2 = ui.as_weak();
                let station_name = ui.get_station_name().to_string();
                let station_url = url.to_string();
                std::thread::Builder::new()
                    .name("poll-logo-fetch".into())
                    .spawn(move || {
                        let tmp = Station::new(&station_name, &station_url).with_logo(&logo);
                        if let Some((rgba, w, h)) = logo_svc.get_rgba(&tmp) {
                            let _ = slint::invoke_from_event_loop(move || {
                                let Some(ui) = ui_weak2.upgrade() else { return };
                                if is_current_station(shown, &station_url) {
                                    set_header_logo(&ui, rgba, w, h);
                                }
                            });
                        }
                    })
                    .ok();
            }
        }
    }
}

/// Start the poll that shows the controller's state in the window, every
/// 200 ms. It runs while the returned timer is kept.
pub(crate) fn start_poll(
    ui: &App,
    ctx: &AppContext,
    last_station: Option<&Station>,
    viz_timer: &std::rc::Rc<slint::Timer>,
    engine_stats: &EngineStats,
    listen_session: &std::rc::Rc<std::cell::RefCell<Option<ListenSession>>>,
    favorites_saver: &DelayedSave,
) -> slint::Timer {
    let mut poll = Poll {
        ctx: ctx.clone(),
        viz_timer: viz_timer.clone(),
        engine_stats: engine_stats.clone(),
        restored_logo: last_station
            .and_then(|s| Some((s.url.clone(), s.logo_url.clone()?)))
            .filter(|(_, logo)| !logo.is_empty()),
        notice: (0u64, Instant::now()),
        listen: listen_session.clone(),
        saver: favorites_saver.clone(),
        settings: app::settings_watch::SettingsWatch::new(SETTINGS_SAVE_DELAY),
    };
    let ui_weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(200),
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            poll.tick(&ui);
        },
    );
    timer
}

/// The header logo's visualizer colours on each theme (dark, light),
/// worked out once per logo: a theme switch only picks
pub(crate) struct HeaderLogo {
    pub(crate) image: slint::Image,
    pub(crate) colors: [Vec<slint::Color>; 2],
}

thread_local! {
    pub(crate) static HEADER_LOGO: std::cell::RefCell<Option<HeaderLogo>> =
        const { std::cell::RefCell::new(None) };
}

/// A logo's visualizer colours on each theme (dark, light). `rgba` is the
/// logo's pixels in RGBA order.
pub(crate) fn logo_colors(rgba: &[u8]) -> [Vec<slint::Color>; 2] {
    [true, false].map(|dark| {
        logo_palette(rgba, dark)
            .into_iter()
            .map(|[r, g, b]| slint::Color::from_rgb_u8(r, g, b))
            .collect()
    })
}

/// A logo as the UI shows it: on its colour fog when it needs one (see
/// [`visual::with_backdrop`])
pub(crate) fn logo_image(rgba: Vec<u8>, width: u32, height: u32) -> slint::Image {
    let (rgba, width, height) = visual::with_backdrop(rgba, width, height);
    let pixels = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, width, height);
    slint::Image::from_rgba8(pixels)
}

/// Show a station logo in the header. The visualizer takes its colours
/// from the logo itself, not from the fog it is shown on.
pub(crate) fn set_header_logo(ui: &App, rgba: Vec<u8>, width: u32, height: u32) {
    let colors = logo_colors(&rgba);
    let image = logo_image(rgba, width, height);
    HEADER_LOGO.with(|header| {
        *header.borrow_mut() = Some(HeaderLogo {
            image: image.clone(),
            colors,
        });
    });
    ui.set_current_logo(image);
}

/// Colour the visualizer with the main colours of the station logo
pub(crate) fn apply_logo_palette(ui: &App) {
    let dark = ui.get_dark_mode();
    let image = ui.get_current_logo();
    HEADER_LOGO.with(|header| {
        let mut header = header.borrow_mut();
        // A logo not shown through set_header_logo (none at all, say)
        if header.as_ref().is_none_or(|h| h.image != image) {
            *header = Some(HeaderLogo {
                colors: image
                    .to_rgba8()
                    .map(|buf| logo_colors(buf.as_bytes()))
                    .unwrap_or_default(),
                image,
            });
        }
        let Some(logo) = header.as_ref() else { return };
        let colors = logo.colors[!dark as usize].clone();
        ui.global::<VizStyle>()
            .set_logo_colors(ModelRc::from(std::rc::Rc::new(VecModel::from(colors))));
    });
}

thread_local! {
    /// The station the player shows, numbered (see [`ShownStation`])
    pub(crate) static SHOWN_STATION: std::cell::RefCell<ShownStation> = Default::default();
    /// A station picked here that the controller hasn't taken up yet
    pub(crate) static PENDING_PICK: std::cell::RefCell<PendingPick> = Default::default();
}

/// The player now shows the station at `url`. Returns its number, for the
/// logo fetched for it.
pub(crate) fn show_station(url: &str) -> u64 {
    SHOWN_STATION.with(|s| s.borrow_mut().show(url))
}

/// Whether a logo fetched for `url` while station number `shown` was in
/// the player may still be shown: no station was picked since (from the
/// UI or an agent). The poll overwrites the logo of a station it finds
/// changed, so one landing just before it doesn't stay.
pub(crate) fn is_current_station(shown: u64, url: &str) -> bool {
    SHOWN_STATION.with(|s| s.borrow().is_current(shown, url))
}

/// Shared helper for all play actions. Enriches metadata from favorites if available,
/// sets all UI properties consistently, sends the Play command, and spawns logo fetch.
pub(crate) fn play_station_with_metadata(
    ui: &App,
    cmd_tx: &UiSender,
    shared_state: &Arc<Mutex<AppSnapshot>>,
    favorites: &Arc<Mutex<FavoritesManager>>,
    logo_service: &Arc<LogoService>,
    req: PlayMetadata,
) {
    // A favorite's URL, name and logo take priority over the caller's
    let (
        PlayMetadata {
            url,
            name,
            logo_url,
            country,
            ..
        },
        is_favorite,
    ) = {
        let favs = lock(favorites);
        let play = favs.resolve_play(req);
        let is_favorite = favs.is_favorite(&play.url);
        (play, is_favorite)
    };

    // A second click on the same station right after the first is ignored:
    // a touchpad can turn one tap into two, which restarted the stream.
    // The first click acts at once, and another station always switches.
    {
        let now = Instant::now();
        let mut last = lock(&LAST_PLAY_CLICK);
        let guard =
            Duration::from_millis(ui.global::<Defaults>().get_repeat_click_guard().max(0) as u64);
        let repeat = last
            .as_ref()
            .is_some_and(|(last_url, at)| *last_url == url && now.duration_since(*at) < guard);
        if repeat {
            return;
        }
        *last = Some((url.clone(), now));
    }
    let shown = show_station(&url);

    // Set UI metadata properties
    ui.set_station_logo_url(logo_url.as_deref().unwrap_or("").into());
    ui.set_station_country(country.as_deref().unwrap_or("").into());

    // Try cached logo first to avoid placeholder flash. The cache is keyed
    // by the stream URL, so a logo cached earlier shows even when the
    // station has no logo URL now (as the poll timer does on a station
    // change); otherwise replaying the same station would lose it.
    let cached = logo_service.get_cached_rgba(&Station::new(name.as_deref().unwrap_or(""), &url));
    let cache_hit = if let Some((rgba, w, h)) = cached {
        set_header_logo(ui, rgba, w, h);
        true
    } else {
        false
    };
    if !cache_hit {
        ui.set_current_logo(Default::default());
    }

    // The controller takes up the Play only after stopping the old
    // station; the poll shows this one as starting until then. Its number
    // is read before the Play goes out, which may be taken at once.
    let play_seq = lock(shared_state).play_seq;
    PENDING_PICK.with(|p| {
        p.borrow_mut()
            .pick(play_seq, &url, name.clone(), Instant::now())
    });

    // Send play command
    cmd_tx.send(app::state::AppCommand::Play {
        url: url.clone(),
        name: name.clone(),
        logo_url: logo_url.clone(),
        country: country.clone(),
        taken: None,
    });
    // Show it as starting right away, in the header, the favorites and the
    // Play button (it turns into Stop), not only at the next state poll
    ui.set_station_url(url.as_str().into());
    ui.set_station_name(name.as_deref().unwrap_or(UNNAMED_STATION).into());
    ui.set_codec_info(AWAITING_STREAM.into());
    ui.set_status_text(STARTING_STATUS.into());
    ui.set_is_error(false);
    ui.set_is_playing(false);
    ui.set_now_playing_title(Default::default());
    ui.set_is_station_favorited(is_favorite);
    ui.set_is_loading(true);

    // Save settings (persists last_station, volume, eq, etc.)
    {
        let mut s = lock(shared_state);
        s.station_url = Some(url.clone());
        s.station_name = name.clone();
        s.station_logo_url = logo_url.clone();
        s.station_country = country;
        drop(s);
        save_settings(shared_state, ui);
    }

    // Fetch logo on background thread (only if not already cached)
    if !cache_hit {
        if let Some(ref logo) = logo_url {
            if !logo.is_empty() {
                let ui_weak = ui.as_weak();
                let logo_svc = logo_service.clone();
                let play_name = name.unwrap_or_default();
                let play_url = url;
                let play_logo = logo.clone();
                std::thread::Builder::new()
                    .name("logo-fetch".into())
                    .spawn(move || {
                        let tmp_station = Station::new(&play_name, &play_url).with_logo(&play_logo);
                        if let Some((rgba, width, height)) = logo_svc.get_rgba(&tmp_station) {
                            let _ = slint::invoke_from_event_loop(move || {
                                let Some(ui) = ui_weak.upgrade() else { return };
                                // Another station may have been picked while
                                // this one's logo was downloading
                                if !is_current_station(shown, &play_url) {
                                    return;
                                }
                                set_header_logo(&ui, rgba, width, height);
                            });
                        }
                    })
                    .ok();
            }
        }
    }
}

/// Stream URL and time of the last station started from the UI, so a
/// double tap doesn't start it twice
pub(crate) static LAST_PLAY_CLICK: Mutex<Option<(String, Instant)>> = Mutex::new(None);

/// The name shown for a stream without one
pub(crate) const UNNAMED_STATION: &str = "Radiotrope";

/// The status of a station that is starting (as the controller sets it)
pub(crate) const STARTING_STATUS: &str = status::CONNECTING;

/// The codec line of a station not playing yet
pub(crate) const AWAITING_STREAM: &str = "Waiting for stream";

pub(crate) fn format_codec_line(s: &AppSnapshot) -> String {
    if s.codec_name.is_empty() {
        return AWAITING_STREAM.to_string();
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
        parts.push(text::sample_rate(s.sample_rate));
    }
    if s.channels > 0 {
        parts.push(match s.channels {
            1 => "Mono".to_string(),
            2 => "Stereo".to_string(),
            n => format!("{} ch", n),
        });
    }
    parts.join(SEPARATOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_line_joins_with_middle_dots() {
        let mut s = AppSnapshot::default();
        assert_eq!(format_codec_line(&s), "Waiting for stream");
        s.codec_name = "MP3".into();
        s.bitrate = Some(128);
        s.sample_rate = 44_100;
        s.channels = 2;
        assert_eq!(
            format_codec_line(&s),
            "MP3 \u{b7} 128 kbps \u{b7} 44.1 kHz \u{b7} Stereo"
        );
    }
    #[test]
    fn the_view_reads_the_snapshot() {
        let mut s = AppSnapshot::default();
        let view = PlayerView::of(&s);
        assert_eq!(view.station_name, UNNAMED_STATION);
        assert_eq!(view.station_url, None);
        assert_eq!(view.codec_info, AWAITING_STREAM);
        assert_eq!(view.now_playing, "");
        assert!(!view.is_playing && !view.is_loading);

        s.station_name = Some("Kiss".into());
        s.station_url = Some("http://kiss.example/live".into());
        s.title = "Song".into();
        assert_eq!(PlayerView::of(&s).now_playing, "Song");
        s.artist = "Band".into();
        s.playback = PlaybackState::Playing;
        let view = PlayerView::of(&s);
        assert_eq!(view.station_name, "Kiss");
        assert_eq!(
            view.station_url.as_deref(),
            Some("http://kiss.example/live")
        );
        assert_eq!(view.now_playing, "Band - Song");
        assert!(view.is_playing && !view.is_loading);
    }

    #[test]
    fn the_view_is_loading_while_resolving_or_connecting() {
        let mut s = AppSnapshot {
            is_resolving: true,
            ..Default::default()
        };
        assert!(PlayerView::of(&s).is_loading);
        s.is_resolving = false;
        s.status_text = status::CONNECTING.into();
        assert!(PlayerView::of(&s).is_loading);
        s.status_text = "Playing".into();
        assert!(!PlayerView::of(&s).is_loading);
    }

    #[test]
    fn a_picked_station_shows_as_starting_over_the_one_still_playing() {
        let mut s = AppSnapshot {
            station_name: Some("Old".into()),
            station_url: Some("http://old.example/".into()),
            title: "Song".into(),
            playback: PlaybackState::Playing,
            is_error: true,
            ..Default::default()
        };
        s.status_text = "Playing".into();
        let mut pending = PendingPick::default();
        let now = Instant::now();
        pending.pick(s.play_seq, "http://new.example/", None, now);
        let picked = pending
            .waiting(s.play_seq, now, PLAY_TAKE_TIMEOUT)
            .cloned()
            .expect("the controller hasn't taken it");

        let mut view = PlayerView::of(&s);
        view.starting(&picked);
        assert_eq!(view.station_name, UNNAMED_STATION);
        assert_eq!(view.station_url.as_deref(), Some("http://new.example/"));
        assert_eq!(view.status_text, STARTING_STATUS);
        assert_eq!(view.codec_info, AWAITING_STREAM);
        assert_eq!(view.now_playing, "");
        assert!(view.is_loading && !view.is_playing && !view.is_error);
    }
}
