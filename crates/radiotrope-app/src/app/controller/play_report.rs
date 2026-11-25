//! Stations > Report Play Count: the one place a play is counted in the
//! Radiotrope directory, which orders its lists by plays.
//!
//! The controller tells [`PlayReporter`] each time a station it was asked
//! to start (a pick in the window, Next or Previous, the tray, a phone or
//! an agent, a scheduled entry) first plays; reconnects and retries of the
//! same start are not new plays. The reporter counts it only with Report
//! Play Count allowed, and only for a station from the directory: a
//! favorite saved from it, or one just picked from it (Search Stations, a
//! phone or an agent by its directory id). A custom URL is never counted.
//! The server counts a station once a day per listener anyway.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use radiotrope_app::data::favorites::FavoritesManager;
use radiotrope_app::data::types::Station;
use radiotrope_app::providers::radiotrope::{RadiotropeProvider, PROVIDER_ID};
use radiotrope_app::providers::StationProvider;

/// Told the stream URL of a station that just started playing
pub type PlayReporter = Arc<dyn Fn(&str) + Send + Sync>;

/// How many stations picked from the directory are remembered
const PICKED_KEPT: usize = 32;

/// Directory stations picked by URL that may not be favorites, newest last
static PICKED: Mutex<VecDeque<Station>> = Mutex::new(VecDeque::new());

/// A station from the directory is about to be played by its URL (from
/// Search Stations, or a phone or an agent by its directory id): its play
/// is counted once it plays
pub fn picked_from_directory(station: &Station) {
    if !from_directory(station) {
        return;
    }
    let mut picked = PICKED.lock().unwrap_or_else(|e| e.into_inner());
    picked.retain(|s| s.url != station.url);
    picked.push_back(station.clone());
    while picked.len() > PICKED_KEPT {
        picked.pop_front();
    }
}

/// The directory's station at `url`, to count its play: `None` when
/// counting isn't allowed, or for a station not from the directory
pub(crate) fn play_to_report(
    url: &str,
    allowed: bool,
    favorites: &FavoritesManager,
) -> Option<Station> {
    if !allowed {
        return None;
    }
    let picked = PICKED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .rev()
        .find(|s| s.url == url)
        .cloned();
    picked
        .or_else(|| favorites.get_by_url(url).map(|f| f.station.clone()))
        .filter(from_directory)
}

/// The reporter the controller is given: counts a play of a directory
/// station in the background with Report Play Count allowed
pub fn reporter(favorites: Arc<Mutex<FavoritesManager>>) -> PlayReporter {
    Arc::new(move |url| {
        let station = {
            let favorites = favorites.lock().unwrap_or_else(|e| e.into_inner());
            play_to_report(url, crate::view_options::report_plays(), &favorites)
        };
        if let Some(station) = station {
            send(station);
        }
    })
}

fn from_directory(station: &Station) -> bool {
    station.provider == PROVIDER_ID
        && station
            .provider_id
            .as_deref()
            .is_some_and(|id| !id.is_empty())
}

