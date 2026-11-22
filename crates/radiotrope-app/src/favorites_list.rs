//! The favorites list on screen: its rows, their logos and their listening
//! figures

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use slint::{ComponentHandle, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use radiotrope_app::data::favorites::FavoritesManager;
use radiotrope_app::data::types::{Favorite, FavoriteSort};
use radiotrope_app::network::logo::{LogoService, Workers};
use radiotrope_app::text;
use radiotrope_app::visual::with_backdrop;

use crate::browse_ui::mark_browse_favorites;
use crate::context::lock;
use crate::favorites_ui::session_listen_secs;
use crate::{App, FavoriteStation};

fn favorite_to_slint(
    f: &radiotrope_app::data::types::Favorite,
    stats: radiotrope_app::data::types::StationStats,
) -> FavoriteStation {
    FavoriteStation {
        id: f.id().into(),
        name: f.name().into(),
        url: f.url().into(),
        logo_url: f.station.logo_url.as_deref().unwrap_or("").into(),
        country: f.station.country.as_deref().unwrap_or("").into(),
        listen_time: format_listen_time(stats.total_listen_time_secs).into(),
        last_played: format_last_played(stats.last_played).into(),
        play_count: stats.play_count.min(i32::MAX as u32) as i32,
        session_time: format_listen_time(session_listen_secs(&f.id())).into(),
    }
}

/// Refresh the stats text of every favorites row in place, without
/// rebuilding the list (which would interrupt a drag)
pub(crate) fn update_favorite_stats(ui: &App, favs: &FavoritesManager) {
    let model = ui.get_favorites_list();
    for i in 0..model.row_count() {
        let Some(mut row) = model.row_data(i) else {
            continue;
        };
        if favs.get(&row.id).is_none() {
            continue;
        }
        let stats = favs.stats(&row.id);
        let listen_time: slint::SharedString =
            format_listen_time(stats.total_listen_time_secs).into();
        let last_played: slint::SharedString = format_last_played(stats.last_played).into();
        let play_count = stats.play_count.min(i32::MAX as u32) as i32;
        let session_time: slint::SharedString =
            format_listen_time(session_listen_secs(&row.id)).into();
        if row.listen_time != listen_time
            || row.last_played != last_played
            || row.play_count != play_count
            || row.session_time != session_time
        {
            row.listen_time = listen_time;
            row.last_played = last_played;
            row.play_count = play_count;
            row.session_time = session_time;
            model.set_row_data(i, row);
        }
    }
}

/// Total listening time for a favorites row: "3 h 25 min", "40 min",
/// "<1 min", or empty when never played
fn format_listen_time(secs: u64) -> String {
    text::minutes(secs)
}

/// When a favorite was last played, relative to now: "Just now",
/// "12 min ago", "3 h ago", "Yesterday", "5 days ago", "2 weeks ago",
/// "4 months ago", or empty when never played
fn format_last_played(timestamp: Option<u64>) -> String {
    let Some(ts) = timestamp else {
        return String::new();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let ago = now.saturating_sub(ts);
    let (min, hour, day) = (60, 3600, 86400);
    match ago {
        a if a < 2 * min => "Just now".into(),
        a if a < hour => format!("{} min ago", a / min),
        a if a < day => format!("{} h ago", a / hour),
        a if a < 2 * day => "Yesterday".into(),
        a if a < 14 * day => format!("{} days ago", a / day),
        a if a < 60 * day => format!("{} weeks ago", a / (7 * day)),
        a if a < 365 * day => format!("{} months ago", a / (30 * day)),
        a => {
            let years = a / (365 * day);
            if years == 1 {
                "1 year ago".into()
            } else {
                format!("{years} years ago")
            }
        }
    }
}

/// A favorite's logo as its row shows it (no larger than
/// [`ROW_LOGO_SIZE`](radiotrope_app::network::browse_logos::ROW_LOGO_SIZE),
/// about 26 KB), and when the file it was decoded from was written: a
/// newer file (a logo downloaded again) is decoded again
struct DecodedLogo {
    image: slint::Image,
    written: Option<SystemTime>,
}

// In-memory cache of decoded slint::Image keyed by favorite ID, for the
// favorites in the list (the others are let go at each redraw).
// All callers run on the Slint event-loop thread, so thread_local is safe and avoids
// threading slint::Image (which is !Send) through closures.
thread_local! {
    static LOGO_IMAGE_CACHE: std::cell::RefCell<HashMap<String, DecodedLogo>> =
        std::cell::RefCell::new(HashMap::new());
}

thread_local! {
    /// The favorites generation the list on screen shows. A change made
    /// elsewhere (an agent) shows as a newer one, which the poll redraws.
    pub(crate) static FAVORITES_SHOWN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Threads reading and decoding the list's logos
const LOADERS: usize = 4;

/// The threads that read the logo files and decode them, so the UI thread
/// does neither. Downloads have their own (see
/// [`LogoService::fetch_in_background`]): a slow host holds up no cached
/// logo.
static LOADING_THREADS: LazyLock<Arc<Workers>> =
    LazyLock::new(|| Workers::new("favorite-logo", LOADERS));

/// The favorites (by id) whose logo a loader has been given, and whether
/// the list asked for it again meanwhile: the favorite may have changed,
/// so the list looks once more when that logo is done
static LOADING: Mutex<HashMap<String, bool>> = Mutex::new(HashMap::new());

/// Logos the loaders are done with, for the UI thread to take (see
/// [`show_loaded`])
static LOADED: Mutex<Vec<Loaded>> = Mutex::new(Vec::new());

/// What a loader found for a favorite
struct Loaded {
    id: String,
    logo: Found,
    /// The list asked for it again while it was being loaded
    again: bool,
}

/// A favorite's cached logo, next to the one its row shows
enum Found {
    /// The one shown
    Same,
    /// None (any more), or one that can't be shown
    Gone,
    /// Another: on its backdrop, ready to show, and when its file was
    /// written
    New(SharedPixelBuffer<Rgba8Pixel>, Option<SystemTime>),
}

/// Tells the UI thread that [`LOADED`] has logos to take
type Notify = Arc<dyn Fn() + Send + Sync>;

/// The favorites on screen are up to date with `favs`
pub(crate) fn note_favorites_shown(favs: &FavoritesManager) {
    FAVORITES_SHOWN.set(favs.generation());
}

/// Remove a cached logo image, forcing re-decode on next refresh.
pub(crate) fn invalidate_logo_image(id: &str) {
    LOGO_IMAGE_CACHE.with(|cache| {
        cache.borrow_mut().remove(id);
    });
}

/// Redraw the favorites list, and start loading the logos it lacks.
///
/// Nothing here reads a file or decodes an image: the rows show the logos
/// already decoded (a placeholder for the others) at once, and the
/// loaders fill in the rest as they go (see [`request_logos`]).
pub(crate) fn refresh_favorites(
    ui: &App,
    favorites: &Arc<Mutex<FavoritesManager>>,
    logo_service: &Arc<LogoService>,
) {
    let (sorted, items): (Vec<Favorite>, Vec<FavoriteStation>) = {
        let favs = lock(favorites);
        mark_browse_favorites(ui, &favs);
        note_favorites_shown(&favs);
        favs.sorted(FavoriteSort::Manual)
            .into_iter()
            .map(|f| (f.clone(), favorite_to_slint(f, favs.stats(&f.id()))))
            .unzip()
    };
    let logos = request_logos(ui, &sorted, favorites, logo_service);

    ui.set_favorites_list(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
    ui.set_favorite_logos(ModelRc::from(std::rc::Rc::new(VecModel::from(logos))));
}

/// The decoded logos of `favs`, in their order, with an empty image for
/// those not decoded yet. A loader is given every favorite whose logo
/// isn't known to be up to date:
/// - nothing is known of its cached file yet (the first look, or the
///   file was just written or deleted)
/// - the file is newer than the logo shown, or gone
/// - the logo is missing or out of date, and worth fetching
///
/// What is known comes from memory ([`LogoService::glance`]), so a redraw
/// with nothing to load costs a map lookup per favorite.
fn request_logos(
    ui: &App,
    favs: &[Favorite],
    favorites: &Arc<Mutex<FavoritesManager>>,
    logo_service: &Arc<LogoService>,
) -> Vec<slint::Image> {
    let ids: Vec<String> = favs.iter().map(|f| f.id()).collect();
    let mut notify: Option<Notify> = None;
    LOGO_IMAGE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        // Favorites removed here, by a phone, an agent or an import keep
        // no logo in memory
        if cache.len() > ids.len() || cache.keys().any(|id| !ids.contains(id)) {
            let listed: HashSet<&String> = ids.iter().collect();
            cache.retain(|id, _| listed.contains(id));
        }
        favs.iter()
            .zip(&ids)
            .map(|(fav, id)| {
                let decoded = cache.get(id);
                let shown = decoded.and_then(|logo| logo.written);
                let load = match logo_service.glance(fav) {
                    Some(glance) => glance.modified != shown || glance.wanted,
                    None => true,
                };
                if load {
                    let notify = notify
                        .get_or_insert_with(|| notifier(ui, favorites, logo_service))
                        .clone();
                    queue_load(logo_service, fav, id, shown, notify);
                }
                // An out of date logo shows until its new one arrives
                decoded.map(|logo| logo.image.clone()).unwrap_or_default()
            })
            .collect()
    })
}

/// Has the UI thread take the loaded logos (see [`show_loaded`])
fn notifier(
    ui: &App,
    favorites: &Arc<Mutex<FavoritesManager>>,
    logo_service: &Arc<LogoService>,
) -> Notify {
    let ui = ui.as_weak();
    let favorites = favorites.clone();
    let logo_service = logo_service.clone();
    Arc::new(move || {
        let favorites = favorites.clone();
        let logo_service = logo_service.clone();
        let _ = ui.upgrade_in_event_loop(move |ui| show_loaded(&ui, &favorites, &logo_service));
    })
}

/// Give `fav`'s logo to a loader, unless one has it already. `shown` is
/// when the file of the logo on screen was written.
fn queue_load(
    logo_service: &Arc<LogoService>,
    fav: &Favorite,
    id: &str,
    shown: Option<SystemTime>,
    notify: Notify,
) {
    {
        let mut loading = lock(&LOADING);
        if let Some(again) = loading.get_mut(id) {
            *again = true;
            return;
        }
        loading.insert(id.to_string(), false);
    }
    let logo_service = logo_service.clone();
    let fav = fav.clone();
    LOADING_THREADS.run(move || load(&logo_service, fav, shown, notify));
}

/// On a loader thread: decode `fav`'s cached logo if it isn't the one
/// shown, and have it fetched if it is missing or out of date
fn load(logo_service: &Arc<LogoService>, fav: Favorite, shown: Option<SystemTime>, notify: Notify) {
    let id = fav.id();
    // Read from the disk the first time; the list's later looks are then
    // answered from memory
    let written = logo_service.cached_modified(&fav);
    let logo = if written == shown {
        Found::Same
    } else {
        row_logo(logo_service, &fav, written)
    };
    let again = lock(&LOADING).remove(&id).unwrap_or(false);
    if again || !matches!(logo, Found::Same) {
        loaded(Loaded { id, logo, again }, &notify);
    }
    if logo_service.is_missing(&fav) {
        let service = logo_service.clone();
        logo_service.fetch_in_background(fav, move |fav: &Favorite| {
            // Each logo shows as it arrives
            let written = service.cached_modified(fav);
            let logo = row_logo(&service, fav, written);
            let id = fav.id();
            let again = false;
            loaded(Loaded { id, logo, again }, &notify);
        });
    }
}

/// `fav`'s cached logo at row size, on its fog when it needs one.
/// `written` is when its file was written. One that doesn't decode is
/// deleted (see [`LogoService::get_cached_row_rgba`]).
fn row_logo(logo_service: &LogoService, fav: &Favorite, written: Option<SystemTime>) -> Found {
    let Some((rgba, width, height)) = logo_service.get_cached_row_rgba(fav) else {
        return Found::Gone;
    };
    let (rgba, width, height) = with_backdrop(rgba, width, height);
    Found::New(
        SharedPixelBuffer::clone_from_slice(&rgba, width, height),
        written,
    )
}

/// Hand a loaded logo to the UI thread. It is told only of the first one
/// waiting: it takes all there are when it comes, so the list is redrawn
/// every few logos, not once for each.
fn loaded(logo: Loaded, notify: &Notify) {
    let first = {
        let mut waiting = lock(&LOADED);
        waiting.push(logo);
        waiting.len() == 1
    };
    if first {
        notify();
    }
}

/// On the UI thread: put the logos the loaders are done with on their
/// rows. The rows are set in place, so a drag goes on. Returns how many
/// logos were taken.
fn show_loaded(
    ui: &App,
    favorites: &Arc<Mutex<FavoritesManager>>,
    logo_service: &Arc<LogoService>,
) -> usize {
    let logos = std::mem::take(&mut *lock(&LOADED));
    if logos.is_empty() {
        return 0;
    }
    let list = ui.get_favorites_list();
    let rows: HashMap<slint::SharedString, usize> = (0..list.row_count())
        .filter_map(|row| Some((list.row_data(row)?.id, row)))
        .collect();
    let model = ui.get_favorite_logos();
    let mut look_again = false;
    LOGO_IMAGE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        for Loaded { id, logo, again } in &logos {
            look_again |= again;
            // Not a favorite any more
            let Some(&row) = rows.get(id.as_str()) else {
                continue;
            };
            let image = match logo {
                Found::Same => continue,
                Found::New(pixels, written) => {
                    let image = slint::Image::from_rgba8(pixels.clone());
                    cache.insert(
                        id.clone(),
                        DecodedLogo {
                            image: image.clone(),
                            written: *written,
                        },
                    );
                    image
                }
                Found::Gone => {
                    cache.remove(id);
                    slint::Image::default()
                }
            };
            if row < model.row_count() {
                model.set_row_data(row, image);
            }
        }
    });
    if look_again {
        let favs: Vec<Favorite> = lock(favorites)
            .sorted(FavoriteSort::Manual)
            .into_iter()
            .cloned()
            .collect();
        request_logos(ui, &favs, favorites, logo_service);
    }
    logos.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use radiotrope_app::data::cache::ImageCache;
    use radiotrope_app::data::types::HasLogo;
    use radiotrope_app::network::browse_logos::ROW_LOGO_SIZE;

    #[test]
    fn listening_time_says_min() {
        assert_eq!(format_listen_time(40 * 60), "40 min");
        assert_eq!(format_listen_time(3 * 3600 + 25 * 60), "3 h 25 min");
    }

    /// A logo as stations have them: full cache size, and not one flat
    /// colour (that would decode far faster than a real one)
    fn logo_png(seed: u32) -> Vec<u8> {
        let size = radiotrope_app::data::cache::LOGO_MAX_SIZE;
        let image = image::RgbaImage::from_fn(size, size, |x, y| {
            let n = (x * 31 + y * 17 + seed * 7).wrapping_mul(2_654_435_761);
            image::Rgba([(n >> 24) as u8, (n >> 16) as u8, (x + y) as u8, 255])
        });
        let mut png = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }

    /// Take what the loaders are done with until `done`, as the event
    /// loop would
    fn show_until(
        ui: &App,
        favorites: &Arc<Mutex<FavoritesManager>>,
        logos: &Arc<LogoService>,
        done: impl Fn() -> bool,
    ) {
        let give_up = Instant::now() + Duration::from_secs(120);
        while !done() {
            assert!(Instant::now() < give_up, "the logos did not load");
            if show_loaded(ui, favorites, logos) == 0 {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn logos_shown(ui: &App) -> usize {
        let logos = ui.get_favorite_logos();
        (0..logos.row_count())
            .filter_map(|row| logos.row_data(row))
            .filter(|logo| logo.size().width > 0)
            .count()
    }

    fn logos_in_memory() -> usize {
        LOGO_IMAGE_CACHE.with(|cache| cache.borrow().len())
    }

    // One test only: the loaders' state is shared, and Slint's platform is
    // set once per thread
    #[test]
    fn five_hundred_favorites_show_at_once_and_their_logos_load_off_the_ui_thread() {
        const FAVORITES: usize = 500;
        /// Far above what the redraw takes (a few ms), far below reading
        /// and decoding 500 logos here (seconds in a debug build)
        const REDRAW_BOUND: Duration = Duration::from_millis(500);

        slint::platform::set_platform(Box::new(crate::headless::HeadlessPlatform::default()))
            .expect("no other Slint platform yet");
        let ui = App::new().unwrap();
        let dir =
            std::env::temp_dir().join(format!("radiotrope_favorites_list_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let logos =
            Arc::new(LogoService::with_cache(ImageCache::with_dir(dir.clone()).unwrap()).unwrap());
        let mut manager = FavoritesManager::new();
        // A few different pictures are enough: each file is read and
        // decoded on its own
        let pngs: Vec<Vec<u8>> = (0..8).map(logo_png).collect();
        for i in 0..FAVORITES {
            let fav = Favorite::new(format!("Station {i}"), format!("http://station.test/{i}"));
            let key = fav.logo_cache_key();
            assert!(logos
                .cache()
                .put_thumbnail(&key, &pngs[i % pngs.len()])
                .is_some());
            manager.add(fav).unwrap();
        }
        let favorites = Arc::new(Mutex::new(manager));

        // The first redraw, as at startup: every row at once, no logo yet
        let started = Instant::now();
        refresh_favorites(&ui, &favorites, &logos);
        let first = started.elapsed();
        assert_eq!(ui.get_favorites_list().row_count(), FAVORITES);
        assert_eq!(ui.get_favorite_logos().row_count(), FAVORITES);
        assert!(first < REDRAW_BOUND, "the first redraw took {first:?}");

        // The loaders decode them; the rows are set in place
        let started = Instant::now();
        show_until(&ui, &favorites, &logos, || logos_shown(&ui) == FAVORITES);
        let loading = started.elapsed();
        assert_eq!(logos_in_memory(), FAVORITES);
        let largest = (0..FAVORITES)
            .filter_map(|row| ui.get_favorite_logos().row_data(row))
            .map(|logo| logo.size().width.max(logo.size().height))
            .max();
        assert_eq!(largest, Some(ROW_LOGO_SIZE), "rows keep small logos");

        // A redraw with everything loaded gives the loaders nothing, and
        // keeps the logos
        let started = Instant::now();
        refresh_favorites(&ui, &favorites, &logos);
        let again = started.elapsed();
        assert!(again < REDRAW_BOUND, "a later redraw took {again:?}");
        assert!(lock(&LOADING).is_empty(), "nothing was left to load");
        assert_eq!(logos_shown(&ui), FAVORITES);
        eprintln!(
            "{FAVORITES} favorites: first redraw {first:?}, logos loaded after {loading:?}, later redraw {again:?}"
        );

        // A logo downloaded again (a newer file) replaces the one shown
        let changed = lock(&favorites).sorted(FavoriteSort::Manual)[3].clone();
        let small = {
            let image = image::RgbaImage::from_pixel(40, 20, image::Rgba([200, 30, 30, 255]));
            let mut png = Vec::new();
            image
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            png
        };
        assert!(logos
            .cache()
            .put_thumbnail(&changed.logo_cache_key(), &small)
            .is_some());
        refresh_favorites(&ui, &favorites, &logos);
        let width_of_changed = || ui.get_favorite_logos().row_data(3).unwrap().size().width;
        assert_eq!(
            width_of_changed(),
            ROW_LOGO_SIZE,
            "the old one shows meanwhile"
        );
        show_until(&ui, &favorites, &logos, || width_of_changed() == 40);

        // A deleted logo leaves its row
        logos.delete(&changed);
        refresh_favorites(&ui, &favorites, &logos);
        show_until(&ui, &favorites, &logos, || width_of_changed() == 0);
        assert_eq!(logos_in_memory(), FAVORITES - 1);

        // Favorites removed elsewhere keep no logo in memory
        {
            let mut favs = lock(&favorites);
            let ids: Vec<String> = favs.sorted(FavoriteSort::Manual)[100..]
                .iter()
                .map(|f| f.id())
                .collect();
            for id in ids {
                favs.remove(&id).unwrap();
            }
        }
        refresh_favorites(&ui, &favorites, &logos);
        assert_eq!(ui.get_favorites_list().row_count(), 100);
        assert_eq!(logos_in_memory(), 99);
        assert_eq!(logos_shown(&ui), 99);
        let _ = std::fs::remove_dir_all(dir);
    }
}
