//! What the window's handlers share

use std::sync::{Arc, Mutex, MutexGuard};

use radiotrope_app::data::favorites::FavoritesManager;
use radiotrope_app::network::logo::LogoService;

use crate::app::state::AppSnapshot;
use crate::app::ui_sender::UiSender;
use crate::App;

/// The handles nearly every handler of the window needs. Cloning it clones
/// the handles, not what they point at. It stays on the window's thread
/// (the command sender does); another thread takes the handles it needs.
#[derive(Clone)]
pub(crate) struct AppContext {
    /// The window
    pub(crate) ui: slint::Weak<App>,
    /// Commands to the controller
    pub(crate) tx: UiSender,
    /// What the controller last reported
    pub(crate) state: Arc<Mutex<AppSnapshot>>,
    pub(crate) favorites: Arc<Mutex<FavoritesManager>>,
    /// Station logos, cached on disk
    pub(crate) logos: Arc<LogoService>,
}

/// Lock `mutex`, also after a thread panicked while holding it: what it
/// guards is still used
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
