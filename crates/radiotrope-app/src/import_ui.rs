//! Favorites to and from a file: File > Import Favorites and Export
//! Favorites

use std::sync::{Arc, Mutex};

use slint::ComponentHandle;

use radiotrope_app::data::favorites::{FavoritesManager, ImportMode, ImportOutcome};

use crate::context::{lock, AppContext};
use crate::favorites_list::refresh_favorites;
use crate::{control, App};

/// Wire File > Import Favorites and Export Favorites
pub(crate) fn setup(ui: &App, ctx: &AppContext) {
    let (favorites, logo_service) = (&ctx.favorites, &ctx.logos);
    {
        // The list read from the file picked, until Add Missing or Replace
        let pending: std::rc::Rc<std::cell::RefCell<Vec<radiotrope_app::data::types::Favorite>>> =
            Default::default();
        {
            let ui_weak = ui.as_weak();
            let favs = favorites.clone();
            ui.on_export_favorites(move || export_favorites(ui_weak.clone(), favs.clone()));
        }
        {
            let ui_weak = ui.as_weak();
            let favs = favorites.clone();
            let pending = pending.clone();
            ui.on_import_favorites(move || {
                import_favorites(ui_weak.clone(), favs.clone(), pending.clone())
            });
        }
        {
            let ui_weak = ui.as_weak();
            let favs = favorites.clone();
            let logo_svc = logo_service.clone();
            ui.on_import_favorites_chosen(move |replace| {
                let Some(ui) = ui_weak.upgrade() else { return };
                let list = std::mem::take(&mut *pending.borrow_mut());
                let mode = if replace {
                    ImportMode::Replace
                } else {
                    ImportMode::Add
                };
                let mut f = lock(&favs);
                let done = f
                    .import_list(mode, &list, control::MAX_FAVORITES, false)
                    .and_then(|outcome| f.save().map(|_| outcome));
                drop(f);
                match done {
                    Ok(outcome) => {
                        let text = import_done_text(mode, &outcome, list.len());
                        ui.set_import_message(text.into());
                        ui.set_import_message_detail(Default::default());
                        ui.set_import_message_error(false);
                    }
                    Err(e) => {
                        ui.set_import_message(e.to_string().into());
                        ui.set_import_message_detail(Default::default());
                        ui.set_import_message_error(true);
                    }
                }
                refresh_favorites(&ui, &favs, &logo_svc);
            });
        }
    }
}

/// Show the favorites file dialog with only a message (an export done, or
/// what went wrong)
pub(crate) fn show_favorites_file_message(
    ui: &App,
    title: &str,
    message: &str,
    detail: &str,
    error: bool,
) {
    ui.set_import_title(title.into());
    ui.set_import_subtitle(Default::default());
    ui.set_import_message(message.into());
    ui.set_import_message_detail(detail.into());
    ui.set_import_message_error(error);
    ui.set_show_import_favorites(true);
}

/// What taking in a list would change, for Import Favorites: "Adds 12,
/// keeps 30"
#[cfg(feature = "desktop")]
pub(crate) fn import_preview_text(mode: ImportMode, o: &ImportOutcome) -> String {
    if !o.changes() {
        return match mode {
            ImportMode::Add => "You have them all already.".into(),
            ImportMode::Replace => "Nothing would change.".into(),
        };
    }
    let mut parts = Vec::new();
    if o.added > 0 {
        parts.push(format!("adds {}", o.added));
    }
    if o.updated > 0 {
        parts.push(format!("updates {}", o.updated));
    }
    if o.removed > 0 {
        parts.push(format!("removes {}", o.removed));
    }
    if mode == ImportMode::Add && o.kept > 0 {
        parts.push(format!("keeps {}", o.kept));
    }
    if mode == ImportMode::Replace && o.reordered {
        parts.push("new order".into());
    }
    let mut text = parts.join(", ");
    if let Some(first) = text.get(..1) {
        text = first.to_uppercase() + &text[1..];
    }
    if o.over_cap > 0 {
        text += &format!(
            ". {} don't fit ({} favorites at most)",
            o.over_cap,
            control::MAX_FAVORITES
        );
    }
    text + "."
}

/// What Import Favorites did
pub(crate) fn import_done_text(mode: ImportMode, o: &ImportOutcome, listed: usize) -> String {
    if !o.changes() {
        return "Nothing changed: your favorites already match.".into();
    }
    match mode {
        ImportMode::Add => {
            let mut text = match o.added {
                1 => "Added 1 station.".to_string(),
                n => format!("Added {n} stations."),
            };
            if o.over_cap > 0 {
                text += &format!(" {} didn't fit.", o.over_cap);
            }
            text
        }
        ImportMode::Replace => format!("Your favorites are now the file's {listed} stations."),
    }
}