/// Count the play; nobody waits on it, and a failure costs only the count
fn send(station: Station) {
    static DIRECTORY: OnceLock<Option<RadiotropeProvider>> = OnceLock::new();
    let _ = std::thread::Builder::new()
        .name("report-play".into())
        .spawn(move || {
            if let Some(directory) = DIRECTORY.get_or_init(|| RadiotropeProvider::new().ok()) {
                let _ = directory.report_click(&station);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use radiotrope_app::data::types::Favorite;

    fn directory_station(name: &str, url: &str, id: &str) -> Station {
        Station::new(name, url).with_provider(PROVIDER_ID, Some(id.to_string()))
    }

    #[test]
    fn a_favorite_from_the_directory_is_counted_only_when_allowed() {
        let mut favorites = FavoritesManager::new();
        let url = "http://report.test/favorite";
        favorites
            .add(Favorite::from_station(directory_station(
                "Jazz FM", url, "jazz-fm",
            )))
            .unwrap();

        let counted = play_to_report(url, true, &favorites).expect("counted when allowed");
        assert_eq!(counted.provider_id.as_deref(), Some("jazz-fm"));
        assert!(
            play_to_report(url, false, &favorites).is_none(),
            "not without a yes"
        );
    }

    #[test]
    fn a_custom_url_is_never_counted() {
        let mut favorites = FavoritesManager::new();
        let url = "http://report.test/custom";
        favorites.add(Favorite::new("My Stream", url)).unwrap();
        assert!(play_to_report(url, true, &favorites).is_none());
        assert!(play_to_report("http://report.test/unknown", true, &favorites).is_none());

        // Picking one that isn't from the directory doesn't make it one
        picked_from_directory(&Station::new("Typed In", "http://report.test/typed"));
        assert!(play_to_report("http://report.test/typed", true, &favorites).is_none());
    }

    #[test]
    fn a_station_picked_from_the_directory_is_counted() {
        let favorites = FavoritesManager::new();
        let url = "http://report.test/picked";
        assert!(play_to_report(url, true, &favorites).is_none());
        picked_from_directory(&directory_station("Picked FM", url, "picked-fm"));
        assert!(play_to_report(url, true, &favorites).is_some());
        assert!(play_to_report(url, false, &favorites).is_none());
    }

    /// The URLs reported, in order
    type Reports = Arc<Mutex<Vec<String>>>;
    type State = Arc<Mutex<crate::app::state::AppSnapshot>>;

    /// A controller with no engine, its reports noted
    fn controller() -> (super::super::AppController, State, Reports) {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (analysis_tx, _) = crossbeam_channel::unbounded();
        let (stats_tx, _) = crossbeam_channel::unbounded();
        let state = Arc::new(Mutex::new(crate::app::state::AppSnapshot::default()));
        let reports = Arc::new(Mutex::new(Vec::new()));
        let controller =
            super::super::AppController::new(cmd_rx, cmd_tx, state.clone(), analysis_tx, stats_tx)
                .with_play_reporter({
                    let reports = reports.clone();
                    Arc::new(move |url: &str| reports.lock().unwrap().push(url.to_string()))
                });
        (controller, state, reports)
    }

    /// The station the controller is on starts playing (or plays again
    /// after a reconnect)
    fn plays(
        controller: &mut super::super::AppController,
        state: &Mutex<crate::app::state::AppSnapshot>,
    ) {
        state.lock().unwrap().is_resolving = false;
        controller.handle_engine_event(radiotrope::audio::EngineEvent {
            stream: None,
            event: radiotrope::audio::AudioEvent::Playing(radiotrope::audio::CodecInfo {
                codec_name: "MP3".into(),
                channels: 2,
                sample_rate: 44_100,
                bits_per_sample: None,
                bitrate: None,
            }),
        });
    }

    fn play(url: &str) -> crate::app::state::AppCommand {
        crate::app::state::AppCommand::Play {
            url: url.into(),
            backups: Vec::new(),
            name: None,
            logo_url: None,
            country: None,
            taken: None,
        }
    }

    /// Nothing listens there
    const ONE: &str = "http://127.0.0.1:1/one.mp3";
    const TWO: &str = "http://127.0.0.1:1/two.mp3";

    #[test]
    fn each_start_is_reported_once_when_it_plays() {
        let (mut controller, state, reports) = controller();
        controller.handle_command(play(ONE));
        assert!(reports.lock().unwrap().is_empty(), "not before it plays");
        plays(&mut controller, &state);
        assert_eq!(*reports.lock().unwrap(), [ONE]);
        // The stream reconnects and plays again: the same play
        plays(&mut controller, &state);
        assert_eq!(reports.lock().unwrap().len(), 1);

        // Started again by the user: a new play
        controller.handle_command(play(ONE));
        plays(&mut controller, &state);
        assert_eq!(*reports.lock().unwrap(), [ONE, ONE]);
    }

    #[test]
    fn next_and_previous_report_the_station_stepped_to_once() {
        let (mut controller, state, reports) = controller();
        let step = |url: &str| crate::app::state::AppCommand::Step {
            url: url.into(),
            backups: Vec::new(),
            name: None,
            logo_url: None,
            country: None,
        };
        // Stepped past ONE to TWO: only TWO plays
        controller.handle_command(step(ONE));
        controller.handle_command(step(TWO));
        controller.settling = None;
        plays(&mut controller, &state);
        assert_eq!(*reports.lock().unwrap(), [TWO]);
        plays(&mut controller, &state);
        assert_eq!(*reports.lock().unwrap(), [TWO]);
    }

    #[test]
    fn a_restart_after_an_outage_or_falling_behind_is_no_new_play() {
        let (mut controller, state, reports) = controller();
        controller.handle_command(play(ONE));
        plays(&mut controller, &state);
        // Started again to catch up after the output was gone
        controller.handle_engine_event(radiotrope::audio::EngineEvent {
            stream: None,
            event: radiotrope::audio::AudioEvent::FellBehind,
        });
        plays(&mut controller, &state);
        assert_eq!(reports.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_station_stopped_before_it_plays_is_not_reported() {
        let (mut controller, state, reports) = controller();
        controller.handle_command(play(ONE));
        controller.handle_command(crate::app::state::AppCommand::Stop);
        plays(&mut controller, &state);
        assert!(reports.lock().unwrap().is_empty());
    }

    // Search Stations notes the pick and plays it; nothing else counts it,
    // so one play is one report
    #[test]
    fn a_search_pick_is_reported_once() {
        let url = "http://127.0.0.1:1/searched.mp3";
        picked_from_directory(&directory_station("Searched FM", url, "searched-fm"));
        let (mut controller, state, reports) = controller();
        controller.handle_command(play(url));
        plays(&mut controller, &state);
        let reported = reports.lock().unwrap().clone();
        assert_eq!(reported, [url]);
        let favorites = FavoritesManager::new();
        assert!(reported
            .iter()
            .all(|url| play_to_report(url, true, &favorites).is_some()));
    }
}
