//! Stations for phones: searching the directory and the favorites

use std::collections::HashMap;

use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde::{Deserialize, Serialize};

use radiotrope_app::config::ui::SEARCH_PAGE_SIZE;
use radiotrope_app::data::types::{Favorite, Station, StationStats};
use radiotrope_app::providers::{CategoryType, SearchOrder, StationFilter};

use radiotrope_app::config::remote::MAX_IMPORT_BODY;
use radiotrope_app::data::favorites::ImportMode;

use super::server::{
    control_error, error, json, no_content, read_json, read_json_up_to, Body, Shared,
};
use crate::control::{
    check_length, sorted_genres, FavoriteEdit, Found, MAX_FAVORITES, MAX_NAME_CHARS, MAX_URL_CHARS,
};

/// Genres a station sent to the player may have
const MAX_GENRES: usize = 32;

/// Stations a search sends unless told otherwise
const DEFAULT_SEARCH_LIMIT: usize = 30;
/// Genres, countries or languages sent unless told otherwise
const DEFAULT_CATEGORY_LIMIT: usize = 100;
/// The most a category list sends
const MAX_CATEGORY_LIMIT: usize = 1000;

/// A station from the directory
#[derive(Serialize)]
struct FoundStation {
    /// Its directory id, to play it or save it as a favorite
    id: Option<String>,
    name: String,
    url: String,
    country: Option<String>,
    language: Option<String>,
    genres: Vec<String>,
    codec: Option<String>,
    bitrate_kbps: Option<u32>,
    homepage: Option<String>,
    /// Where the directory has its logo (phones load it themselves)
    logo_url: Option<String>,
    /// Its favorite id, when it is one
    favorite_id: Option<String>,
}

#[derive(Serialize)]
struct SearchResult {
    stations: Vec<FoundStation>,
    /// More may follow at offset + the stations sent
    has_more: bool,
}