/// Save the favorites to a file the user picks
#[cfg(feature = "desktop")]
pub(crate) fn export_favorites(ui_weak: slint::Weak<App>, favs: Arc<Mutex<FavoritesManager>>) {
    let Some(ui) = ui_weak.upgrade() else { return };
    let save = rfd::AsyncFileDialog::new()
        .set_title("Export Favorites")
        .set_file_name("Radiotrope Favorites.json")
        .add_filter("Radiotrope favorites", &["json"])
        .set_parent(&ui.window().window_handle())
        .save_file();
    let spawned = slint::spawn_local(async move {
        let Some(file) = save.await else { return };
        let Some(ui) = ui_weak.upgrade() else { return };
        let path = file.path().to_path_buf();
        let saved = lock(&favs).export_to(&path);
        match saved {
            Ok(count) => show_favorites_file_message(
                &ui,
                "Export Favorites",
                &format!("Saved {count} favorites. To restore them, use Import Favorites."),
                &path.display().to_string(),
                false,
            ),
            Err(e) => {
                show_favorites_file_message(&ui, "Export Favorites", &e.to_string(), "", true)
            }
        }
    });
    if let Err(e) = spawned {
        eprintln!("Failed to open the file dialog: {e}");
    }
}

/// Pick a favorites file, then ask Add Missing or Replace
#[cfg(feature = "desktop")]
pub(crate) fn import_favorites(
    ui_weak: slint::Weak<App>,
    favs: Arc<Mutex<FavoritesManager>>,
    pending: std::rc::Rc<std::cell::RefCell<Vec<radiotrope_app::data::types::Favorite>>>,
) {
    let Some(ui) = ui_weak.upgrade() else { return };
    let pick = rfd::AsyncFileDialog::new()
        .set_title("Import Favorites")
        .add_filter("Radiotrope favorites", &["json"])
        .set_parent(&ui.window().window_handle())
        .pick_file();
    let spawned = slint::spawn_local(async move {
        let Some(file) = pick.await else { return };
        let Some(ui) = ui_weak.upgrade() else { return };
        let path = file.path().to_path_buf();
        let list = match radiotrope_app::data::favorites::read_list(&path) {
            Ok(list) if !list.is_empty() => list,
            Ok(_) => {
                return show_favorites_file_message(
                    &ui,
                    "Import Favorites",
                    "The file has no favorites.",
                    "",
                    true,
                )
            }
            Err(e) => {
                return show_favorites_file_message(
                    &ui,
                    "Import Favorites",
                    &e.to_string(),
                    "",
                    true,
                )
            }
        };
        let previews = {
            let mut f = lock(&favs);
            [ImportMode::Add, ImportMode::Replace].map(|mode| {
                f.import_list(mode, &list, control::MAX_FAVORITES, true)
                    .map(|o| import_preview_text(mode, &o))
                    .unwrap_or_else(|e| e.to_string())
            })
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        ui.set_import_title("Import Favorites".into());
        ui.set_import_subtitle(
            match list.len() {
                1 => format!("{name}: 1 station"),
                n => format!("{name}: {n} stations"),
            }
            .into(),
        );
        let [add, replace] = previews;
        ui.set_import_add_summary(add.into());
        ui.set_import_replace_summary(replace.into());
        ui.set_import_message(Default::default());
        ui.set_import_message_detail(Default::default());
        ui.set_import_message_error(false);
        *pending.borrow_mut() = list;
        ui.set_show_import_favorites(true);
    });
    if let Err(e) = spawned {
        eprintln!("Failed to open the file dialog: {e}");
    }
}

/// The kiosk build has no file dialog (the menu items are greyed out)
#[cfg(not(feature = "desktop"))]
pub(crate) fn export_favorites(_ui_weak: slint::Weak<App>, _favs: Arc<Mutex<FavoritesManager>>) {}

#[cfg(not(feature = "desktop"))]
pub(crate) fn import_favorites(
    _ui_weak: slint::Weak<App>,
    _favs: Arc<Mutex<FavoritesManager>>,
    _pending: std::rc::Rc<std::cell::RefCell<Vec<radiotrope_app::data::types::Favorite>>>,
) {
}
