//! Data persistence
//!
//! Handles favorites, settings, history, and caching.

pub mod cache;
pub mod favorites;
pub mod settings;
pub mod storage;
pub mod types;

// Re-export common types
pub use cache::ImageCache;
pub use favorites::FavoritesManager;
pub use settings::{Settings, Theme};
pub use storage::{config_dir, data_path, ensure_config_dir, load, save};
pub use types::{
    url_to_id, Favorite, FavoriteFilter, FavoriteSort, FavoriteUpdate, HasLogo, Station,
};