/// `GET /v1/search?q=&genre=&country=&language=&codec=&min_bitrate=&order=&limit=&offset=`
pub(super) async fn search(shared: &Shared, request: &Request<Incoming>) -> Response<Body> {
    let query = query_params(request);
    let text = |key: &str| {
        query
            .get(key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let number = |key: &str| text(key).and_then(|v| v.parse::<usize>().ok());
    let order = match text("order").as_deref() {
        None | Some("popular") => SearchOrder::Popular,
        Some("votes") => SearchOrder::Votes,
        Some("trending") => SearchOrder::Trending,
        Some("bitrate") => SearchOrder::Bitrate,
        Some("name") => SearchOrder::Name,
        Some(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "order must be popular, votes, trending, bitrate or name",
            )
        }
    };
    let filter = StationFilter {
        name: text("q"),
        genre: text("genre"),
        country: text("country"),
        language: text("language"),
        codec: text("codec"),
        min_bitrate: number("min_bitrate").map(|b| b as u32),
        order,
    };
    let limit = number("limit")
        .map(|l| l.clamp(1, SEARCH_PAGE_SIZE))
        .unwrap_or(DEFAULT_SEARCH_LIMIT.min(SEARCH_PAGE_SIZE));
    let offset = number("offset").unwrap_or(0);
    match shared.control.search(filter, limit, offset).await {
        Ok(page) => json(
            StatusCode::OK,
            &SearchResult {
                stations: page.stations.into_iter().map(found_station).collect(),
                has_more: page.has_more,
            },
        ),
        Err(e) => control_error(e),
    }
}

fn found_station(
    Found {
        station,
        favorite_id,
    }: Found,
) -> FoundStation {
    FoundStation {
        id: station.provider_id.clone().filter(|id| !id.is_empty()),
        genres: sorted_genres(&station.genres),
        name: station.name,
        url: station.url,
        country: station.country,
        language: station.language,
        codec: station.codec,
        bitrate_kbps: station.bitrate.filter(|b| *b > 0),
        homepage: station.homepage,
        logo_url: station.logo_url,
        favorite_id,
    }
}

#[derive(Serialize)]
struct CategoryItem {
    /// Pass this as genre, country or language to a search
    name: String,
    /// Two-letter country code, for countries
    code: Option<String>,
    stations: Option<usize>,
}

#[derive(Serialize)]
struct CategoryList {
    categories: Vec<CategoryItem>,
}

/// `GET /v1/categories/{genre|country|language}?filter=&limit=`, largest
/// first
pub(super) async fn categories(
    shared: &Shared,
    kind: &str,
    request: &Request<Incoming>,
) -> Response<Body> {
    let wanted = match kind {
        "genre" => CategoryType::Genre,
        "country" => CategoryType::Country,
        "language" => CategoryType::Language,
        _ => {
            return error(
                StatusCode::NOT_FOUND,
                "not_found",
                "Categories are genre, country or language",
            )
        }
    };
    let query = query_params(request);
    let limit = query
        .get("limit")
        .and_then(|l| l.trim().parse::<usize>().ok())
        .map(|l| l.clamp(1, MAX_CATEGORY_LIMIT))
        .unwrap_or(DEFAULT_CATEGORY_LIMIT);
    let filter = query.get("filter").cloned();
    match shared.control.categories(wanted, filter, limit).await {
        Ok(found) => json(
            StatusCode::OK,
            &CategoryList {
                categories: found
                    .into_iter()
                    .map(|c| CategoryItem {
                        name: c.name,
                        code: c.code.filter(|code| !code.is_empty()),
                        stations: c.station_count,
                    })
                    .collect(),
            },
        ),
        Err(e) => control_error(e),
    }
}

/// A favorite as phones see it
#[derive(Serialize)]
pub(super) struct FavoriteItem {
    /// Stays the same while its stream URL does
    id: String,
    name: String,
    url: String,
    /// Where to get its logo from this player, e.g. "/v1/logos/1a2b..."
    logo: Option<String>,
    /// Where the logo comes from, as the Edit Favorite dialog shows it
    logo_url: Option<String>,
    country: Option<String>,
    language: Option<String>,
    genres: Vec<String>,
    codec: Option<String>,
    bitrate_kbps: Option<u32>,
    homepage: Option<String>,
    /// The directory it came from ("radio-browser", or "manual")
    provider: String,
    /// Its id in that directory
    provider_id: Option<String>,
    play_count: u32,
    listen_seconds: u64,
    /// Unix time
    added_at: u64,
    /// Unix time
    last_played: Option<u64>,
}

impl FavoriteItem {
    fn new(fav: &Favorite, stats: StationStats) -> Self {
        let id = fav.id();
        let s = &fav.station;
        FavoriteItem {
            logo: s.logo_url.as_ref().map(|_| format!("/v1/logos/{id}")),
            id,
            name: s.name.clone(),
            url: s.url.clone(),
            logo_url: s.logo_url.clone(),
            country: s.country.clone(),
            language: s.language.clone(),
            genres: sorted_genres(&s.genres),
            codec: s.codec.clone(),
            bitrate_kbps: s.bitrate.filter(|b| *b > 0),
            homepage: s.homepage.clone(),
            provider: s.provider.clone(),
            provider_id: s.provider_id.clone(),
            play_count: stats.play_count,
            listen_seconds: stats.total_listen_time_secs,
            added_at: fav.added_at,
            last_played: stats.last_played,
        }
    }
}

#[derive(Serialize)]
struct FavoritesList {
    /// The `favorites_rev` of the state these are from
    rev: u64,
    /// In the user's own order
    favorites: Vec<FavoriteItem>,
}

/// `GET /v1/favorites`
pub(super) async fn favorites(shared: &Shared) -> Response<Body> {
    let rev = shared.control.favorites_generation().await;
    match shared.control.favorites_with_stats().await {
        Ok(favorites) => json(
            StatusCode::OK,
            &FavoritesList {
                rev,
                favorites: favorites
                    .iter()
                    .map(|(fav, stats)| FavoriteItem::new(fav, *stats))
                    .collect(),
            },
        ),
        Err(e) => control_error(e),
    }
}

/// A station to save: a directory station by its id, or one given in full
/// (a stream the user typed, the station playing)
#[derive(Deserialize)]
struct AddFavoriteBody {
    #[serde(default)]
    station_id: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    logo_url: Option<String>,
    #[serde(default)]
    country: Option<String>,
}

/// `POST /v1/favorites`; a URL already there is updated
pub(super) async fn add_favorite(shared: &Shared, request: Request<Incoming>) -> Response<Body> {
    let body: AddFavoriteBody = match read_json(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let station_id = given(&body.station_id);
    let url = given(&body.url);
    let fav = match (station_id, url) {
        (Some(id), None) => match shared.control.station(id.to_string()).await {
            Ok(station) => Favorite::from_station(station),
            Err(e) => return control_error(e),
        },
        (None, Some(url)) => {
            let name = given(&body.name).unwrap_or_default();
            let logo = given(&body.logo_url);
            let country = given(&body.country);
            let checked = check_stream(url)
                .and_then(|_| check_length("name", name, MAX_NAME_CHARS))
                .and_then(|_| check_length("logo_url", logo.unwrap_or(""), MAX_URL_CHARS))
                .and_then(|_| check_length("country", country.unwrap_or(""), MAX_NAME_CHARS));
            if let Err(text) = checked {
                return error(StatusCode::BAD_REQUEST, "bad_request", &text);
            }
            let name = if name.is_empty() {
                radiotrope_app::data::types::name_from_url(url)
            } else {
                name.to_string()
            };
            let mut station = Station::new(name, url);
            station.logo_url = logo.map(str::to_string);
            station.country = country.map(str::to_string);
            Favorite::from_station(station)
        }
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "Give station_id or url",
            )
        }
    };
    let item = FavoriteItem::new(&fav, StationStats::default());
    match shared.control.add_favorite(fav).await {
        Ok(()) => json(StatusCode::CREATED, &item),
        Err(e) => control_error(e),
    }
}

