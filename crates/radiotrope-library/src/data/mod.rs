//! Station data and favorites persistence.

pub mod countries;
pub mod favorites;
pub mod languages;
pub mod storage;
pub mod types;

pub use favorites::FavoritesManager;
pub use storage::{config_dir, data_path, ensure_config_dir, load, save};
pub use types::{
    url_to_id, Favorite, FavoriteFilter, FavoriteSort, FavoriteUpdate, HasLogo, Station,
};
