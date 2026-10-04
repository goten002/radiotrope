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
use radiotrope_app::data::favorites::{FavoritesManager, PlayMetadata};
use radiotrope_app::data::recordings;
use radiotrope_app::data::schedule::{Entry, ScheduledStation};
use radiotrope_app::data::types::{name_from_url, url_to_id, Favorite, FavoriteSort, Station};
use radiotrope_app::providers::types::Category;
use radiotrope_app::providers::{CategoryType, ProviderRegistry, StationFilter};

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
        }
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
    /// play is counted in the directory, as the GUI does; nobody waits on it.
    pub async fn station_to_play(&self, id: String) -> Result<Station, Error> {
        let control = self.clone();
        let station = tokio::task::spawn_blocking(move || control.find_station(&id))
            .await
            .map_err(|e| Error::Failed(format!("Lookup failed: {e}")))??;
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
                return Err(Error::Failed(format!("Search failed: {e}")));
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
        .map_err(|e| Error::Failed(format!("Search failed: {e}")))?
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
                Some(e) if all.is_empty() => Err(Error::Failed(format!("Listing failed: {e}"))),
                _ => Ok(all),
            }
        })
        .await
        .map_err(|e| Error::Failed(format!("Listing failed: {e}")))??;

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
        self.with_favorites(move |control, favorites| {
            if favorites.count() >= MAX_FAVORITES && !favorites.is_favorite(fav.url()) {
                return Err(Error::TooManyFavorites);
            }
            favorites
                .add(fav)
                .map_err(|e| Error::Failed(e.to_string()))?;
            control
                .save_favorites(favorites)
                .map_err(|e| Error::Failed(format!("Failed to save: {e}")))
        })
        .await?
    }

    /// Remove the favorite with this id; tells which it was
    pub async fn remove_favorite(&self, id: String) -> Result<Favorite, Error> {
        self.with_favorites(move |control, favorites| {
            let removed = favorites
                .remove(&id)
                .map_err(|e| Error::Failed(e.to_string()))?;
            control
                .save_favorites(favorites)
                .map_err(|e| Error::Failed(format!("Removed but failed to save: {e}")))?;
            Ok(removed)
        })
        .await?
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
        // The settings as the window has them, not as the file may be
        // half-way through a save
        let setup = &s.recording_setup;
        self.send(AppCommand::StartRecording {
            folder: recordings::folder(setup.dir.as_deref()),
            format: setup.format,
            bitrate: setup.bitrate,
            // As in the window: the switch only counts with the EQ on
            with_eq: setup.with_eq && s.eq_enabled,
            cover: None,
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
                .map_err(|e| Error::Failed(format!("Failed to initialize provider: {e}")))?,
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
            Some(e) => Error::Failed(format!("Lookup failed: {e}")),
            None => Error::NoStation(id.to_string()),
        })
    }

    fn save_favorites(
        &self,
        favorites: &mut FavoritesManager,
    ) -> radiotrope_app::error::Result<()> {
        match &self.favorites_file {
            Some(path) => favorites.save_to(path),
            None => favorites.save(),
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
}

/// A favorite's genres, A to Z
pub fn sorted_genres(genres: &HashSet<String>) -> Vec<String> {
    let mut genres: Vec<String> = genres.iter().cloned().collect();
    genres.sort();
    genres
}
