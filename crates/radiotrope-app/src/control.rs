//! Driving the player from outside the window
//!
//! Agents (over MCP) and, next, phones (over the Remote API) reach the
//! player through a [`Control`]: it sends the controller's commands, reads
//! the shared state and favorites, and waits for outcomes the same way for
//! every caller, so an agent and a phone always get the same behaviour.
//! What a caller shows (the tools' texts, the API's JSON) stays with it.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, TrySendError};

use radiotrope::audio::PlaybackState;
use radiotrope_app::data::favorites::{
    FavoritesManager, ImportMode, ImportOutcome, PlayMetadata, SaveJob,
};
use radiotrope_app::data::recordings;
use radiotrope_app::data::schedule::{self, Action, ClockTime, Days, End, Entry, ScheduledStation};
use radiotrope_app::data::types::{
    name_from_url, url_to_id, Favorite, FavoriteSort, FavoriteUpdate, Station, StationStats,
};
use radiotrope_app::providers::types::Category;
use radiotrope_app::providers::{CategoryType, ProviderRegistry, StationFilter};

use crate::app::controller::CoverSource;
use crate::app::state::{AppCommand, AppSnapshot};

/// How long recording changes wait for the recorder to answer
const RECORDING_WAIT: Duration = Duration::from_secs(3);
/// How often waits look at the player's state
const POLL: Duration = Duration::from_millis(100);
/// How long schedule changes wait for the player to answer
const SCHEDULE_WAIT: Duration = Duration::from_secs(3);
/// Adding stops at this many favorites
pub const MAX_FAVORITES: usize = 1000;
/// Longest station name or country a caller may give, in characters
pub const MAX_NAME_CHARS: usize = 512;
/// Longest stream or logo URL a caller may give, in characters
pub const MAX_URL_CHARS: usize = 2048;

/// Why something couldn't be done. Callers word the ones that name an id,
/// so each can say where its ids come from.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// The player's command queue is full
    Busy,
    /// The player has quit
    Stopped,
    /// The player went away before taking up a Play
    NotTaken,
    NoFavorite(String),
    NoStation(String),
    NoEntry(u64),
    /// Adding would pass [`MAX_FAVORITES`]
    TooManyFavorites,
    /// Anything else, in words for the user
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Busy => f.write_str("The player is busy; try again in a moment"),
            Error::Stopped | Error::NotTaken => f.write_str("The player has stopped"),
            Error::NoFavorite(id) => write!(f, "No favorite with id {id}"),
            Error::NoStation(id) => write!(f, "No station with id {id}"),
            Error::NoEntry(id) => write!(f, "No schedule entry with id {id}"),
            Error::TooManyFavorites => write!(f, "There are {MAX_FAVORITES} favorites already"),
            Error::Failed(text) => f.write_str(text),
        }
    }
}

impl From<String> for Error {
    fn from(text: String) -> Self {
        Error::Failed(text)
    }
}

/// How a Play went
#[derive(Debug)]
pub enum Played {
    /// Not waited for
    Starting,
    /// Still resolving or buffering when the wait ran out
    StillConnecting,
    /// Playing; the state as it started
    Playing(Box<AppSnapshot>),
    /// Taken up, but another station was started after it
    Superseded,
}

/// How starting a recording went
#[derive(Debug)]
pub enum RecordingStart {
    Already(PathBuf),
    Started(PathBuf),
    /// The recorder hasn't answered yet
    Starting,
}

/// How stopping a recording went
#[derive(Debug)]
pub enum RecordingStop {
    NotRecording,
    /// The recorder's own words, e.g. where the file was saved
    Saved(String),
    /// No word from the recorder yet; the file it was writing
    Stopping(PathBuf),
}

/// A station from the directory, and its favorite id if it is one
#[derive(Debug)]
pub struct Found {
    pub station: Station,
    pub favorite_id: Option<String>,
}

/// One page of search results
#[derive(Debug)]
pub struct SearchPage {
    pub stations: Vec<Found>,
    /// More may follow at offset + stations.len()
    pub has_more: bool,
}

/// What the controller needs from outside callers, shared by all of them
#[derive(Clone)]
pub struct Control {
    cmd_tx: Sender<AppCommand>,
    state: Arc<Mutex<AppSnapshot>>,
    favorites: Arc<Mutex<FavoritesManager>>,
    /// Built on first search; shares its HTTP client and server list
    providers: Arc<Mutex<Option<Arc<ProviderRegistry>>>>,
    /// Where favorites are saved; `None` is the usual data folder
    favorites_file: Option<PathBuf>,
    /// A recording's cover art, as the window and the schedule get it
    cover_source: Option<CoverSource>,
}

impl Control {
    pub fn new(
        cmd_tx: Sender<AppCommand>,
        state: Arc<Mutex<AppSnapshot>>,
        favorites: Arc<Mutex<FavoritesManager>>,
    ) -> Self {
        Self {
            cmd_tx,
            state,
            favorites,
            providers: Arc::new(Mutex::new(None)),
            favorites_file: None,
            cover_source: None,
        }
    }

    /// Where a recording started here gets its cover art
    pub fn with_cover_source(mut self, cover: CoverSource) -> Self {
        self.cover_source = Some(cover);
        self
    }

