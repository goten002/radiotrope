//! Media keys and the system's media controls (desktop builds)
//!
//! The keyboard's play, stop, next and previous keys, a headset's buttons
//! and the system's media panel control the player, and the panel shows
//! what it plays:
//!
//! - Linux: MPRIS (`org.mpris.MediaPlayer2.radiotrope`), which the GNOME,
//!   KDE and COSMIC media widgets and `playerctl` use ([`mpris`])
//! - Windows: the System Media Transport Controls, the media card of the
//!   volume flyout and the lock screen ([`smtc`])
//! - macOS: Now Playing in Control Center and the media keys
//!   ([`now_playing`])
//!
//! The controller tells [`observer`] each change it makes; what the panel
//! shows is worked out from that and handed to the window's thread, which
//! owns the platform's part. A command from the panel runs there too, as
//! the window's own buttons do. With the setting off nothing is
//! registered with the system at all.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use slint::ComponentHandle;

use radiotrope_app::data::types::Station;
use radiotrope_app::network::logo::LogoService;

use crate::app::state::{AppSnapshot, PlayerPhase};
use crate::context::lock;
use crate::App;

#[cfg(target_os = "linux")]
mod mpris;
#[cfg(target_os = "macos")]
mod now_playing;
#[cfg(windows)]
mod smtc;

/// Playing or not, as the system's controls know it. A station on its
/// way (connecting, buffering, reconnecting) counts as playing: Stop is
/// what the button does then.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Status {
    #[default]
    Stopped,
    Paused,
    Playing,
}

impl Status {
    fn of(phase: PlayerPhase) -> Self {
        match phase {
            PlayerPhase::Playing
            | PlayerPhase::Connecting
            | PlayerPhase::Buffering
            | PlayerPhase::Reconnecting => Status::Playing,
            PlayerPhase::Paused => Status::Paused,
            PlayerPhase::Idle | PlayerPhase::Stopped | PlayerPhase::Failed => Status::Stopped,
        }
    }
}

/// What the controller's state says about the panel, copied on its
/// thread: compared there, so the window hears only of changes
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Playing {
    /// The station's name; empty for a station without one
    station: String,
    /// The station's address, which its cached logo is found by
    station_url: String,
    /// The song, as the station sends it (title, and artist when the
    /// station's text was "Artist - Title")
    title: String,
    artist: String,
    status: Status,
}

impl Playing {
    pub(crate) fn of(s: &AppSnapshot) -> Self {
        Self {
            station: s.station_name.as_deref().unwrap_or_default().to_string(),
            station_url: s.station_url.as_deref().unwrap_or_default().to_string(),
            title: s.title.clone(),
            artist: s.artist.clone(),
            status: Status::of(s.phase()),
        }
    }

    /// The same as `s` says, without copying it
    fn same_as(&self, s: &AppSnapshot) -> bool {
        self.station == s.station_name.as_deref().unwrap_or_default()
            && self.station_url == s.station_url.as_deref().unwrap_or_default()
            && self.title == s.title
            && self.artist == s.artist
            && self.status == Status::of(s.phase())
    }
}

/// What the system's panel shows
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Track {
    /// The song when the station names it, else the station
    pub title: String,
    /// The song's artist, or the station under a song with no artist
    pub artist: Option<String>,
    /// The station, under a song that has its own artist
    pub album: Option<String>,
    /// The station's logo, from the logo cache
    pub art: Option<PathBuf>,
    pub status: Status,
    /// There is a station to play
    pub can_play: bool,
}

impl Track {
    /// What the panel shows for `p`. Song info is left out while stopped:
    /// it is the last song heard, not what Play would start.
    pub(crate) fn of(p: &Playing, art: Option<PathBuf>) -> Self {
        let station = match p.station.trim() {
            "" if p.station_url.is_empty() => "",
            "" => crate::player_ui::UNNAMED_STATION,
            name => name,
        };
        let title = p.title.trim();
        let artist = p.artist.trim();
        let song = (p.status != Status::Stopped).then_some((title, artist));
        let some = |text: &str| (!text.is_empty()).then(|| text.to_string());
        let (title, artist, album) = match song {
            Some((title, artist)) if !title.is_empty() && !artist.is_empty() => {
                (title, some(artist), some(station))
            }
            Some((song, "")) | Some(("", song)) if !song.is_empty() => (song, some(station), None),
            _ => (station, None, None),
        };
        Track {
            title: title.to_string(),
            artist,
            album,
            art: art.filter(|_| !p.station_url.is_empty()),
            status: p.status,
            can_play: !p.station_url.is_empty(),
        }
    }
}

/// What the system asks the player to do
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Play,
    /// A live station can't wait where it was: Pause stops it
    Pause,
    /// Not on Windows, whose card sends Play or Pause by the status shown
    #[cfg_attr(windows, allow(dead_code))]
    PlayPause,
    Stop,
    /// The next favorite, as the N key
    Next,
    /// The previous favorite, as the P key
    Previous,
    /// Show the window (MPRIS)
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Raise,
    /// Quit as the menu's Quit does (MPRIS)
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Quit,
}