/// What an edit changes; fields left out stay as they are, and an empty
/// logo_url or country clears it
#[derive(Deserialize)]
struct EditFavoriteBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    logo_url: Option<String>,
    #[serde(default)]
    country: Option<String>,
}

/// `PATCH /v1/favorites/{id}`; answers with the favorite as it is now
/// (its id changes with its stream URL)
pub(super) async fn edit_favorite(
    shared: &Shared,
    id: &str,
    request: Request<Incoming>,
) -> Response<Body> {
    let body: EditFavoriteBody = match read_json(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let current = match shared.control.favorites_in_order().await {
        Ok(all) => all.into_iter().find(|f| f.id() == id),
        Err(e) => return control_error(e),
    };
    let Some(current) = current else {
        return control_error(crate::control::Error::NoFavorite(id.to_string()));
    };
    let s = &current.station;
    let trimmed = |v: Option<String>| v.map(|v| v.trim().to_string());
    let edit = FavoriteEdit {
        name: trimmed(body.name).unwrap_or_else(|| s.name.clone()),
        url: trimmed(body.url).unwrap_or_else(|| s.url.clone()),
        logo_url: trimmed(body.logo_url).or_else(|| s.logo_url.clone()),
        country: trimmed(body.country).or_else(|| s.country.clone()),
    };
    let checked = if edit.name.is_empty() {
        Err("name must not be empty".to_string())
    } else {
        check_stream(&edit.url)
            .and_then(|_| check_length("name", &edit.name, MAX_NAME_CHARS))
            .and_then(|_| {
                check_length(
                    "logo_url",
                    edit.logo_url.as_deref().unwrap_or(""),
                    MAX_URL_CHARS,
                )
            })
            .and_then(|_| {
                check_length(
                    "country",
                    edit.country.as_deref().unwrap_or(""),
                    MAX_NAME_CHARS,
                )
            })
    };
    if let Err(text) = checked {
        return error(StatusCode::BAD_REQUEST, "bad_request", &text);
    }
    match shared.control.edit_favorite(id.to_string(), edit).await {
        Ok((old, new)) => {
            let stats = shared.control.favorite_stats(new.id()).await;
            let item = FavoriteItem::new(&new, stats);
            if let Some(edited) = &shared.window.favorite_edited {
                edited(old, new);
            }
            json(StatusCode::OK, &item)
        }
        Err(e) => control_error(e),
    }
}

/// `DELETE /v1/favorites/{id}`
pub(super) async fn remove_favorite(shared: &Shared, id: &str) -> Response<Body> {
    match shared.control.remove_favorite(id.to_string()).await {
        Ok(_) => no_content(),
        Err(e) => control_error(e),
    }
}

#[derive(Deserialize)]
struct OrderBody {
    /// Favorite ids in their new order; any left out follow them
    ids: Vec<String>,
}

/// `PUT /v1/favorites/order`
pub(super) async fn reorder(shared: &Shared, request: Request<Incoming>) -> Response<Body> {
    let body: OrderBody = match read_json(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    match shared.control.reorder_favorites(body.ids).await {
        Ok(()) => no_content(),
        Err(e) => control_error(e),
    }
}

/// A station in a list sent to the player: what `GET /v1/favorites`
/// gives, without the plays and listening time
#[derive(Deserialize)]
struct ImportStation {
    name: String,
    url: String,
    #[serde(default)]
    logo_url: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    genres: Vec<String>,
    #[serde(default)]
    codec: Option<String>,
    #[serde(default)]
    bitrate_kbps: Option<u32>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    provider_id: Option<String>,
}

impl ImportStation {
    /// The favorite to save, or what is wrong with it
    fn into_favorite(self) -> Result<Favorite, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("name must not be empty".into());
        }
        check_stream(&self.url)?;
        check_length("name", name, MAX_NAME_CHARS)?;
        let text = |v: Option<String>| v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let (logo_url, homepage) = (text(self.logo_url), text(self.homepage));
        check_length("logo_url", logo_url.as_deref().unwrap_or(""), MAX_URL_CHARS)?;
        check_length("homepage", homepage.as_deref().unwrap_or(""), MAX_URL_CHARS)?;
        let short = [
            ("country", text(self.country)),
            ("language", text(self.language)),
            ("codec", text(self.codec)),
            ("provider", text(self.provider)),
            ("provider_id", text(self.provider_id)),
        ];
        for (field, value) in &short {
            check_length(field, value.as_deref().unwrap_or(""), MAX_NAME_CHARS)?;
        }
        if self.genres.len() > MAX_GENRES {
            return Err(format!("at most {MAX_GENRES} genres"));
        }
        for genre in &self.genres {
            check_length("genre", genre, MAX_NAME_CHARS)?;
        }
        let [(_, country), (_, language), (_, codec), (_, provider), (_, provider_id)] = short;
        let mut station = Station::new(name, self.url.trim());
        station.logo_url = logo_url;
        station.homepage = homepage;
        station.country = country;
        station.language = language;
        station.codec = codec;
        station.bitrate = self.bitrate_kbps.filter(|b| *b > 0);
        station.genres = self
            .genres
            .into_iter()
            .map(|g| g.trim().to_string())
            .filter(|g| !g.is_empty())
            .collect();
        if let Some(provider) = provider {
            station.provider = provider;
        }
        station.provider_id = provider_id;
        Ok(Favorite::from_station(station))
    }
}