    /// Search these providers and save favorites to this file (for tests)
    #[cfg(test)]
    pub fn with_test_setup(mut self, providers: ProviderRegistry, favorites_file: PathBuf) -> Self {
        self.providers = Arc::new(Mutex::new(Some(Arc::new(providers))));
        self.favorites_file = Some(favorites_file);
        self
    }

    pub fn snapshot(&self) -> AppSnapshot {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Hand `cmd` to the player without waiting: a player whose queue is
    /// full (a recording finishing, a stuck audio device) says so instead
    /// of holding up every caller
    pub fn send(&self, cmd: AppCommand) -> Result<(), Error> {
        match self.cmd_tx.try_send(cmd) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(Error::Busy),
            Err(TrySendError::Disconnected(_)) => Err(Error::Stopped),
        }
    }

    // -- Playback -----------------------------------------------------------

    /// Start a station and wait up to `wait` for it to play or fail.
    /// `sent` runs once the player has the command.
    pub async fn play(
        &self,
        url: String,
        name: Option<String>,
        logo_url: Option<String>,
        country: Option<String>,
        wait: Duration,
        sent: impl FnOnce(),
    ) -> Result<Played, Error> {
        // The player says which station number ours became, so a station
        // started by the window or another caller just before or after
        // isn't taken for ours
        let (taken, ours) = tokio::sync::oneshot::channel();
        // The logo and country go with it, so the header shows them as it
        // does for a station picked in the UI
        self.send(AppCommand::Play {
            url,
            name,
            logo_url,
            country,
            taken: Some(taken),
        })?;
        sent();
        if wait.is_zero() {
            return Ok(Played::Starting);
        }

        let deadline = tokio::time::Instant::now() + wait;
        let seq = match tokio::time::timeout_at(deadline, ours).await {
            Ok(Ok(seq)) => seq,
            // The player went away without taking it
            Ok(Err(_)) => return Err(Error::NotTaken),
            Err(_) => return Ok(Played::StillConnecting),
        };
        loop {
            let s = self.snapshot();
            if s.play_seq != seq {
                return Ok(Played::Superseded);
            }
            if !s.is_resolving {
                if s.playback == PlaybackState::Playing {
                    return Ok(Played::Playing(Box::new(s)));
                }
                if let Some(e) = &s.last_error {
                    return Err(Error::Failed(e.clone()));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(Played::StillConnecting);
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// What the favorites know about a URL about to be played: its name,
    /// logo and country, as the window fills them in
    pub async fn resolve_play(
        &self,
        url: String,
        name: Option<String>,
    ) -> Result<PlayMetadata, Error> {
        let request = PlayMetadata {
            url,
            name,
            ..Default::default()
        };
        self.with_favorites(move |_, favorites| favorites.resolve_play(request))
            .await
    }

    /// A station by its directory id, looked up off the async thread. The
    /// play is counted in the directory, as the GUI does (not with
    /// Stations > Report Plays off); nobody waits on it.
    pub async fn station_to_play(&self, id: String) -> Result<Station, Error> {
        let station = self.station(id).await?;
        if !crate::view_options::report_plays() {
            return Ok(station);
        }
        let control = self.clone();
        let clicked = station.clone();
        tokio::task::spawn_blocking(move || {
            if let Ok(providers) = control.providers() {
                if let Some(p) = providers.get(&clicked.provider) {
                    let _ = p.report_click(&clicked);
                }
            }
        });
        Ok(station)
    }

    /// A station by its directory id, looked up off the async thread
    pub async fn station(&self, id: String) -> Result<Station, Error> {
        let control = self.clone();
        tokio::task::spawn_blocking(move || control.find_station(&id))
            .await
            .map_err(|e| Error::Failed(format!("Can't look up the station: {e}")))?
    }

    /// A favorite's URL, name, logo and country, to play it
    pub async fn favorite_to_play(
        &self,
        id: String,
    ) -> Result<(String, String, Option<String>, Option<String>), Error> {
        self.with_favorites(move |_, favorites| {
            let fav = favorites.get(&id).ok_or(Error::NoFavorite(id))?;
            Ok((
                fav.url().to_string(),
                fav.name().to_string(),
                fav.station.logo_url.clone(),
                fav.station.country.clone(),
            ))
        })
        .await?
    }

    pub fn stop(&self) -> Result<(), Error> {
        self.send(AppCommand::Stop)
    }

    /// Set the volume (0 to 1); unmutes above 0. Tells whether it was muted.
    pub fn set_volume(&self, volume: f32) -> Result<bool, Error> {
        let volume = volume.clamp(0.0, 1.0);
        let was_muted = self.snapshot().is_muted;
        self.send(AppCommand::SetVolume(volume))?;
        // Set it in the shared state now so the GUI shows it at once
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.volume = volume;
        if volume > 0.0 {
            s.is_muted = false;
        }
        Ok(was_muted)
    }

    pub fn set_muted(&self, muted: bool) -> Result<(), Error> {
        self.send(if muted {
            AppCommand::Mute
        } else {
            AppCommand::Unmute
        })
    }

    /// Use this accent ("#rrggbb"); it is saved with the settings
    pub fn set_accent(&self, hex: String) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .accent_color = Some(hex);
    }

    /// Change the recording settings the next recording starts with; the
    /// window saves them
    pub fn set_recording_settings(&self, change: &crate::app::state::RecordingSettingsChange) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        change.apply_to_setup(&mut s.recording_setup);
    }

    // -- Directory ----------------------------------------------------------

    /// One page of the directory's stations matching `filter`
    pub async fn search(
        &self,
        filter: StationFilter,
        limit: usize,
        offset: usize,
    ) -> Result<SearchPage, Error> {
        // Blocking HTTP (reqwest's blocking client, which must not even be
        // built inside the async runtime): off it, so other calls keep flowing
        let control = self.clone();
        tokio::task::spawn_blocking(move || {
            let providers = control.providers()?;
            let mut stations = Vec::new();
            let mut has_more = false;
            let mut failed = None;
            for id in providers.list_ids() {
                let Some(provider) = providers.get(id) else {
                    continue;
                };
                match provider.search_filtered(&filter, limit, offset) {
                    Ok(r) => {
                        has_more |= r.has_more;
                        stations.extend(r.stations);
                    }
                    Err(e) => failed = Some(e),
                }
            }
            if let Some(e) = failed.filter(|_| stations.is_empty()) {
                return Err(Error::Failed(format!("Can't search: {e}")));
            }
            let favorites = control.favorites();
            let stations = stations
                .into_iter()
                .map(|station| Found {
                    favorite_id: favorites
                        .is_favorite(&station.url)
                        .then(|| url_to_id(&station.url)),
                    station,
                })
                .collect();
            Ok(SearchPage { stations, has_more })
        })
        .await
        .map_err(|e| Error::Failed(format!("Can't search: {e}")))?
    }

    /// The directory's genres, countries or languages: those whose name
    /// holds `filter` (or whose code is it), largest first, at most `limit`
    pub async fn categories(
        &self,
        wanted: CategoryType,
        filter: Option<String>,
        limit: usize,
    ) -> Result<Vec<Category>, Error> {
        let control = self.clone();
        let all = tokio::task::spawn_blocking(move || {
            let providers = control.providers()?;
            let mut all = Vec::new();
            let mut failed = None;
            for id in providers.list_ids() {
                match providers.get(id).map(|p| p.browse_categories()) {
                    Some(Ok(c)) => all.extend(c),
                    Some(Err(e)) => failed = Some(e),
                    None => {}
                }
            }
            match failed {
                Some(e) if all.is_empty() => {
                    Err(Error::Failed(format!("Can't load the list: {e}")))
                }
                _ => Ok(all),
            }
        })
        .await
        .map_err(|e| Error::Failed(format!("Can't load the list: {e}")))??;

        let filter = filter
            .map(|f| f.trim().to_lowercase())
            .filter(|f| !f.is_empty());
        let mut categories: Vec<Category> = all
            .into_iter()
            .filter(|c| c.category_type == wanted)
            .filter(|c| {
                filter.as_deref().is_none_or(|f| {
                    c.name.to_lowercase().contains(f)
                        || c.code
                            .as_deref()
                            .is_some_and(|code| code.eq_ignore_ascii_case(f))
                })
            })
            .collect();
        categories.sort_by_key(|c| std::cmp::Reverse(c.station_count));
        categories.truncate(limit);
        Ok(categories)
    }

    // -- Favorites ----------------------------------------------------------

    /// The favorites in the user's own order
    pub async fn favorites_in_order(&self) -> Result<Vec<Favorite>, Error> {
        self.with_favorites(|_, favorites| {
            favorites
                .sorted(FavoriteSort::Manual)
                .into_iter()
                .cloned()
                .collect()
        })
        .await
    }

    /// The favorites in the user's order, each with its plays and
    /// listening time
    pub async fn favorites_with_stats(&self) -> Result<Vec<(Favorite, StationStats)>, Error> {
        self.with_favorites(|_, favorites| {
            favorites
                .sorted(FavoriteSort::Manual)
                .into_iter()
                .map(|f| (f.clone(), favorites.stats(&f.id())))
                .collect()
        })
        .await
    }

    /// Take in a list of favorites from a phone or a file (see
    /// [`FavoritesManager::import_list`]); with `preview` only tells what
    /// would change. Also tells whether an import can now be undone.
    pub async fn import_favorites(
        &self,
        mode: ImportMode,
        list: Vec<Favorite>,
        preview: bool,
    ) -> Result<(ImportOutcome, bool), Error> {
        let ((outcome, can_undo), saved) = self
            .change_favorites(move |favorites| {
                let outcome = favorites
                    .import_list(mode, &list, MAX_FAVORITES, preview)
                    .map_err(|e| Error::Failed(e.to_string()))?;
                Ok((outcome, favorites.can_undo_import()))
            })
            .await?;
        if !preview && outcome.changes() {
            saved.map_err(|e| Error::Failed(format!("Can't save: {e}")))?;
        }
        Ok((outcome, can_undo))
    }

    /// Put back the favorites from before the last import; tells how many
    /// there are now, or `None` when there is nothing to undo
    pub async fn undo_favorites_import(&self) -> Result<Option<usize>, Error> {
        let (count, saved) = self
            .change_favorites(move |favorites| {
                if !favorites.can_undo_import() {
                    return Ok(None);
                }
                favorites
                    .undo_import()
                    .map(Some)
                    .map_err(|e| Error::Failed(e.to_string()))
            })
            .await?;
        if count.is_some() {
            saved.map_err(|e| Error::Failed(format!("Can't save: {e}")))?;
        }
        Ok(count)
    }

    /// The plays and listening time of the favorite with this id
    pub async fn favorite_stats(&self, id: String) -> StationStats {
        self.with_favorites(move |_, favorites| favorites.stats(&id))
            .await
            .unwrap_or_default()
    }

    /// Counts the changes to the favorites, so callers can tell when to
    /// list them again
    pub async fn favorites_generation(&self) -> u64 {
        self.with_favorites(|_, favorites| favorites.generation())
            .await
            .unwrap_or_default()
    }

    /// The favorite id of a station URL, if it is a favorite
    pub async fn favorite_id(&self, url: String) -> Option<String> {
        self.with_favorites(move |_, favorites| {
            favorites.is_favorite(&url).then(|| url_to_id(&url))
        })
        .await
        .unwrap_or_default()
    }

    /// Save a station to the favorites (a URL already there is updated)
    pub async fn add_favorite(&self, fav: Favorite) -> Result<(), Error> {
        let ((), saved) = self
            .change_favorites(move |favorites| {
                if favorites.count() >= MAX_FAVORITES && !favorites.is_favorite(fav.url()) {
                    return Err(Error::TooManyFavorites);
                }
                favorites.add(fav).map_err(|e| Error::Failed(e.to_string()))
            })
            .await?;
        saved.map_err(|e| Error::Failed(format!("Can't save: {e}")))
    }

    /// Remove the favorite with this id; tells which it was
    pub async fn remove_favorite(&self, id: String) -> Result<Favorite, Error> {
        let (removed, saved) = self
            .change_favorites(move |favorites| {
                if favorites.get(&id).is_none() {
                    return Err(Error::NoFavorite(id));
                }
                favorites
                    .remove(&id)
                    .map_err(|e| Error::Failed(e.to_string()))
            })
            .await?;
        saved.map_err(|e| Error::Failed(format!("Removed, but can't save: {e}")))?;
        Ok(removed)
    }

    /// Change a favorite's name, stream, logo and country, as the window's
    /// Edit Favorite does; tells what it was and what it is now
    pub async fn edit_favorite(
        &self,
        id: String,
        edit: FavoriteEdit,
    ) -> Result<(Favorite, Favorite), Error> {
        let (edited, saved) = self
            .change_favorites(move |favorites| {
                let old = favorites
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| Error::NoFavorite(id.clone()))?;
                let mut update = FavoriteUpdate::new()
                    .name(edit.name.clone())
                    .url(edit.url.clone());
                update.logo_url = Some(edit.logo_url.clone().filter(|l| !l.is_empty()));
                update.country = Some(edit.country.clone().filter(|c| !c.is_empty()));
                if let Err(e) = favorites.update(&id, update) {
                    return Err(Error::Failed(
                        if url_to_id(&edit.url) != id && favorites.is_favorite(&edit.url) {
                            "Another favorite already has this stream URL".into()
                        } else {
                            e.to_string()
                        },
                    ));
                }
                let new = favorites
                    .get(&url_to_id(&edit.url))
                    .cloned()
                    .ok_or_else(|| Error::NoFavorite(url_to_id(&edit.url)))?;
                Ok((old, new))
            })
            .await?;
        saved.map_err(|e| Error::Failed(format!("Changed, but can't save: {e}")))?;
        Ok(edited)
    }

    /// Put the favorites in this order; any left out keep theirs after
    /// the ones given
    pub async fn reorder_favorites(&self, ids: Vec<String>) -> Result<(), Error> {
        let ((), saved) = self
            .change_favorites(move |favorites| {
                if let Some(missing) = ids.iter().find(|id| favorites.get(id).is_none()) {
                    return Err(Error::NoFavorite(missing.clone()));
                }
                let mut order: Vec<String> = Vec::with_capacity(favorites.count());
                for id in &ids {
                    if !order.contains(id) {
                        order.push(id.clone());
                    }
                }
                for fav in favorites.sorted(FavoriteSort::Manual) {
                    let id = fav.id();
                    if !order.contains(&id) {
                        order.push(id);
                    }
                }
                let refs: Vec<&str> = order.iter().map(String::as_str).collect();
                favorites
                    .reorder(&refs)
                    .map_err(|e| Error::Failed(e.to_string()))
            })
            .await?;
        saved.map_err(|e| Error::Failed(format!("Reordered, but can't save: {e}")))
    }

    // -- Recording ----------------------------------------------------------

    /// Record the playing station, with the window's recording settings.
    /// `sent` runs once the player has the command.
    pub async fn start_recording(&self, sent: impl FnOnce()) -> Result<RecordingStart, Error> {
        let s = self.snapshot();
        if let Some(r) = &s.recording {
            return Ok(RecordingStart::Already(r.path.clone()));
        }
        if s.playback != PlaybackState::Playing {
            return Err(Error::Failed(
                "Nothing is playing; start a station first".into(),
            ));
        }
        let notice_before = s.recording_notice.as_ref().map(|n| n.seq);
        let cover = self.cover_for(&s).await;
        // The settings as the window has them, not as the file may be
        // half-way through a save
        let setup = &s.recording_setup;
        self.send(AppCommand::StartRecording {
            folder: recordings::folder(setup.dir.as_deref()),
            format: setup.format,
            bitrate: setup.bitrate,
            // As in the window: the switch only counts with the EQ on
            with_eq: setup.with_eq && s.eq_enabled,
            cover,
        })?;
        sent();
        let deadline = Instant::now() + RECORDING_WAIT;
        while Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
            let s = self.snapshot();
            if let Some(r) = &s.recording {
                return Ok(RecordingStart::Started(r.path.clone()));
            }
            if let Some(n) = s.recording_notice.filter(|n| Some(n.seq) != notice_before) {
                if n.is_error {
                    return Err(Error::Failed(n.text));
                }
            }
        }
        Ok(RecordingStart::Starting)
    }

