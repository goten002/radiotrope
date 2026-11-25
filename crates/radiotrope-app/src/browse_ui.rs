//! The Search Stations window: the directory's stations, the country and
//! genre filters' lists, and the flags

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, Model, ModelRc, SharedPixelBuffer, VecModel};

use radiotrope_app::config::ui::SEARCH_PAGE_SIZE;
use radiotrope_app::data::favorites::{FavoritesManager, PlayMetadata};
use radiotrope_app::data::types::Station;
use radiotrope_app::error::ServiceProblem;
use radiotrope_app::network::browse_logos::BrowseLogos;
use radiotrope_app::providers::types::{CategoryType, SearchResults, StationFilter};
use radiotrope_app::providers::{RadiotropeProvider, StationProvider};

use crate::context::{lock, AppContext};
use crate::favorites_list::refresh_favorites;
use crate::player_ui::play_station_with_metadata;
use crate::{row_logos, App, BrowseStation, FilterEntry};

/// Wire the Search Stations window: searching and paging, the rows'
/// logos, the star and Play on a row, and the country and genre filters
pub(crate) fn setup(ui: &App, ctx: &AppContext, browse_logos: Arc<BrowseLogos>) {
    let (favorites, logo_service) = (&ctx.favorites, &ctx.logos);
    // Generation counter for browse logo fetches (to cancel stale requests)
    let browse_logo_gen = Arc::new(AtomicU64::new(0));

    // What the station browser shows, and how far "Load More" has paged
    let browse_state = Arc::new(Mutex::new((BrowseQuery::default(), 0usize)));

    let row_logos = row_logos::RowLogos::start(
        ui.as_weak(),
        browse_logos,
        logo_service.clone(),
        browse_logo_gen.clone(),
    );
    {
        let row_logos = row_logos.clone();
        let ctx = ctx.clone();
        ui.on_browse_rows_changed(move || {
            if let Some(ui) = ctx.ui.upgrade() {
                row_logos.refresh(&ui);
            }
        });
    }
    {
        let row_logos = row_logos.clone();
        let ctx = ctx.clone();
        ui.on_browse_closed(move || {
            if let Some(ui) = ctx.ui.upgrade() {
                row_logos.release(&ui);
                row_logos::return_freed_memory();
            }
        });
    }

    // search-stations callback: a name query within the filters (nothing
    // given shows the most played stations)
    {
        let state = browse_state.clone();
        let gen = browse_logo_gen.clone();
        let row_logos = row_logos.clone();
        let ctx = ctx.clone();
        ui.on_search_stations(move |query, country, genre| {
            let query = BrowseQuery {
                name: query.trim().to_string(),
                country: country.to_string(),
                genre: genre.to_string(),
            };
            start_browse(&ctx.ui, &state, &gen, &row_logos, &ctx.favorites, query);
        });
    }

    // load-more-stations callback
    {
        let ui_weak = ui.as_weak();
        let state = browse_state.clone();
        let gen = browse_logo_gen.clone();
        let row_logos = row_logos.clone();
        let favs = favorites.clone();
        ui.on_load_more_stations(move || {
            // The offset moves on only once the page is in the list, so a
            // list put aside meanwhile keeps the offset of the rows it holds
            let (query, offset) = {
                let s = lock(&state);
                (s.0.clone(), s.1 + SEARCH_PAGE_SIZE)
            };
            let my_gen = gen.load(Ordering::Relaxed);
            let ui_weak = ui_weak.clone();
            let gen = gen.clone();
            let row_logos = row_logos.clone();
            let favs = favs.clone();
            let state = state.clone();
            std::thread::Builder::new()
                .name("load-more".into())
                .spawn(move || {
                    let results = fetch_browse_page(&query, offset);
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = ui_weak.upgrade() else { return };
                        // A new search or the other mode's list replaced this
                        // one meanwhile (and cleared its loading mark)
                        if gen.load(Ordering::Relaxed) != my_gen {
                            return;
                        }
                        ui.set_search_loading_more(false);
                        match results {
                            Ok(results) => {
                                lock(&state).1 = offset;
                                show_browse_results(&ui, results, true, &favs, &row_logos)
                            }
                            // The offset stayed, so the retry asks for this page again
                            Err(e) => show_browse_error(&ui, &e),
                        }
                    });
                })
                .ok();
        });
    }

    // Try the failed browser request again: the first page, or the next one
    {
        let state = browse_state.clone();
        let gen = browse_logo_gen.clone();
        let row_logos = row_logos.clone();
        let ctx = ctx.clone();
        ui.on_retry_browse(move || {
            let Some(ui) = ctx.ui.upgrade() else { return };
            if ui.get_search_loading() || ui.get_search_loading_more() {
                return;
            }
            ui.set_search_error(Default::default());
            if ui.get_search_results().row_count() == 0 {
                let query = lock(&state).0.clone();
                start_browse(&ctx.ui, &state, &gen, &row_logos, &ctx.favorites, query);
            } else {
                ui.set_search_loading_more(true);
                ui.invoke_load_more_stations();
            }
        });
    }

    // Star on a search result: add the station to favorites, or remove it
    {
        let row_logos = row_logos.clone();
        let ctx = ctx.clone();
        ui.on_toggle_browse_favorite(move |item| {
            let Some(ui) = ctx.ui.upgrade() else { return };
            let mut f = lock(&ctx.favorites);
            let existing = f
                .find_match(&item.url, Some(item.provider_id.as_str()))
                .map(|fav| fav.id());
            let mut removed = None;
            match existing {
                // One click removes; the Undo offered for a favorite
                // removed from the list puts it back
                Some(id) => removed = crate::favorites_ui::remove_keeping_place(&mut f, &id),
                None => {
                    use radiotrope_app::data::types::Favorite;
                    // With every stream the directory lists, and the one
                    // the row plays as its default
                    let fav = Favorite::from_station(station_of(&item));
                    if !item.logo_url.is_empty() {
                        // The browser already has the logo: no new download
                        if let Some(png) = row_logos.logos().cached_png(&item.logo_url) {
                            let _ = ctx.logos.cache().put_logo(&fav, &png);
                        }
                    }
                    let _ = f.add(fav);
                    // Starred again by hand: its Undo has nothing left to do
                    crate::favorites_ui::forget_undo_of(&ui, |removed| {
                        removed.url() == item.url.as_str()
                            || (!item.provider_id.is_empty()
                                && removed.station.provider_id.as_deref()
                                    == Some(item.provider_id.as_str()))
                    });
                }
            }
            let current = ui.get_station_url();
            ui.set_is_station_favorited(!current.is_empty() && f.is_favorite(&current));
            drop(f);
            crate::favorites_ui::save_favorites();
            refresh_favorites(&ui, &ctx.favorites, &ctx.logos);
            crate::favorites_ui::offer_undo(&ui, removed);
        });
    }

    // A filter's list opens: the countries or genres. Once loaded they
    // show at once, and the directory is asked again behind them.
    {
        let ui_weak = ui.as_weak();
        ui.on_load_filter_entries(move |kind| {
            let Some(ui) = ui_weak.upgrade() else { return };
            if !show_filter_entries(&ui) {
                ui.set_filter_entries(ModelRc::default());
                ui.set_filter_entries_error(Default::default());
                ui.set_filter_entries_loading(true);
            }
            let ui_weak = ui_weak.clone();
            let kind = kind.to_string();
            std::thread::Builder::new()
                .name("load-filters".into())
                .spawn(move || {
                    let results = browse_directory().and_then(|p| p.browse_categories());
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = ui_weak.upgrade() else { return };
                        match results {
                            Ok(categories) => {
                                // One answer has both lists
                                for kind in ["country", "genre"] {
                                    keep_filter_entries(kind, filter_entries(kind, &categories));
                                }
                                show_filter_entries(&ui);
                            }
                            // Another list is open by now, or an older
                            // answer is on screen: it stays
                            Err(e) => {
                                if ui.get_filter_kind() == kind.as_str()
                                    && ui.get_filter_entries_loading()
                                {
                                    // The same words as the search window
                                    let problem = ServiceProblem::of(&e);
                                    ui.set_filter_entries_error(problem.message().into());
                                    ui.set_filter_entries_loading(false);
                                }
                            }
                        }
                    });
                })
                .ok();
        });
    }
    install_filter(ui);

    // play-station callback
    {
        let ctx = ctx.clone();
        ui.on_play_station(move |station| {
            if let Some(ui) = ctx.ui.upgrade() {
                // Its play is counted once it plays (Report Play Count)
                if !station.provider_id.is_empty() {
                    crate::app::controller::play_report::picked_from_directory(
                        &Station::new(station.name.as_str(), station.url.as_str()).with_provider(
                            radiotrope_app::providers::radiotrope::PROVIDER_ID,
                            Some(station.provider_id.to_string()),
                        ),
                    );
                }
                // Its default stream, with the others to fall back to
                play_station_with_metadata(&ui, &ctx, PlayMetadata::of(&station_of(&station)));
            }
        });
    }

    // A stream picked from a row's menu: the row plays that one from now
    // on, and so does the station's favorite, if it has one
    {
        let ctx = ctx.clone();
        ui.on_pick_browse_stream(move |row, stream| {
            let Some(ui) = ctx.ui.upgrade() else { return };
            let model = ui.get_search_results();
            let Some(item) = usize::try_from(row)
                .ok()
                .and_then(|row| model.row_data(row))
            else {
                return;
            };
            let item = with_stream(item, stream);
            model.set_row_data(row as usize, item.clone());
            let changed = pick_favorite_stream(&mut lock(&ctx.favorites), &item);
            if changed {
                crate::favorites_ui::save_favorites();
                refresh_favorites(&ui, &ctx.favorites, &ctx.logos);
            }
        });
    }
}