#[derive(Deserialize)]
struct ImportBody {
    /// "add" or "replace"
    mode: String,
    /// Only tell what would change
    #[serde(default)]
    preview: bool,
    /// In the order they should have
    favorites: Vec<ImportStation>,
}

#[derive(Serialize)]
struct ImportAnswer {
    #[serde(flatten)]
    outcome: radiotrope_app::data::favorites::ImportOutcome,
    /// `POST /v1/favorites/import/undo` has a list to put back
    can_undo: bool,
    /// `favorites_rev` after the import
    rev: u64,
}

/// `POST /v1/favorites/import`: add a list of favorites, or make it the
/// list; with `"preview": true` only answers what would change
pub(super) async fn import_favorites(
    shared: &Shared,
    request: Request<Incoming>,
) -> Response<Body> {
    let body: ImportBody = match read_json_up_to(request, MAX_IMPORT_BODY).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let mode = match body.mode.as_str() {
        "add" => ImportMode::Add,
        "replace" => ImportMode::Replace,
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "mode must be add or replace",
            )
        }
    };
    if body.favorites.len() > MAX_FAVORITES {
        return error(
            StatusCode::BAD_REQUEST,
            "too_many_favorites",
            &format!("At most {MAX_FAVORITES} favorites can be sent"),
        );
    }
    let mut list = Vec::with_capacity(body.favorites.len());
    for (i, station) in body.favorites.into_iter().enumerate() {
        match station.into_favorite() {
            Ok(fav) => list.push(fav),
            Err(text) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    &format!("favorites[{i}]: {text}"),
                )
            }
        }
    }
    match shared
        .control
        .import_favorites(mode, list, body.preview)
        .await
    {
        Ok((outcome, can_undo)) => json(
            StatusCode::OK,
            &ImportAnswer {
                outcome,
                can_undo,
                rev: shared.control.favorites_generation().await,
            },
        ),
        Err(e) => control_error(e),
    }
}