    /// The playing station's cached logo as cover art, read and encoded off
    /// the async task. None when it isn't cached (as in the window: a
    /// recording doesn't wait for a logo).
    async fn cover_for(&self, s: &AppSnapshot) -> Option<Vec<u8>> {
        let cover = self.cover_source.clone()?;
        let station = Station::new(
            s.station_name.as_deref().unwrap_or_default(),
            s.station_url.as_deref()?,
        );
        tokio::task::spawn_blocking(move || cover(&station))
            .await
            .ok()
            .flatten()
    }

    /// Stop the recording and save it. `sent` runs once the player has the
    /// command.
    pub async fn stop_recording(&self, sent: impl FnOnce()) -> Result<RecordingStop, Error> {
        let s = self.snapshot();
        let Some(recording) = s.recording else {
            return Ok(RecordingStop::NotRecording);
        };
        let notice_before = s.recording_notice.as_ref().map(|n| n.seq);
        self.send(AppCommand::StopRecording)?;
        sent();
        let deadline = Instant::now() + RECORDING_WAIT;
        while Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
            let s = self.snapshot();
            if let Some(n) = s.recording_notice.filter(|n| Some(n.seq) != notice_before) {
                return if n.is_error {
                    Err(Error::Failed(n.text))
                } else {
                    Ok(RecordingStop::Saved(n.text))
                };
            }
            if s.recording.is_none() {
                break;
            }
        }
        Ok(RecordingStop::Stopping(recording.path))
    }

    // -- Sleep Timer and Scheduler ------------------------------------------

    /// Stop playback `minutes` from now, or turn the timer off with `None`
    pub fn set_sleep_timer(&self, minutes: Option<u32>, fade: bool) -> Result<(), Error> {
        self.send(AppCommand::SetSleepTimer { minutes, fade })
    }

    /// The scheduled entry with this id
    pub fn entry(&self, id: u64) -> Result<Entry, Error> {
        self.snapshot()
            .schedule
            .into_iter()
            .find(|e| e.id == id)
            .ok_or(Error::NoEntry(id))
    }

    /// Add an entry (id 0) or replace the one with its id; tells the id
    pub async fn save_entry(&self, entry: Entry) -> Result<u64, Error> {
        self.ask_schedule(|reply| AppCommand::SaveScheduleEntry {
            entry,
            reply: Some(reply),
        })
        .await
    }

    pub async fn remove_entry(&self, id: u64) -> Result<(), Error> {
        self.ask_schedule(|reply| AppCommand::RemoveScheduleEntry {
            id,
            reply: Some(reply),
        })
        .await
    }

    pub async fn set_entry_enabled(&self, id: u64, enabled: bool) -> Result<(), Error> {
        self.ask_schedule(|reply| AppCommand::SetScheduleEntryEnabled {
            id,
            enabled,
            reply: Some(reply),
        })
        .await
    }

    /// The station a schedule entry is for: a favorite (by id), or a URL
    /// with an optional name. Callers check that exactly one is given.
    pub async fn scheduled_station(
        &self,
        favorite_id: Option<String>,
        url: Option<String>,
        name: Option<String>,
    ) -> Result<ScheduledStation, Error> {
        if let Some(id) = favorite_id {
            return self
                .with_favorites(move |_, favorites| {
                    let fav = favorites.get(&id).ok_or(Error::NoFavorite(id))?;
                    Ok(ScheduledStation {
                        name: fav.name().to_string(),
                        url: fav.url().to_string(),
                        logo_url: fav.station.logo_url.clone(),
                        country: fav.station.country.clone(),
                    })
                })
                .await?;
        }
        let url = url.unwrap_or_default();
        let name = match name.filter(|n| !n.is_empty()) {
            Some(name) => name,
            None => name_from_url(&url),
        };
        // A favorite's logo and country come along
        let lookup = url.clone();
        let (logo_url, country) = self
            .with_favorites(move |_, favorites| {
                favorites
                    .get_by_url(&lookup)
                    .map(|f| (f.station.logo_url.clone(), f.station.country.clone()))
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default();
        Ok(ScheduledStation {
            name,
            url,
            logo_url,
            country,
        })
    }

    /// A Scheduler entry from what a caller asked for, checked as the
    /// window checks its form (the player then checks for clashes)
    pub async fn build_entry(&self, request: EntryRequest) -> Result<Entry, Error> {
        let start = ClockTime::parse(&request.start)
            .ok_or_else(|| format!("start must be a time like 07:30, not {:?}", request.start))?;
        let days = parse_days(&request.days)?;
        let date = match request
            .date
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            Some(text) => Some(
                chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
                    .map_err(|_| format!("date must be YYYY-MM-DD, not {text:?}"))?,
            ),
            None => None,
        };
        let end = match (request.end_after_minutes, request.end_at.as_deref()) {
            (Some(_), Some(_)) => {
                return Err("Give end_after_minutes or end_at, not both"
                    .to_string()
                    .into())
            }
            (Some(minutes), None) => {
                let minutes = minutes.round();
                if !(1.0..=f64::from(schedule::MAX_MINUTES)).contains(&minutes) {
                    return Err(format!(
                        "end_after_minutes must be from 1 to {}",
                        schedule::MAX_MINUTES
                    )
                    .into());
                }
                End::After {
                    minutes: minutes as u32,
                }
            }
            (None, Some(text)) => End::At {
                time: ClockTime::parse(text)
                    .ok_or_else(|| format!("end_at must be a time like 22:00, not {text:?}"))?,
            },
            (None, None) => End::Never,
        };
        let volume = match request.volume {
            Some(v) if !(0.0..=100.0).contains(&v) => {
                return Err("volume must be from 0 to 100".to_string().into())
            }
            Some(v) => Some((v / 100.0) as f32),
            None => None,
        };
        let station = if request.action == Action::Stop {
            None
        } else {
            Some(self.entry_station(&request).await?)
        };
        Ok(Entry {
            id: 0,
            enabled: true,
            action: request.action,
            station,
            start,
            days,
            date,
            end,
            volume,
            fade: request.fade,
            fallback: request.fallback.unwrap_or(true),
            armed_from: 0,
        })
    }

    /// The station an entry asks for: a favorite, or a URL
    async fn entry_station(&self, request: &EntryRequest) -> Result<ScheduledStation, Error> {
        let favorite_id = request
            .favorite_id
            .as_deref()
            .map(str::trim)
            .filter(|i| !i.is_empty());
        let url = request
            .url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty());
        match (favorite_id, url) {
            (Some(_), Some(_)) => {
                return Err("Give favorite_id or url, not both".to_string().into())
            }
            (None, None) => return Err("Give the station: favorite_id or url".to_string().into()),
            (Some(_), None) => {}
            (None, Some(url)) => {
                check_length("url", url, MAX_URL_CHARS)?;
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err("url must start with http:// or https://".to_string().into());
                }
                let name = request.name.as_deref().map(str::trim).unwrap_or_default();
                check_length("name", name, MAX_NAME_CHARS)?;
            }
        }
        self.scheduled_station(
            favorite_id.map(str::to_string),
            url.map(str::to_string),
            request.name.as_deref().map(|n| n.trim().to_string()),
        )
        .await
    }

    /// Send a schedule change and wait for the player's answer
    async fn ask_schedule<T>(
        &self,
        command: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, String>>) -> AppCommand,
    ) -> Result<T, Error> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.send(command(reply))?;
        match tokio::time::timeout(SCHEDULE_WAIT, answer).await {
            Ok(Ok(result)) => result.map_err(Error::Failed),
            Ok(Err(_)) => Err(Error::Stopped),
            Err(_) => Err(Error::Busy),
        }
    }

    // -- Shared pieces ------------------------------------------------------

    fn providers(&self) -> Result<Arc<ProviderRegistry>, Error> {
        let mut providers = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = providers.as_ref() {
            return Ok(p.clone());
        }
        let registry = Arc::new(
            ProviderRegistry::with_defaults()
                .map_err(|e| Error::Failed(format!("Can't start the station directory: {e}")))?,
        );
        *providers = Some(registry.clone());
        Ok(registry)
    }

    /// A station by its directory id (blocking)
    fn find_station(&self, id: &str) -> Result<Station, Error> {
        let providers = self.providers()?;
        let mut failed = None;
        for provider_id in providers.list_ids() {
            let Some(provider) = providers.get(provider_id) else {
                continue;
            };
            match provider.get_station(id) {
                Ok(Some(station)) => return Ok(station),
                Ok(None) => {}
                Err(e) => failed = Some(e),
            }
        }
        Err(match failed {
            Some(e) => Error::Failed(format!("Can't look up the station: {e}")),
            None => Error::NoStation(id.to_string()),
        })
    }

    /// The first half of a save of `favorites`: what there is to write,
    /// copied while they are held (`None`: nothing changed)
    fn begin_save(
        &self,
        favorites: &FavoritesManager,
    ) -> radiotrope_app::error::Result<Option<SaveJob>> {
        match &self.favorites_file {
            Some(path) => Ok(favorites.begin_save_to(path)),
            None => favorites.begin_save(),
        }
    }

    /// The favorites, locked (blocking: the window may hold the lock while
    /// it draws them, and taking in another player's save reads the file)
    fn favorites(&self) -> MutexGuard<'_, FavoritesManager> {
        let mut favorites = self.favorites.lock().unwrap_or_else(|e| e.into_inner());
        // What another player saved since: with `--mcp --standalone` the
        // agent's own player and the usual one share the file
        favorites.reload_if_changed();
        favorites
    }

    /// Run `work` on the favorites off the async thread, which every local
    /// agent shares: waiting for the lock or the disk there would hold them
    /// all up
    async fn with_favorites<R, F>(&self, work: F) -> Result<R, Error>
    where
        F: FnOnce(&Control, &mut FavoritesManager) -> R + Send + 'static,
        R: Send + 'static,
    {
        let control = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut favorites = control.favorites();
            work(&control, &mut favorites)
        })
        .await
        .map_err(|e| Error::Failed(format!("Favorites unavailable: {e}")))
    }

    /// Run `change` on the favorites and save what it changed, off the
    /// async thread. The favorites are held for the change and again to
    /// note what was written, not while the disk is written and the file
    /// lock waited for: the window, agents and phones go on meanwhile.
    ///
    /// Gives what `change` returned and how the save went. A `change` that
    /// fails saves nothing.
    async fn change_favorites<R, F>(
        &self,
        change: F,
    ) -> Result<(R, radiotrope_app::error::Result<()>), Error>
    where
        F: FnOnce(&mut FavoritesManager) -> Result<R, Error> + Send + 'static,
        R: Send + 'static,
    {
        let control = self.clone();
        tokio::task::spawn_blocking(move || {
            let (value, job) = {
                let mut favorites = control.favorites();
                let value = change(&mut favorites)?;
                (value, control.begin_save(&favorites))
            };
            let saved = match job {
                Ok(Some(job)) => {
                    let done = job.write();
                    control
                        .favorites
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .finish_save(done)
                }
                Ok(None) => Ok(()),
                Err(e) => Err(e),
            };
            Ok((value, saved))
        })
        .await
        .map_err(|e| Error::Failed(format!("Favorites unavailable: {e}")))?
    }
}