/// What the station browser is listing: stations whose name matches,
/// within the filters. Each part may be empty; all empty lists the most
/// played stations.
#[derive(Clone, Default)]
struct BrowseQuery {
    pub(crate) name: String,
    /// ISO 3166-1 code
    pub(crate) country: String,
    /// The directory's genre slug
    pub(crate) genre: String,
}

impl BrowseQuery {
    pub(crate) fn filter(&self) -> StationFilter {
        let given = |v: &str| Some(v.to_string()).filter(|v| !v.is_empty());
        StationFilter {
            name: given(&self.name),
            country: given(&self.country),
            genre: given(&self.genre),
            ..Default::default()
        }
    }
}

/// Fetch one page of browser results from the Radiotrope directory
fn fetch_browse_page(
    query: &BrowseQuery,
    offset: usize,
) -> radiotrope_app::error::Result<SearchResults> {
    browse_directory()?.search_filtered(&query.filter(), SEARCH_PAGE_SIZE, offset)
}

/// The directory the station browser lists, made once: it remembers where
/// each list's next page starts
fn browse_directory() -> radiotrope_app::error::Result<Arc<RadiotropeProvider>> {
    static DIRECTORY: std::sync::OnceLock<Arc<RadiotropeProvider>> = std::sync::OnceLock::new();
    if let Some(directory) = DIRECTORY.get() {
        return Ok(directory.clone());
    }
    let directory = Arc::new(RadiotropeProvider::new()?);
    Ok(DIRECTORY.get_or_init(|| directory).clone())
}

