use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender};
use radiotrope_library::data::favorites::{FavoriteEdit, FavoritesManager};
use radiotrope_library::data::types::{FavoriteSort, Station};

const FOLLOW_INTERVAL: Duration = Duration::from_secs(2);

pub enum Reply {
    Followed(Result<Vec<Station>, String>),
    Edited(Result<Vec<Station>, String>),
}

pub struct FavoritesWorker {
    requests: Sender<FavoriteEdit>,
    pub replies: Receiver<Reply>,
    thread: JoinHandle<()>,
}

impl FavoritesWorker {
    pub fn spawn(path: PathBuf, mut manager: FavoritesManager) -> std::io::Result<Self> {
        let (requests, work) = bounded(1);
        let (send, replies) = unbounded();
        let thread = thread::Builder::new()
            .name("favorites".into())
            .spawn(move || {
                let mut next_follow = Instant::now() + FOLLOW_INTERVAL;
                loop {
                    let reply = match work
                        .recv_timeout(next_follow.saturating_duration_since(Instant::now()))
                    {
                        Ok(edit) => {
                            let result = FavoritesManager::edit_file(&path, edit)
                                .map(|updated| {
                                    manager = updated;
                                    stations(&manager)
                                })
                                .map_err(|error| error.to_string());
                            Reply::Edited(result)
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            next_follow = Instant::now() + FOLLOW_INTERVAL;
                            match manager.refresh_observed(&path) {
                                Ok(false) => continue,
                                Ok(true) => Reply::Followed(Ok(stations(&manager))),
                                Err(error) => Reply::Followed(Err(error.to_string())),
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    };
                    if send.send(reply).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            requests,
            replies,
            thread,
        })
    }

    pub fn edit(&self, edit: FavoriteEdit) -> Result<(), String> {
        self.requests
            .try_send(edit)
            .map_err(|error| error.to_string())
    }

    /// Finish an edit that was already accepted before quitting.
    pub fn finish(self) {
        drop(self.requests);
        let _ = self.thread.join();
    }
}

pub fn stations(manager: &FavoritesManager) -> Vec<Station> {
    let mut list = manager.sorted(FavoriteSort::Manual);
    list.sort_by_cached_key(|favorite| {
        (
            favorite.sort_order,
            favorite.name().to_lowercase(),
            favorite.url(),
        )
    });
    list.into_iter()
        .filter(|favorite| !favorite.url().trim().is_empty())
        .map(|favorite| favorite.station.clone())
        .map(|mut station| {
            if station.name.trim().is_empty() {
                station.name = station.url.clone();
            }
            station
        })
        .collect()
}

pub fn find_match<'a>(favorites: &'a [Station], station: &Station) -> Option<&'a Station> {
    favorites
        .iter()
        .find(|favorite| favorite.url == station.url)
        .or_else(|| {
            let provider_id = station.provider_id.as_deref().filter(|id| !id.is_empty())?;
            favorites
                .iter()
                .find(|favorite| favorite.provider_id.as_deref() == Some(provider_id))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use radiotrope_library::data::types::Favorite;

    #[test]
    fn edits_round_trip_metadata_and_match_changed_urls_by_provider_id() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("favorites.json");
        let mut watcher = FavoritesManager::observe(&path).unwrap();
        let station = Station::new("Jazz Radio", "https://original.test")
            .with_provider("radiotrope", Some("jazz-1".into()))
            .with_logo("https://jazz.test/logo.png")
            .with_metadata(
                Some("France".into()),
                Some("French".into()),
                ["jazz".to_owned()].into(),
            )
            .with_audio_info(Some("MP3".into()), Some(128));
        let favorite = Favorite::from_station(station.clone());
        FavoritesManager::edit_file(&path, FavoriteEdit::Add(Box::new(favorite))).unwrap();
        assert!(watcher.refresh_observed(&path).unwrap());
        assert_eq!(stations(&watcher), vec![station.clone()]);

        let mut moved = station.clone();
        moved.url = "https://moved.test".into();
        let list = stations(&watcher);
        assert_eq!(find_match(&list, &moved), Some(&station));
        let duplicate = FavoriteEdit::Add(Box::new(Favorite::from_station(moved)));
        let latest = FavoritesManager::edit_file(&path, duplicate).unwrap();
        assert_eq!(stations(&latest), vec![station.clone()]);

        FavoritesManager::edit_file(&path, FavoriteEdit::Remove(station.id())).unwrap();
        assert!(watcher.refresh_observed(&path).unwrap());
        assert!(stations(&watcher).is_empty());
    }

    #[test]
    fn malformed_files_are_left_byte_for_byte_intact() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("favorites.json");
        for damaged in [
            "{broken",
            "",
            r#"{"favorites":[{"url":"https://bad.test"}]}"#,
        ] {
            std::fs::write(&path, damaged).unwrap();
            assert!(FavoritesManager::observe(&path).is_err());
            let add = FavoriteEdit::Add(Box::new(Favorite::new("New", "https://new.test")));
            assert!(FavoritesManager::edit_file(&path, add).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), damaged);
            let remove = FavoriteEdit::Remove("unknown".into());
            assert!(FavoritesManager::edit_file(&path, remove).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), damaged);
        }
    }

    #[test]
    fn matching_urls_take_precedence_over_provider_ids() {
        let station = Station::new("Directory", "https://same.test")
            .with_provider("radiotrope", Some("1".into()));
        let favorites = vec![
            Station::new("By provider", "https://old.test")
                .with_provider("radiotrope", Some("1".into())),
            Station::new("By URL", "https://same.test"),
        ];
        assert_eq!(find_match(&favorites, &station), Some(&favorites[1]));
    }

    #[test]
    fn concurrent_edits_keep_unrelated_favorites() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("favorites.json");
        let barrier = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            for name in ["One", "Two"] {
                let path = &path;
                let barrier = &barrier;
                scope.spawn(move || {
                    let favorite = Favorite::new(name, format!("https://{name}.test"));
                    barrier.wait();
                    FavoritesManager::edit_file(path, FavoriteEdit::Add(Box::new(favorite)))
                        .unwrap();
                });
            }
            barrier.wait();
        });
        let observed = FavoritesManager::observe(&path).unwrap();
        assert_eq!(observed.count(), 2);
    }
}