/// A favorite's fields as the window's Edit Favorite has them
#[derive(Debug, Clone)]
pub struct FavoriteEdit {
    pub name: String,
    pub url: String,
    /// None or empty: no logo
    pub logo_url: Option<String>,
    pub country: Option<String>,
}

/// A Scheduler entry as callers describe it; [`Control::build_entry`]
/// checks it
#[derive(Debug, Clone)]
pub struct EntryRequest {
    pub action: Action,
    /// "HH:MM", local time
    pub start: String,
    /// "mon".."sun", "daily", "weekdays", "weekends"; none: once
    pub days: Vec<String>,
    /// A one-off's day, "YYYY-MM-DD"
    pub date: Option<String>,
    pub favorite_id: Option<String>,
    pub url: Option<String>,
    pub name: Option<String>,
    pub end_after_minutes: Option<f64>,
    pub end_at: Option<String>,
    /// 0 to 100; None keeps the volume
    pub volume: Option<f64>,
    pub fade: bool,
    pub fallback: Option<bool>,
}

/// `text` is at most `max` characters, or says which field is too long
pub fn check_length(what: &str, text: &str, max: usize) -> Result<(), String> {
    if text.chars().count() > max {
        return Err(format!("{what} is too long (at most {max} characters)"));
    }
    Ok(())
}