#[derive(Serialize)]
struct UndoAnswer {
    /// Favorites there are now
    count: usize,
    rev: u64,
}

/// `POST /v1/favorites/import/undo`: put back the favorites from before
/// the last import
pub(super) async fn undo_import(shared: &Shared) -> Response<Body> {
    match shared.control.undo_favorites_import().await {
        Ok(Some(count)) => json(
            StatusCode::OK,
            &UndoAnswer {
                count,
                rev: shared.control.favorites_generation().await,
            },
        ),
        Ok(None) => error(
            StatusCode::CONFLICT,
            "nothing_to_undo",
            "There is no import to undo",
        ),
        Err(e) => control_error(e),
    }
}

/// A stream URL the player can play and keep
fn check_stream(url: &str) -> Result<(), String> {
    check_length("url", url, MAX_URL_CHARS)?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("url must start with http:// or https://".into());
    }
    Ok(())
}

fn given(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// The request's query string as names and values, decoded (`+` is a
/// space, `%xx` a byte); a name given twice keeps its last value
pub(super) fn query_params(request: &Request<Incoming>) -> HashMap<String, String> {
    parse_query(request.uri().query().unwrap_or(""))
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(name), decode(value))
        })
        .collect()
}

fn decode(text: &str) -> String {
    fn hex(byte: u8) -> Option<u8> {
        (byte as char).to_digit(16).map(|d| d as u8)
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => match (
                bytes.get(i + 1).and_then(|b| hex(*b)),
                bytes.get(i + 2).and_then(|b| hex(*b)),
            ) {
                (Some(high), Some(low)) => {
                    out.push(high * 16 + low);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_string_is_decoded() {
        let q =
            parse_query("q=jazz+fm&country=%CE%95%CE%BB%CE%BB%CE%AC%CE%B4%CE%B1&x&bad=%zz&end=%4");
        assert_eq!(q["q"], "jazz fm");
        assert_eq!(q["country"], "Ελλάδα");
        assert_eq!(q["x"], "");
        assert_eq!(q["bad"], "%zz");
        assert_eq!(q["end"], "%4");
    }
}