/// Replace the browser list with the first page of `query`, fetched in the
/// background. Results of an older request that finish later are dropped.
fn start_browse(
    ui_weak: &slint::Weak<App>,
    state: &Arc<Mutex<(BrowseQuery, usize)>>,
    gen: &Arc<AtomicU64>,
    row_logos: &row_logos::RowLogos,
    favs: &Arc<Mutex<FavoritesManager>>,
    query: BrowseQuery,
) {
    *lock(state) = (query.clone(), 0);
    let my_gen = gen.fetch_add(1, Ordering::Relaxed) + 1;
    // Clear old results immediately
    if let Some(ui) = ui_weak.upgrade() {
        ui.set_search_results(ModelRc::default());
        ui.set_browse_logos(ModelRc::default());
        ui.set_has_more(false);
        ui.set_search_error(Default::default());
        ui.set_search_loading(true);
        // A page still loading for the old list is dropped when it comes
        ui.set_search_loading_more(false);
    }
    let ui_weak = ui_weak.clone();
    let gen = gen.clone();
    let row_logos = row_logos.clone();
    let favs = favs.clone();
    std::thread::Builder::new()
        .name("station-browse".into())
        .spawn(move || {
            let results = fetch_browse_page(&query, 0);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                if gen.load(Ordering::Relaxed) != my_gen {
                    return;
                }
                match results {
                    Ok(results) => show_browse_results(&ui, results, false, &favs, &row_logos),
                    Err(e) => show_browse_error(&ui, &e),
                }
                ui.set_search_loading(false);
            });
        })
        .ok();
}