/// "mon".."sun" (or the full names), "daily", "weekdays", "weekends"
pub fn parse_days(days: &[String]) -> Result<Days, String> {
    let mut flags = [false; 7];
    for day in days {
        let day = day.trim().to_lowercase();
        match day.as_str() {
            "daily" | "every day" | "everyday" | "all" => flags = [true; 7],
            "weekdays" => flags[..5].fill(true),
            "weekends" => flags[5..].fill(true),
            _ => {
                let i = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
                    .iter()
                    .position(|d| day.starts_with(d))
                    .ok_or_else(|| format!("Not a day: {day}"))?;
                flags[i] = true;
            }
        }
    }
    Ok(Days::from_flags(flags))
}

/// A favorite's genres, A to Z
pub fn sorted_genres(genres: &HashSet<String>) -> Vec<String> {
    let mut genres: Vec<String> = genres.iter().cloned().collect();
    genres.sort();
    genres
}

#[cfg(test)]
mod tests {
    use super::*;
    use radiotrope_app::providers::types::SearchResults;
    use radiotrope_app::providers::StationProvider;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A directory with one station, which counts the plays reported to it
    struct CountingDirectory {
        plays: Arc<AtomicUsize>,
    }

    impl StationProvider for CountingDirectory {
        fn name(&self) -> &'static str {
            "Counting"
        }
        fn id(&self) -> &'static str {
            "counting"
        }
        fn search(
            &self,
            _: &str,
            _: usize,
            _: usize,
        ) -> radiotrope_app::error::Result<SearchResults> {
            Ok(no_results())
        }
        fn browse_categories(&self) -> radiotrope_app::error::Result<Vec<Category>> {
            Ok(Vec::new())
        }
        fn browse_category(
            &self,
            _: &Category,
            _: usize,
            _: usize,
        ) -> radiotrope_app::error::Result<SearchResults> {
            Ok(no_results())
        }
        fn get_popular(&self, _: usize) -> radiotrope_app::error::Result<Vec<Station>> {
            Ok(Vec::new())
        }
        fn get_station(&self, id: &str) -> radiotrope_app::error::Result<Option<Station>> {
            Ok((id == "jazz").then(|| {
                Station::new("Jazz FM", "http://jazz.test/stream")
                    .with_provider("counting", Some("jazz".to_string()))
            }))
        }
        fn report_click(&self, _: &Station) -> radiotrope_app::error::Result<()> {
            self.plays.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn no_results() -> SearchResults {
        SearchResults {
            total: Some(0),
            stations: Vec::new(),
            has_more: false,
        }
    }

    /// A control over the counting directory, saving favorites in a folder
    /// of its own
    fn control(name: &str) -> (Control, Arc<AtomicUsize>, PathBuf) {
        let (cmd_tx, _cmd_rx) = crossbeam_channel::bounded(8);
        let plays = Arc::new(AtomicUsize::new(0));
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(CountingDirectory {
            plays: plays.clone(),
        }));
        let dir = std::env::temp_dir().join(format!(
            "radiotrope-control-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let control = Control::new(
            cmd_tx,
            Arc::new(Mutex::new(AppSnapshot::default())),
            Arc::new(Mutex::new(FavoritesManager::new())),
        )
        .with_test_setup(providers, dir.join("favorites.json"));
        (control, plays, dir)
    }

    /// Wait for the report, which nobody waits on
    async fn reported(plays: &AtomicUsize, count: usize) -> bool {
        for _ in 0..200 {
            if plays.load(Ordering::SeqCst) == count {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    // The only test that changes the switch, which the whole process shares
    #[tokio::test]
    async fn a_play_is_not_reported_with_report_plays_off() {
        let (control, plays, dir) = control("report");

        crate::view_options::set_report_plays(false);
        let station = control.station_to_play("jazz".into()).await.unwrap();
        assert_eq!(station.name, "Jazz FM", "the station still plays");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(plays.load(Ordering::SeqCst), 0, "no report when off");

        crate::view_options::set_report_plays(true);
        control.station_to_play("jazz".into()).await.unwrap();
        assert!(reported(&plays, 1).await, "one report when on");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_change_is_saved_and_leaves_nothing_unsaved() {
        let (control, _, dir) = control("save");
        let file = dir.join("favorites.json");

        control
            .add_favorite(Favorite::new("Jazz FM", "http://jazz.test/stream"))
            .await
            .unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().contains("Jazz FM"));
        assert!(!control.favorites().is_dirty(), "the save was noted");

        // A change that fails saves nothing and says why
        let missing = control.remove_favorite("no-such-id".into()).await;
        assert!(matches!(missing, Err(Error::NoFavorite(id)) if id == "no-such-id"));

        let id = url_to_id("http://jazz.test/stream");
        let removed = control.remove_favorite(id).await.unwrap();
        assert_eq!(removed.name(), "Jazz FM");
        assert!(!std::fs::read_to_string(&file).unwrap().contains("Jazz FM"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A control over a playing station whose recording "starts" as soon
    /// as the player has the command, keeping the command for the test
    async fn record_with(cover_source: Option<CoverSource>) -> AppCommand {
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(8);
        let state = Arc::new(Mutex::new(AppSnapshot {
            playback: PlaybackState::Playing,
            station_name: Some("Jazz FM".into()),
            station_url: Some("http://jazz.test/stream".into()),
            ..AppSnapshot::default()
        }));
        let mut control = Control::new(
            cmd_tx,
            state.clone(),
            Arc::new(Mutex::new(FavoritesManager::new())),
        );
        if let Some(cover) = cover_source {
            control = control.with_cover_source(cover);
        }
        let started = control
            .start_recording(|| {
                state.lock().unwrap().recording = Some(crate::app::state::RecordingProgress {
                    path: PathBuf::from("Jazz FM.ogg"),
                    duration: Duration::ZERO,
                    bytes: 0,
                });
            })
            .await
            .unwrap();
        assert!(matches!(started, RecordingStart::Started(_)));
        cmd_rx.try_recv().unwrap()
    }

    fn sent_cover(cmd: AppCommand) -> Option<Vec<u8>> {
        match cmd {
            AppCommand::StartRecording { cover, .. } => cover,
            _ => panic!("not a StartRecording"),
        }
    }

    #[tokio::test]
    async fn a_recording_started_here_has_the_station_logo() {
        // As the phone remote and MCP start one: the cover comes from the
        // same source as the window's and the schedule's
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let cover: CoverSource = Arc::new({
            let asked = asked.clone();
            move |station: &Station| {
                asked.lock().unwrap().push(station.url.clone());
                Some(b"\x89PNG logo".to_vec())
            }
        });
        let cmd = record_with(Some(cover)).await;
        assert_eq!(sent_cover(cmd).as_deref(), Some(&b"\x89PNG logo"[..]));
        assert_eq!(*asked.lock().unwrap(), ["http://jazz.test/stream"]);
    }

    #[tokio::test]
    async fn a_logo_not_cached_records_with_no_cover() {
        let none: CoverSource = Arc::new(|_: &Station| None);
        assert_eq!(sent_cover(record_with(Some(none)).await), None);
        assert_eq!(sent_cover(record_with(None).await), None);
    }
}
