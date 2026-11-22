//! The Search Stations window: the directory's stations, the country and
//! genre pickers, and the flags

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
use crate::{play_station_with_metadata, row_logos, App, BrowseStation, FilterEntry};

/// Wire the Search Stations window: searching and paging, the rows'
/// logos, the star and Play on a row, and the country and genre pickers
pub(crate) fn setup(ui: &App, ctx: &AppContext, browse_logos: Arc<BrowseLogos>) {
    let (ui_tx, shared_state) = (&ctx.tx, &ctx.state);
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
        let ui_weak = ui.as_weak();
        let row_logos = row_logos.clone();
        ui.on_browse_rows_changed(move || {
            if let Some(ui) = ui_weak.upgrade() {
                row_logos.refresh(&ui);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let row_logos = row_logos.clone();
        ui.on_browse_closed(move || {
            if let Some(ui) = ui_weak.upgrade() {
                row_logos.release(&ui);
                row_logos::return_freed_memory();
            }
        });
    }

    // search-stations callback: a name query within the filters (nothing
    // given shows the most played stations)
    {
        let ui_weak = ui.as_weak();
        let state = browse_state.clone();
        let gen = browse_logo_gen.clone();
        let row_logos = row_logos.clone();
        let favs = favorites.clone();
        ui.on_search_stations(move |query, country, genre| {
            let query = BrowseQuery {
                name: query.trim().to_string(),
                country: country.to_string(),
                genre: genre.to_string(),
            };
            start_browse(&ui_weak, &state, &gen, &row_logos, &favs, query);
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
        let ui_weak = ui.as_weak();
        let state = browse_state.clone();
        let gen = browse_logo_gen.clone();
        let row_logos = row_logos.clone();
        let favs = favorites.clone();
        ui.on_retry_browse(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if ui.get_search_loading() || ui.get_search_loading_more() {
                return;
            }
            ui.set_search_error(Default::default());
            if ui.get_search_results().row_count() == 0 {
                let query = lock(&state).0.clone();
                start_browse(&ui_weak, &state, &gen, &row_logos, &favs, query);
            } else {
                ui.set_search_loading_more(true);
                ui.invoke_load_more_stations();
            }
        });
    }

    // Star on a search result: add the station to favorites, or remove it
    {
        let ui_weak = ui.as_weak();
        let favs = favorites.clone();
        let logo_svc = logo_service.clone();
        let row_logos = row_logos.clone();
        ui.on_toggle_browse_favorite(move |item| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut f = lock(&favs);
            let existing = f
                .find_match(&item.url, Some(item.provider_id.as_str()))
                .map(|fav| fav.url().to_string());
            match existing {
                Some(url) => {
                    let _ = f.remove_by_url(&url);
                }
                None => {
                    use radiotrope_app::data::types::Favorite;
                    let mut fav = Favorite::new(item.name.as_str(), item.url.as_str())
                        .with_metadata(
                            Some(item.country.to_string()).filter(|c| !c.is_empty()),
                            None,
                            Default::default(),
                        )
                        .with_provider(
                            radiotrope_app::providers::radiotrope::PROVIDER_ID,
                            Some(item.provider_id.to_string()).filter(|id| !id.is_empty()),
                        )
                        .with_audio_info(
                            Some(item.codec.to_string()).filter(|c| !c.is_empty()),
                            u32::try_from(item.bitrate).ok().filter(|b| *b > 0),
                        );
                    if !item.logo_url.is_empty() {
                        fav = fav.with_logo(item.logo_url.as_str());
                        // The browser already has the logo: no new download
                        if let Some(png) = row_logos.logos().cached_png(&item.logo_url) {
                            let _ = logo_svc.cache().put_logo(&fav, &png);
                        }
                    }
                    let _ = f.add(fav);
                }
            }
            let _ = f.save();
            let current = ui.get_station_url();
            ui.set_is_station_favorited(!current.is_empty() && f.is_favorite(&current));
            drop(f);
            refresh_favorites(&ui, &favs, &logo_svc);
        });
    }

    // The countries or genres the station browser's filter picker lists
    {
        let ui_weak = ui.as_weak();
        ui.on_load_filter_entries(move |kind| {
            let ui_weak = ui_weak.clone();
            let kind = kind.to_string();
            std::thread::Builder::new()
                .name("load-filters".into())
                .spawn(move || {
                    let results = browse_directory().and_then(|p| p.browse_categories());
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = ui_weak.upgrade() else { return };
                        // The picker moved on to another kind meanwhile
                        if ui.get_filter_kind() != kind.as_str() {
                            return;
                        }
                        match results {
                            Ok(categories) => {
                                let entries = filter_entries(&kind, categories);
                                ALL_FILTER_ENTRIES.with(|all| *all.borrow_mut() = entries);
                                show_filter_entries(&ui, &ui.get_filter_entries_text());
                                ui.set_filter_entries_error(Default::default());
                            }
                            Err(e) => {
                                // The same words as the search window
                                let problem = ServiceProblem::of(&e);
                                ui.set_filter_entries_error(problem.message().into());
                            }
                        }
                        ui.set_filter_entries_loading(false);
                    });
                })
                .ok();
        });
    }

    // Narrow the filter picker's list by name
    {
        let ui_weak = ui.as_weak();
        ui.on_filter_filter_entries(move |text| {
            if let Some(ui) = ui_weak.upgrade() {
                show_filter_entries(&ui, &text);
            }
        });
    }

    // play-station callback
    {
        let play_tx = ui_tx.clone();
        let logo_svc = logo_service.clone();
        let favs = favorites.clone();
        let play_state = shared_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_play_station(move |station| {
            if let Some(ui) = ui_weak.upgrade() {
                let logo_url = station.logo_url.to_string();
                let country = station.country.to_string();
                play_station_with_metadata(
                    &ui,
                    &play_tx,
                    &play_state,
                    &favs,
                    &logo_svc,
                    PlayMetadata {
                        url: station.url.to_string(),
                        name: Some(station.name.to_string()),
                        logo_url: if logo_url.is_empty() {
                            None
                        } else {
                            Some(logo_url)
                        },
                        country: if country.is_empty() {
                            None
                        } else {
                            Some(country)
                        },
                        provider_id: Some(station.provider_id.to_string())
                            .filter(|id| !id.is_empty()),
                    },
                );
                report_browse_play(&station);
            }
        });
    }
}