/// Where the platform's part sends what the system asks for. It may be
/// called on any thread.
pub(crate) type Commands = Arc<dyn Fn(Command) + Send + Sync>;

/// The platform's part: registered with the system while it lives
trait Controls {
    fn show(&mut self, track: &Track);
}

/// The window's side, on its thread
struct Media {
    ui: slint::Weak<App>,
    state: Arc<Mutex<AppSnapshot>>,
    logos: Arc<LogoService>,
    /// The latest the controller told
    playing: Playing,
    /// The station's logo file, looked up once per station (and again
    /// when the header's logo changes): `(station address, file)`
    art: Option<(String, Option<PathBuf>)>,
    controls: Option<Box<dyn Controls>>,
    /// What the controls show now
    shown: Option<Track>,
    /// Counts the times the setting was turned on, so a start that
    /// finishes after it was turned off again does nothing
    turned_on: u64,
}

thread_local! {
    static MEDIA: RefCell<Option<Media>> = const { RefCell::new(None) };
}

/// Set up the setting and, while it is on, the system's media controls.
/// Call once, on the window's thread.
pub(crate) fn setup(ui: &App, state: Arc<Mutex<AppSnapshot>>, logos: Arc<LogoService>, on: bool) {
    let playing = Playing::of(&lock(&state));
    MEDIA.with(|media| {
        *media.borrow_mut() = Some(Media {
            ui: ui.as_weak(),
            state,
            logos,
            playing,
            art: None,
            controls: None,
            shown: None,
            turned_on: 0,
        })
    });
    ui.set_media_controls(on);
    ui.on_media_controls_toggled(turn);
    turn(on);
}

/// The controller's side: tells the window's thread whenever what the
/// panel shows changes. Give it to [`crate::app::controller::AppController::with_observer`].
pub(crate) fn observer() -> crate::app::controller::StateObserver {
    let mut last: Option<Playing> = None;
    Box::new(move |state: &AppSnapshot| {
        if last.as_ref().is_some_and(|last| last.same_as(state)) {
            return;
        }
        let playing = Playing::of(state);
        last = Some(playing.clone());
        let _ = slint::invoke_from_event_loop(move || {
            with_media(|media| {
                media.playing = playing;
                media.refresh();
            })
        });
    })
}

/// The header's logo changed: the panel's may now be in the cache
pub(crate) fn logo_changed() {
    with_media(|media| {
        media.art = None;
        media.refresh();
    });
}

fn with_media(f: impl FnOnce(&mut Media)) {
    MEDIA.with(|media| {
        if let Some(media) = media.borrow_mut().as_mut() {
            f(media);
        }
    });
}

/// Register the player with the system, or take it away
fn turn(on: bool) {
    let Some(ui) = MEDIA.with(|media| {
        let mut media = media.borrow_mut();
        let media = media.as_mut()?;
        media.turned_on += 1;
        media.controls = None;
        media.shown = None;
        Some(media.ui.clone())
    }) else {
        return;
    };
    if !on {
        return;
    }
    let started = MEDIA.with(|m| m.borrow().as_ref().map_or(0, |m| m.turned_on));
    // Windows needs the window's handle, which the window has once the
    // event loop runs; the others start here too, for one way of doing it
    let _ = slint::spawn_local(async move {
        use slint::winit_030::WinitWindowAccessor;
        let Some(window) = ui.upgrade() else {
            return;
        };
        if let Err(e) = window.window().winit_window().await {
            eprintln!("Media keys unavailable: {e}");
            return;
        }
        let Some(window) = ui.upgrade() else {
            return;
        };
        let current = MEDIA.with(|m| m.borrow().as_ref().map_or(0, |m| m.turned_on));
        if current != started || !window.get_media_controls() {
            return;
        }
        match start(&window, commands(&window)) {
            Ok(controls) => with_media(|media| {
                media.controls = Some(controls);
                media.refresh();
            }),
            Err(e) => eprintln!("Media keys unavailable: {e}"),
        }
    });
}

impl Media {
    /// Show the controller's latest on the system's controls, when it
    /// differs from what they show
    fn refresh(&mut self) {
        let Some(controls) = self.controls.as_mut() else {
            return;
        };
        let url = &self.playing.station_url;
        if self.art.as_ref().is_none_or(|(station, _)| station != url) {
            let station = Station::new(&self.playing.station, url);
            let file = (!url.is_empty())
                .then(|| self.logos.get_cached_path(&station))
                .flatten();
            self.art = Some((url.clone(), file));
        }
        let art = self.art.as_ref().and_then(|(_, file)| file.clone());
        let track = Track::of(&self.playing, art);
        if self.shown.as_ref() != Some(&track) {
            controls.show(&track);
            self.shown = Some(track);
        }
    }
}

/// What the system asks, done on the window's thread as its own controls
/// do it
fn commands(ui: &App) -> Commands {
    let ui = ui.as_weak();
    Arc::new(move |command| {
        let _ = ui.upgrade_in_event_loop(move |ui| act(&ui, command));
    })
}