/// Show a page of browser results, appending to the list or replacing it,
/// then load the logos of the rows on screen
fn show_browse_results(
    ui: &App,
    results: SearchResults,
    append: bool,
    favs: &Arc<Mutex<FavoritesManager>>,
    row_logos: &row_logos::RowLogos,
) {
    let mut new_items: Vec<BrowseStation> =
        results.stations.iter().map(station_to_browse).collect();
    {
        let f = lock(favs);
        for item in &mut new_items {
            item.is_favorite = browse_is_favorite(&f, item);
            if item.is_favorite {
                *item = show_favorite_default(&f, item.clone());
            }
        }
    }
    let (mut items, mut logos) = if append {
        (
            ui.get_search_results().iter().collect::<Vec<_>>(),
            ui.get_browse_logos().iter().collect::<Vec<_>>(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    // No logos yet: rows get theirs as they come on screen
    logos.resize(items.len() + new_items.len(), slint::Image::default());
    items.extend(new_items);
    ui.set_search_results(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
    ui.set_browse_logos(ModelRc::from(std::rc::Rc::new(VecModel::from(logos))));
    ui.set_has_more(results.has_more);
    ui.set_search_error(Default::default());
    BROWSE_FAILURES.set(0);
    row_logos.refresh(ui);
}

thread_local! {
    /// Station browser requests failed in a row, which spaces out the retries
    static BROWSE_FAILURES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Say why a station browser request failed, and when it is tried again
fn show_browse_error(ui: &App, e: &radiotrope_app::error::AppError) {
    let failures = BROWSE_FAILURES.get() + 1;
    BROWSE_FAILURES.set(failures);
    let problem = ServiceProblem::of(e);
    ui.set_search_error(problem.message().into());
    ui.set_search_retry_in(problem.retry_delay_secs(failures) as i32);
}

fn station_to_browse(s: &Station) -> BrowseStation {
    BrowseStation {
        name: s.name.as_str().into(),
        url: s.url.as_str().into(),
        logo_url: s.logo_url.as_deref().unwrap_or("").into(),
        country: s.country.as_deref().unwrap_or("").into(),
        genres: genre_names(s).into(),
        // radio-browser said "UNKNOWN" when it had no codec; show no badge
        codec: s
            .codec
            .as_deref()
            .filter(|c| !c.eq_ignore_ascii_case("unknown"))
            .unwrap_or("")
            .into(),
        bitrate: s.bitrate.unwrap_or(0) as i32,
        streams: crate::stream_edit::rows_of(s),
        provider_id: s.provider_id.as_deref().unwrap_or("").into(),
        is_favorite: false,
    }
}

/// The station a row stands for: its streams, with the one the row plays
/// (the directory's first, or the one picked from the row's stream menu)
/// as its default
fn station_of(item: &BrowseStation) -> Station {
    let streams: Vec<_> = item
        .streams
        .iter()
        .map(|row| crate::stream_edit::stream_of(&row))
        .collect();
    let mut station = Station::new(item.name.as_str(), item.url.as_str())
        .with_audio_info(
            Some(item.codec.to_string()).filter(|c| !c.is_empty()),
            u32::try_from(item.bitrate).ok().filter(|b| *b > 0),
        )
        .with_metadata(
            Some(item.country.to_string()).filter(|c| !c.is_empty()),
            None,
            Default::default(),
        )
        .with_provider(
            radiotrope_app::providers::radiotrope::PROVIDER_ID,
            Some(item.provider_id.to_string()).filter(|id| !id.is_empty()),
        )
        .with_logo_opt(Some(item.logo_url.to_string()).filter(|l| !l.is_empty()));
    if streams.iter().any(|s| s.url == station.url) {
        station.set_streams(streams, item.url.as_str());
    }
    station
}

/// The row with the stream at `index` of its streams as the one it plays;
/// unchanged when it has no such stream
fn with_stream(mut item: BrowseStation, index: i32) -> BrowseStation {
    let picked = usize::try_from(index)
        .ok()
        .and_then(|index| item.streams.row_data(index));
    if let Some(stream) = picked {
        item.url = stream.url;
        item.codec = stream.codec;
        item.bitrate = stream.bitrate;
    }
    item
}

/// A favorite plays its own default stream, whatever the directory lists
/// first: its row in the results shows that one as the stream it plays
fn show_favorite_default(favs: &FavoritesManager, item: BrowseStation) -> BrowseStation {
    let default = favs
        .find_match(&item.url, Some(item.provider_id.as_str()))
        .map(|fav| fav.url().to_string());
    let index = default.and_then(|url| item.streams.iter().position(|row| row.url == url.as_str()));
    match index.and_then(|index| i32::try_from(index).ok()) {
        Some(index) => with_stream(item, index),
        None => item,
    }
}

/// A stream was picked from a row's menu, to be played. A favorite takes
/// it as its default (and into its streams, when it didn't have it), so
/// it is what the favorite plays from now on. True when a favorite changed.
fn pick_favorite_stream(favs: &mut FavoritesManager, item: &BrowseStation) -> bool {
    let Some(fav) = favs
        .find_match(&item.url, Some(item.provider_id.as_str()))
        .cloned()
    else {
        return false;
    };
    if fav.url() == item.url.as_str() {
        return false;
    }
    let mut streams = fav.station.streams.clone();
    if !streams.iter().any(|s| s.url == item.url.as_str()) {
        let picked = station_of(item).default_stream();
        streams.push(picked);
    }
    let update =
        radiotrope_app::data::types::FavoriteUpdate::new().streams(streams, item.url.as_str());
    favs.update(&fav.id(), update).is_ok()
}

/// How many genres a row names
const ROW_GENRES: usize = 3;

/// A station's genres for its row, by name ("Blues, Jazz"): the first few,
/// A to Z (the station keeps them in no order)
fn genre_names(station: &Station) -> String {
    let mut names: Vec<&str> = station
        .genres
        .iter()
        .map(|genre| genre.trim())
        .filter(|genre| !genre.is_empty())
        .collect();
    names.sort_by_key(|name| name.to_lowercase());
    names.truncate(ROW_GENRES);
    names.join(", ")
}

/// A search result is a favorite by URL or by the provider's station ID
fn browse_is_favorite(favs: &FavoritesManager, item: &BrowseStation) -> bool {
    favs.find_match(&item.url, Some(item.provider_id.as_str()))
        .is_some()
}

/// Update the stars on the search results after favorites changed
pub(crate) fn mark_browse_favorites(ui: &App, favs: &FavoritesManager) {
    let model = ui.get_search_results();
    for i in 0..model.row_count() {
        let Some(mut item) = model.row_data(i) else {
            continue;
        };
        let fav = browse_is_favorite(favs, &item);
        if item.is_favorite != fav {
            item.is_favorite = fav;
            model.set_row_data(i, item);
        }
    }
    // Lists put aside for the other mode get theirs when shown again
}

// Decoded flag images keyed by country code. Flags are tiny and shared by many
// rows, so each is decoded once. Runs on the Slint event-loop thread only.
thread_local! {
    static FLAG_IMAGE_CACHE: std::cell::RefCell<HashMap<String, slint::Image>> =
        std::cell::RefCell::new(HashMap::new());
}

thread_local! {
    /// The directory's countries and genres, by kind ("country", "genre"),
    /// once loaded; a filter's list shows the ones that match its text
    static ALL_FILTER_ENTRIES: std::cell::RefCell<HashMap<String, Vec<FilterEntry>>> =
        std::cell::RefCell::new(HashMap::new());
}

/// The directory's countries or genres (`kind`: "country" or "genre") as
/// filter entries, A to Z. Each has the name to show (the directory's own
/// for a genre, the English one for a country code) and the code to ask
/// the directory for (the genre's slug, the country's ISO code).
fn filter_entries(
    kind: &str,
    categories: &[radiotrope_app::providers::types::Category],
) -> Vec<FilterEntry> {
    let wanted = match kind {
        "genre" => CategoryType::Genre,
        _ => CategoryType::Country,
    };
    let mut entries: Vec<FilterEntry> = categories
        .iter()
        .filter(|c| c.category_type == wanted)
        .map(|c| {
            let code = match wanted {
                CategoryType::Country => c.code.clone().unwrap_or_default(),
                _ => c.id.clone(),
            };
            FilterEntry {
                flag: if wanted == CategoryType::Country {
                    flag_image(c.code.as_deref(), Some(&c.name))
                } else {
                    Default::default()
                },
                name: filter_name(wanted, &c.name, &code).into(),
                code: code.into(),
                station_count: c.station_count.unwrap_or(0) as i32,
            }
        })
        // One the directory can't be asked for is no use as a filter
        .filter(|e| !e.code.is_empty())
        .collect();
    entries.sort_by_key(|e| e.name.to_lowercase());
    entries
}

/// The name a filter entry shows. The directory names its genres ("Hip
/// Hop" for `hip-hop`) and the country table names the codes; should one
/// come without a name (empty, or the slug or code itself), a genre gets a
/// readable one made from its slug and a country keeps its code.
fn filter_name(kind: CategoryType, name: &str, code: &str) -> String {
    let name = name.trim();
    if kind == CategoryType::Genre && (name.is_empty() || name == code) {
        return name_from_slug(code);
    }
    name.to_string()
}

/// "hip-hop" -> "Hip Hop": a genre's slug as a name, for one the directory
/// gave no name
fn name_from_slug(slug: &str) -> String {
    slug.split(['-', '_'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut letters = word.chars();
            match letters.next() {
                // "and" stays small inside a name ("Drum and Bass")
                _ if word == "and" => word.to_string(),
                Some(first) => first.to_uppercase().chain(letters).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Keep a kind's entries for its filter's list
pub(crate) fn keep_filter_entries(kind: &str, entries: Vec<FilterEntry>) {
    ALL_FILTER_ENTRIES.with_borrow_mut(|all| all.insert(kind.to_string(), entries));
}

/// Narrow the open filter list as its text box is typed in
pub(crate) fn install_filter(ui: &App) {
    let ui_weak = ui.as_weak();
    ui.on_filter_filter_entries(move |_| {
        if let Some(ui) = ui_weak.upgrade() {
            show_filter_entries(&ui);
        }
    });
}

/// Show the open filter list's entries that match its text box, best match
/// first. False while that kind has not been loaded.
pub(crate) fn show_filter_entries(ui: &App) -> bool {
    let kind = ui.get_filter_kind();
    let text = ui.get_filter_entries_text();
    let entries = ALL_FILTER_ENTRIES.with_borrow(|all| {
        all.get(kind.as_str())
            .map(|all| matching_entries(all, &text))
    });
    let Some(entries) = entries else { return false };
    ui.set_filter_entries(ModelRc::from(std::rc::Rc::new(VecModel::from(entries))));
    ui.set_filter_entries_error(Default::default());
    ui.set_filter_entries_loading(false);
    true
}

/// The entries a typed text finds, whatever the case: names that start
/// with it first, then names with a word that starts with it, then names
/// that have it anywhere, each group A to Z as the list is. A country is
/// also found by its code ("gb"). Nothing typed finds everything.
fn matching_entries(all: &[FilterEntry], text: &str) -> Vec<FilterEntry> {
    let needle = text.trim().to_lowercase();
    if needle.is_empty() {
        return all.to_vec();
    }
    let rank = |entry: &FilterEntry| -> Option<u8> {
        let name = entry.name.to_lowercase();
        if name.starts_with(&needle) || entry.code.to_lowercase() == needle {
            Some(0)
        } else if name
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| word.starts_with(&needle))
        {
            Some(1)
        } else if name.contains(&needle) {
            Some(2)
        } else {
            None
        }
    };
    let mut found: Vec<(u8, &FilterEntry)> = all
        .iter()
        .filter_map(|entry| rank(entry).map(|rank| (rank, entry)))
        .collect();
    // Stable: each rank keeps the list's order
    found.sort_by_key(|(rank, _)| *rank);
    found.into_iter().map(|(_, entry)| entry.clone()).collect()
}

/// Flag image for a country (an empty image if there is no flag for it)
pub(crate) fn flag_image(country_code: Option<&str>, country: Option<&str>) -> slint::Image {
    use radiotrope_app::data::flags;

    let Some(code) = flags::flag_code(country_code, country) else {
        return Default::default();
    };
    FLAG_IMAGE_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry(code)
            .or_insert_with_key(|code| {
                flags::flag_png(code)
                    .and_then(|png| image::load_from_memory(png).ok())
                    .map(|img| {
                        let rgba = img.to_rgba8();
                        let pixel_buf = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                            rgba.as_raw(),
                            rgba.width(),
                            rgba.height(),
                        );
                        slint::Image::from_rgba8(pixel_buf)
                    })
                    .unwrap_or_default()
            })
            .clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_entries_take_the_kind_asked_for_with_the_code_the_directory_wants() {
        use radiotrope_app::providers::types::Category;
        let categories = || {
            vec![
                Category::new("rock", "Rock", CategoryType::Genre).with_station_count(9),
                Category::new("ambient", "Ambient", CategoryType::Genre),
                Category::new("GR", "Greece", CategoryType::Country)
                    .with_code(Some("GR".into()))
                    .with_station_count(4),
                // No code: the directory can't be asked for it
                Category::new("Atlantis", "Atlantis", CategoryType::Country),
                Category::new("ell", "Greek", CategoryType::Language),
            ]
        };

        let genres = filter_entries("genre", &categories());
        let names: Vec<_> = genres.iter().map(|e| e.name.to_string()).collect();
        assert_eq!(names, ["Ambient", "Rock"], "A to Z");
        assert_eq!(genres[1].code, "rock", "the slug");
        assert_eq!(genres[1].station_count, 9);

        let countries = filter_entries("country", &categories());
        assert_eq!(countries.len(), 1);
        assert_eq!(
            (countries[0].name.as_str(), countries[0].code.as_str()),
            ("Greece", "GR")
        );

        // Languages aren't a filter
        assert!(filter_entries("genre", &categories())
            .iter()
            .all(|e| e.name != "Greek"));
    }

    fn entry(name: &str, code: &str) -> FilterEntry {
        FilterEntry {
            name: name.into(),
            code: code.into(),
            station_count: 1,
            flag: Default::default(),
        }
    }

    #[test]
    fn a_genre_shows_its_name_and_is_asked_for_by_its_slug() {
        use radiotrope_app::providers::types::Category;
        let genres = filter_entries(
            "genre",
            &[
                Category::new("hip-hop", "Hip Hop", CategoryType::Genre),
                Category::new("r-and-b", "R&B", CategoryType::Genre),
                // Should the directory ever send a genre without a name
                Category::new("drum-and-bass", "drum-and-bass", CategoryType::Genre),
                Category::new("lo-fi", "", CategoryType::Genre),
            ],
        );
        let shown: Vec<_> = genres
            .iter()
            .map(|e| (e.name.as_str(), e.code.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                ("Drum and Bass", "drum-and-bass"),
                ("Hip Hop", "hip-hop"),
                ("Lo Fi", "lo-fi"),
                ("R&B", "r-and-b"),
            ]
        );
        assert_eq!(name_from_slug("hip-hop"), "Hip Hop");
        assert_eq!(name_from_slug("jazz"), "Jazz");
    }

    #[test]
    fn typing_finds_names_that_start_with_it_first() {
        let all = [
            entry("Reunion", "RE"),
            entry("Tunisia", "TN"),
            entry("United Arab Emirates", "AE"),
            entry("United Kingdom", "GB"),
            entry("United States", "US"),
        ];
        let names = |text: &str| -> Vec<String> {
            matching_entries(&all, text)
                .iter()
                .map(|e| e.name.to_string())
                .collect()
        };
        assert_eq!(names("").len(), 5, "nothing typed finds everything");
        assert_eq!(
            names(" UNI "),
            [
                "United Arab Emirates",
                "United Kingdom",
                "United States",
                "Reunion",
                "Tunisia"
            ]
        );
        // A later word that starts with it comes before a match inside one
        assert_eq!(names("k"), ["United Kingdom"]);
        assert_eq!(names("ar"), ["United Arab Emirates"]);
        assert_eq!(names("united k"), ["United Kingdom"]);
        // A country by its code
        assert_eq!(names("gb"), ["United Kingdom"]);
        assert!(names("atlantis").is_empty());
    }

    #[test]
    fn a_row_names_a_few_genres_in_a_steady_order() {
        let mut station = Station::new("Jazz FM", "http://jazz.test");
        assert_eq!(station_to_browse(&station).genres, "");
        station.genres = ["Soul", "Jazz", "Hip Hop", "Blues"]
            .map(String::from)
            .into();
        assert_eq!(station_to_browse(&station).genres, "Blues, Hip Hop, Jazz");
    }

    #[test]
    fn a_browse_query_asks_only_for_what_is_given() {
        assert_eq!(BrowseQuery::default().filter(), StationFilter::default());
        let query = BrowseQuery {
            name: "kiss".into(),
            country: "GR".into(),
            ..Default::default()
        };
        let filter = query.filter();
        assert_eq!(filter.name.as_deref(), Some("kiss"));
        assert_eq!(filter.country.as_deref(), Some("GR"));
        assert_eq!(filter.genre, None);
    }
}
