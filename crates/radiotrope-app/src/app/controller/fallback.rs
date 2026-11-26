//! The streams of the station being played: see
//! [`radiotrope_library::fallback`] for which is tried when one doesn't
//! connect

use std::sync::{Arc, Mutex};

use radiotrope_app::data::favorites::FavoritesManager;

use crate::app::state::AppSnapshot;

pub(super) use radiotrope_library::fallback::StreamList;

/// Gives the backups of the station at a stream address, for a start that
/// knows only the address (a scheduled entry)
pub type BackupsOf = Arc<dyn Fn(&str) -> Vec<String> + Send + Sync>;

/// The [`BackupsOf`] the controller is given: a favorite's other streams,
/// as they are when the station starts (it may have been edited since the
/// entry was saved)
pub fn favorite_backups(favorites: Arc<Mutex<FavoritesManager>>) -> BackupsOf {
    Arc::new(move |url| {
        let favorites = favorites.lock().unwrap_or_else(|e| e.into_inner());
        favorites
            .get_by_url(url)
            .map(|favorite| favorite.station.backups())
            .unwrap_or_default()
    })
}

/// Say in `state` which of `streams` is tried or playing
pub(super) fn show(streams: &StreamList, state: &mut AppSnapshot) {
    state.stream_url = Some(streams.current().to_string());
    state.stream_index = streams.index();
    state.stream_count = streams.count();
}