fn act(ui: &App, command: Command) {
    let active = MEDIA.with(|m| {
        m.borrow()
            .as_ref()
            .map(|m| lock(&m.state).phase().is_active())
    });
    let Some(active) = active else {
        return;
    };
    match command {
        Command::Play if !active => ui.invoke_play_or_stop(),
        Command::Pause | Command::Stop if active => ui.invoke_play_or_stop(),
        Command::PlayPause => ui.invoke_play_or_stop(),
        Command::Next => ui.invoke_step_favorite(true),
        Command::Previous => ui.invoke_step_favorite(false),
        Command::Raise => crate::tray::show_window(ui),
        Command::Quit => ui.invoke_quit_app(),
        Command::Play | Command::Pause | Command::Stop => {}
    }
}

/// Register with the system
fn start(ui: &App, commands: Commands) -> Result<Box<dyn Controls>, String> {
    #[cfg(target_os = "linux")]
    {
        let _ = ui;
        Ok(Box::new(mpris::Mpris::start(commands)?))
    }
    #[cfg(windows)]
    {
        Ok(Box::new(smtc::Smtc::start(ui, commands)?))
    }
    #[cfg(target_os = "macos")]
    {
        let _ = ui;
        Ok(Box::new(now_playing::NowPlaying::start(commands)?))
    }
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        let _ = (ui, commands);
        Err("not available on this system".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing(station: &str, title: &str, artist: &str, status: Status) -> Playing {
        Playing {
            station: station.into(),
            station_url: "https://radio.example/stream".into(),
            title: title.into(),
            artist: artist.into(),
            status,
        }
    }

    #[test]
    fn a_station_without_song_info_is_the_title() {
        let track = Track::of(&playing("Jazz FM", "", "", Status::Playing), None);
        assert_eq!(track.title, "Jazz FM");
        assert_eq!(track.artist, None);
        assert_eq!(track.album, None);
        assert_eq!(track.status, Status::Playing);
        assert!(track.can_play);
    }

    #[test]
    fn a_song_without_artist_has_the_station_as_artist() {
        let track = Track::of(
            &playing("Jazz FM", "News at Six", "", Status::Playing),
            None,
        );
        assert_eq!(track.title, "News at Six");
        assert_eq!(track.artist.as_deref(), Some("Jazz FM"));
        assert_eq!(track.album, None);
    }

    #[test]
    fn an_artist_and_title_keep_the_station_as_album() {
        let track = Track::of(
            &playing("Jazz FM", "So What", "Miles Davis", Status::Playing),
            None,
        );
        assert_eq!(track.title, "So What");
        assert_eq!(track.artist.as_deref(), Some("Miles Davis"));
        assert_eq!(track.album.as_deref(), Some("Jazz FM"));
    }

    #[test]
    fn stopped_shows_the_station_not_the_last_song() {
        let track = Track::of(
            &playing("Jazz FM", "So What", "Miles Davis", Status::Stopped),
            Some(PathBuf::from("/cache/logo.png")),
        );
        assert_eq!(track.title, "Jazz FM");
        assert_eq!(track.artist, None);
        assert_eq!(track.art, Some(PathBuf::from("/cache/logo.png")));
        assert!(track.can_play);
    }

    #[test]
    fn a_station_without_a_name_is_called_radiotrope() {
        let track = Track::of(&playing("  ", "", "", Status::Playing), None);
        assert_eq!(track.title, crate::player_ui::UNNAMED_STATION);
    }

    #[test]
    fn nothing_played_yet_has_nothing_to_show_or_play() {
        let track = Track::of(&Playing::default(), Some(PathBuf::from("/x.png")));
        assert_eq!(track, Track::default());
    }

    #[test]
    fn a_station_on_its_way_counts_as_playing() {
        for phase in [
            PlayerPhase::Connecting,
            PlayerPhase::Buffering,
            PlayerPhase::Reconnecting,
            PlayerPhase::Playing,
        ] {
            assert_eq!(Status::of(phase), Status::Playing, "{phase:?}");
        }
        assert_eq!(Status::of(PlayerPhase::Paused), Status::Paused);
        for phase in [PlayerPhase::Idle, PlayerPhase::Stopped, PlayerPhase::Failed] {
            assert_eq!(Status::of(phase), Status::Stopped, "{phase:?}");
        }
    }

    #[test]
    fn the_controller_state_is_copied_and_compared_as_shown() {
        let mut state = AppSnapshot {
            station_name: Some("Jazz FM".into()),
            station_url: Some("https://radio.example/stream".into()),
            title: "So What".into(),
            artist: "Miles Davis".into(),
            playback: radiotrope::audio::PlaybackState::Playing,
            ..AppSnapshot::default()
        };
        let p = Playing::of(&state);
        assert_eq!(
            p,
            playing("Jazz FM", "So What", "Miles Davis", Status::Playing)
        );
        assert!(p.same_as(&state));
        state.title = "Freddie Freeloader".into();
        assert!(!p.same_as(&state));
        state.title = "So What".into();
        state.playback = radiotrope::audio::PlaybackState::Stopped;
        assert!(!p.same_as(&state));
    }
}