/// What the station browser is listing: stations whose name matches,
/// within the filters. Each part may be empty; all empty lists the most
/// played stations.
#[derive(Clone, Default)]
pub(crate) struct BrowseQuery {
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
pub(crate) fn fetch_browse_page(
    query: &BrowseQuery,
    offset: usize,
) -> radiotrope_app::error::Result<SearchResults> {
    browse_directory()?.search_filtered(&query.filter(), SEARCH_PAGE_SIZE, offset)
}

/// The directory the station browser lists, made once: it remembers where
/// each list's next page starts
pub(crate) fn browse_directory() -> radiotrope_app::error::Result<Arc<RadiotropeProvider>> {
    static DIRECTORY: std::sync::OnceLock<Arc<RadiotropeProvider>> = std::sync::OnceLock::new();
    if let Some(directory) = DIRECTORY.get() {
        return Ok(directory.clone());
    }
    let directory = Arc::new(RadiotropeProvider::new()?);
    Ok(DIRECTORY.get_or_init(|| directory).clone())
}

/// Count a play of a station browser result in the directory, which
/// orders its lists by plays. Nobody waits on it, and a failure costs
/// only the count.
pub(crate) fn report_browse_play(item: &BrowseStation) {
    if item.provider_id.is_empty() {
        return;
    }
    let station = Station::new(item.name.as_str(), item.url.as_str()).with_provider(
        radiotrope_app::providers::radiotrope::PROVIDER_ID,
        Some(item.provider_id.to_string()),
    );
    let _ = std::thread::Builder::new()
        .name("report-play".into())
        .spawn(move || {
            let _ = browse_directory().and_then(|d| d.report_click(&station));
        });
}

/// Replace the browser list with the first page of `query`, fetched in the
/// background. Results of an older request that finish later are dropped.
pub(crate) fn start_browse(
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
pub(crate) fn show_browse_results(
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
    pub(crate) static BROWSE_FAILURES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Say why a station browser request failed, and when it is tried again
pub(crate) fn show_browse_error(ui: &App, e: &radiotrope_app::error::AppError) {
    let failures = BROWSE_FAILURES.get() + 1;
    BROWSE_FAILURES.set(failures);
    let problem = ServiceProblem::of(e);
    ui.set_search_error(problem.message().into());
    ui.set_search_retry_in(problem.retry_delay_secs(failures) as i32);
}

pub(crate) fn station_to_browse(s: &Station) -> BrowseStation {
    BrowseStation {
        name: s.name.as_str().into(),
        url: s.url.as_str().into(),
        logo_url: s.logo_url.as_deref().unwrap_or("").into(),
        country: s.country.as_deref().unwrap_or("").into(),
        // radio-browser said "UNKNOWN" when it had no codec; show no badge
        codec: s
            .codec
            .as_deref()
            .filter(|c| !c.eq_ignore_ascii_case("unknown"))
            .unwrap_or("")
            .into(),
        bitrate: s.bitrate.unwrap_or(0) as i32,
        provider_id: s.provider_id.as_deref().unwrap_or("").into(),
        is_favorite: false,
    }
}

/// A search result is a favorite by URL or by the provider's station ID
pub(crate) fn browse_is_favorite(favs: &FavoritesManager, item: &BrowseStation) -> bool {
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
    pub(crate) static FLAG_IMAGE_CACHE: std::cell::RefCell<HashMap<String, slint::Image>> =
        std::cell::RefCell::new(HashMap::new());
}

thread_local! {
    /// Every entry of the kind the filter picker loaded last; it shows a
    /// filtered copy
    pub(crate) static ALL_FILTER_ENTRIES: std::cell::RefCell<Vec<FilterEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The directory's countries or genres (`kind`: "country" or "genre") as
/// filter picker entries, A to Z
pub(crate) fn filter_entries(
    kind: &str,
    categories: Vec<radiotrope_app::providers::types::Category>,
) -> Vec<FilterEntry> {
    let wanted = match kind {
        "genre" => CategoryType::Genre,
        _ => CategoryType::Country,
    };
    let mut entries: Vec<FilterEntry> = categories
        .into_iter()
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
                code: code.into(),
                name: c.name.as_str().into(),
                station_count: c.station_count.unwrap_or(0) as i32,
            }
        })
        // One the directory can't be asked for is no use as a filter
        .filter(|e| !e.code.is_empty())
        .collect();
    entries.sort_by_key(|e| e.name.to_lowercase());
    entries
}

/// Show the filter picker entries whose name contains `filter`
/// (case-insensitive)
pub(crate) fn show_filter_entries(ui: &App, filter: &str) {
    let filter = filter.trim().to_lowercase();
    let entries: Vec<FilterEntry> = ALL_FILTER_ENTRIES.with(|all| {
        all.borrow()
            .iter()
            .filter(|e| filter.is_empty() || e.name.to_lowercase().contains(&filter))
            .cloned()
            .collect()
    });
    ui.set_filter_entries(ModelRc::from(std::rc::Rc::new(VecModel::from(entries))));
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

        let genres = filter_entries("genre", categories());
        let names: Vec<_> = genres.iter().map(|e| e.name.to_string()).collect();
        assert_eq!(names, ["Ambient", "Rock"], "A to Z");
        assert_eq!(genres[1].code, "rock", "the slug");
        assert_eq!(genres[1].station_count, 9);

        let countries = filter_entries("country", categories());
        assert_eq!(countries.len(), 1);
        assert_eq!(
            (countries[0].name.as_str(), countries[0].code.as_str()),
            ("Greece", "GR")
        );

        // Languages aren't a filter
        assert!(filter_entries("genre", categories())
            .iter()
            .all(|e| e.name != "Greek"));
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
