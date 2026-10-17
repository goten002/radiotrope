//! Radiotrope station directory provider
//!
//! Implementation of `StationProvider` for Radiotrope's own directory at
//! <https://api.radiotrope.app/v1>. Its contract is `docs/api.md` in the
//! radiotrope-api repository.
//!
//! The directory pages with cursors; callers here page with offsets. The
//! provider remembers the cursor that continues each list at each offset it
//! has reached, and walks the list from the start for an offset it has not.

use crate::config::providers::{CATEGORY_CACHE_TTL, RADIOTROPE_API_URL, STATION_CACHE_TTL};
use crate::data::flags::{code_for_country, country_name};
use crate::data::languages::{language_code, language_name};
use crate::data::types::Station;
use crate::error::{AppError, Result};
use crate::network::client::{body_of, fetch_json};
use crate::network::{ApiCache, HttpClient};

use super::traits::StationProvider;
use super::types::{Category, CategoryType, SearchOrder, SearchResults, StationFilter};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// `Station.provider` of the directory's stations
pub const PROVIDER_ID: &str = "radiotrope";

/// Largest page the directory gives
const MAX_PAGE: usize = 200;

/// Remembered cursors kept before the oldest lists are forgotten
const MAX_CURSORS: usize = 2000;

// =============================================================================
// API response types (serde)
// =============================================================================

#[derive(Debug, Deserialize)]
struct ApiStation {
    id: String,
    name: String,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    languages: Vec<String>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    genres: Vec<ApiGenre>,
    #[serde(default)]
    logo: Option<String>,
    #[serde(default)]
    streams: Vec<ApiStream>,
}

#[derive(Debug, Deserialize)]
struct ApiStream {
    url: String,
    #[serde(default)]
    codec: Option<String>,
    #[serde(default)]
    bitrate_kbps: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ApiGenre {
    slug: String,
    name: String,
    #[serde(default)]
    stations: usize,
}

#[derive(Debug, Deserialize)]
struct ApiPage {
    stations: Vec<ApiStation>,
    #[serde(default)]
    total: Option<usize>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Facet<T> {
    items: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct CodeCount {
    code: String,
    stations: usize,
}

// =============================================================================
// ApiStation -> Station conversion
// =============================================================================

/// The codec as the player labels it; none for one the directory couldn't name
fn codec_label(codec: &str) -> Option<String> {
    match codec.to_ascii_lowercase().as_str() {
        "" | "other" => None,
        "vorbis" => Some("OGG".into()),
        other => Some(other.to_ascii_uppercase()),
    }
}

impl ApiStation {
    /// The station as the player keeps it, playing its first (default)
    /// stream. `None` for one without streams.
    fn into_station(self) -> Option<Station> {
        let stream = self.streams.into_iter().next()?;
        // "Greece, Attica", as the player shows a station's place
        let country = self
            .country
            .as_deref()
            .map(|code| country_name(code).unwrap_or_else(|| code.to_string()));
        let region = self.region.filter(|r| !r.trim().is_empty());
        let country = match (country, region) {
            (Some(c), Some(r)) => Some(format!("{c}, {r}")),
            (c, None) => c,
            (None, r) => r,
        };
        let languages: Vec<&str> = self
            .languages
            .iter()
            .map(|code| language_name(code).unwrap_or(code.as_str()))
            .collect();
        let language = Some(languages.join(", ")).filter(|l| !l.is_empty());
        let genres = self.genres.into_iter().map(|g| g.name).collect();
        Some(
            Station::new(self.name, stream.url)
                .with_provider(PROVIDER_ID, Some(self.id))
                .with_logo_opt(self.logo.filter(|l| !l.is_empty()))
                .with_metadata(country, language, genres)
                .with_audio_info(
                    stream.codec.as_deref().and_then(codec_label),
                    stream.bitrate_kbps.filter(|b| *b > 0),
                )
                .with_homepage_opt(self.homepage.filter(|h| !h.is_empty())),
        )
    }
}

fn stations_of(list: Vec<ApiStation>) -> Vec<Station> {
    list.into_iter()
        .filter_map(ApiStation::into_station)
        .collect()
}

// =============================================================================
// RadiotropeProvider
// =============================================================================

/// Query parameters of a station list, without its page size and cursor
type ListParams = Vec<(&'static str, String)>;

/// The Radiotrope station directory
pub struct RadiotropeProvider {
    client: HttpClient,
    base_url: String,
    /// (list, offset) -> the cursor that continues the list there
    cursors: Mutex<HashMap<(String, usize), String>>,
}

impl RadiotropeProvider {
    /// Create a provider for api.radiotrope.app
    ///
    /// Responses are cached on disk (see [`ApiCache`]) and the HTTP
    /// connections are shared with every other provider instance.
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: HttpClient::shared()?.with_cache(ApiCache::open_default()),
            base_url: RADIOTROPE_API_URL.to_string(),
            cursors: Mutex::new(HashMap::new()),
        })
    }

    /// Create a provider for the API at `base_url` (for testing), without a
    /// response cache
    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: HttpClient::new()?,
            base_url: base_url.into(),
            cursors: Mutex::new(HashMap::new()),
        })
    }

    /// Full URL of `path` with `params`, encoded
    fn url(&self, path: &str, params: &[(&str, String)]) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&format!("{}{path}", self.base_url))
            .map_err(|e| AppError::Config(format!("Bad directory URL: {e}")))?;
        // No bare "?" when there are none
        if !params.is_empty() {
            url.query_pairs_mut().extend_pairs(params);
        }
        Ok(url)
    }

    /// GET `url` through the response cache
    fn get_cached<T: DeserializeOwned>(&self, url: reqwest::Url, ttl: Duration) -> Result<T> {
        self.client.get_or_fetch(&format!("GET {url}"), ttl, || {
            fetch_json(self.client.inner().get(url.clone()))
        })
    }

    /// One page of a station list, `limit` long, from where `cursor` points
    fn fetch_page(
        &self,
        params: &ListParams,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ApiPage> {
        let mut all = params.clone();
        all.push(("limit", limit.clamp(1, MAX_PAGE).to_string()));
        if let Some(cursor) = cursor {
            all.push(("cursor", cursor.to_string()));
        }
        self.get_cached(self.url("/stations", &all)?, STATION_CACHE_TTL)
    }

    fn remember(&self, list: &str, offset: usize, cursor: &str) {
        let mut cursors = self.cursors.lock().unwrap_or_else(|e| e.into_inner());
        if cursors.len() >= MAX_CURSORS {
            cursors.clear();
        }
        cursors.insert((list.to_string(), offset), cursor.to_string());
    }

    /// The stations of a list from `offset` on, `limit` of them
    fn list(&self, params: ListParams, limit: usize, offset: usize) -> Result<SearchResults> {
        let list = format!("{params:?}");
        let mut cursor = None;
        if offset > 0 {
            let known = self
                .cursors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&(list.clone(), offset))
                .cloned();
            match known {
                Some(c) => cursor = Some(c),
                None => {
                    // Walk from the start, remembering the way
                    let mut at = 0;
                    while at < offset {
                        let page = self.fetch_page(
                            &params,
                            (offset - at).min(MAX_PAGE),
                            cursor.as_deref(),
                        )?;
                        at += page.stations.len();
                        match page.next_cursor {
                            Some(next) if !page.stations.is_empty() => {
                                self.remember(&list, at, &next);
                                cursor = Some(next);
                            }
                            // The list ends before `offset`
                            _ => return Ok(SearchResults::empty()),
                        }
                    }
                }
            }
        }
        let page = self.fetch_page(&params, limit, cursor.as_deref())?;
        if let Some(next) = &page.next_cursor {
            self.remember(&list, offset + page.stations.len(), next);
        }
        Ok(SearchResults {
            total: page.total,
            has_more: page.next_cursor.is_some() && !page.stations.is_empty(),
            stations: stations_of(page.stations),
        })
    }

    /// The directory's genres, most stations first
    fn genres(&self) -> Result<Vec<ApiGenre>> {
        let facet: Facet<ApiGenre> =
            self.get_cached(self.url("/genres", &[])?, CATEGORY_CACHE_TTL)?;
        Ok(facet.items)
    }

    /// The slug of a genre given by name ("Drum and Bass") or slug
    fn genre_slug(&self, genre: &str) -> String {
        let genre = genre.trim();
        if let Ok(genres) = self.genres() {
            if let Some(g) = genres
                .iter()
                .find(|g| g.slug.eq_ignore_ascii_case(genre) || g.name.eq_ignore_ascii_case(genre))
            {
                return g.slug.clone();
            }
        }
        slug(genre)
    }

    /// One station by its directory ID, or by the Radio Browser UUID it was
    /// built from; `None` if the directory doesn't have it
    fn station_at(&self, path: &str) -> Result<Option<Station>> {
        let url = self.url(path, &[])?;
        let found: Option<ApiStation> =
            self.client
                .get_or_fetch(&format!("GET {url}"), STATION_CACHE_TTL, || {
                    let response = self.client.inner().get(url.clone()).send()?;
                    if response.status() == reqwest::StatusCode::NOT_FOUND {
                        return Ok((None, b"null".to_vec()));
                    }
                    let bytes = response.error_for_status()?.bytes()?.to_vec();
                    let station = serde_json::from_slice(&bytes)
                        .map_err(|e| AppError::InvalidResponse(e.to_string()))?;
                    Ok((Some(station), bytes))
                })?;
        Ok(found.and_then(ApiStation::into_station))
    }
}

/// "Drum and Bass" -> "drum-and-bass", the shape of the directory's slugs
fn slug(text: &str) -> String {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// The ISO 639-3 code of a language given by name or code
fn language_param(language: &str) -> Option<String> {
    let language = language.trim();
    language_code(language).map(str::to_string).or_else(|| {
        (language.len() == 3 && language.chars().all(|c| c.is_ascii_alphabetic()))
            .then(|| language.to_ascii_lowercase())
    })
}

/// The ISO 3166-1 code of a country given by name or code
fn country_param(country: &str) -> Option<String> {
    let country = country.trim();
    if country.len() == 2 && country.chars().all(|c| c.is_ascii_alphabetic()) {
        return Some(country.to_ascii_uppercase());
    }
    code_for_country(country).map(str::to_ascii_uppercase)
}

/// Search text the directory can match: it refuses text with no letters
/// or digits ("!!!")
fn search_text(query: &str) -> Option<String> {
    let query = query.trim();
    query
        .chars()
        .any(char::is_alphanumeric)
        .then(|| query.to_string())
}

/// Whether `id` has the shape of a directory station ID: `st_` and 13
/// characters of its alphabet
fn is_station_id(id: &str) -> bool {
    id.strip_prefix("st_").is_some_and(|rest| {
        rest.len() == 13
            && rest
                .bytes()
                .all(|b| b"0123456789abcdefghjkmnpqrstvwxyz".contains(&b))
    })
}

/// Whether `id` has the shape of a Radio Browser station UUID, such as
/// `9617a958-0601-11e8-ae97-52543be04c81` (favorites saved before the
/// player used this directory carry those)
fn is_uuid(id: &str) -> bool {
    let groups: Vec<&str> = id.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

impl StationProvider for RadiotropeProvider {
    fn name(&self) -> &'static str {
        "Radiotrope"
    }

    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn search(&self, query: &str, limit: usize, offset: usize) -> Result<SearchResults> {
        let mut params = ListParams::new();
        if !query.trim().is_empty() {
            match search_text(query) {
                Some(q) => params.push(("q", q)),
                None => return Ok(SearchResults::empty()),
            }
        }
        self.list(params, limit, offset)
    }

    fn browse_categories(&self) -> Result<Vec<Category>> {
        let mut categories = Vec::new();

        for genre in self.genres()? {
            categories.push(
                Category::new(&genre.slug, &genre.name, CategoryType::Genre)
                    .with_station_count(genre.stations),
            );
        }

        let countries: Facet<CodeCount> =
            self.get_cached(self.url("/countries", &[])?, CATEGORY_CACHE_TTL)?;
        for country in countries.items {
            let name = country_name(&country.code).unwrap_or_else(|| country.code.clone());
            categories.push(
                Category::new(&country.code, name, CategoryType::Country)
                    .with_station_count(country.stations)
                    .with_code(Some(country.code)),
            );
        }

        let languages: Facet<CodeCount> =
            self.get_cached(self.url("/languages", &[])?, CATEGORY_CACHE_TTL)?;
        for language in languages.items {
            let name = language_name(&language.code).unwrap_or(&language.code);
            categories.push(
                Category::new(&language.code, name, CategoryType::Language)
                    .with_station_count(language.stations),
            );
        }

        Ok(categories)
    }

    fn browse_category(
        &self,
        category: &Category,
        limit: usize,
        offset: usize,
    ) -> Result<SearchResults> {
        self.search_category(category, "", limit, offset)
    }

    fn search_category(
        &self,
        category: &Category,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<SearchResults> {
        let mut params = ListParams::new();
        let selected = match category.category_type {
            CategoryType::Genre => Some(("genre", category.id.clone())),
            CategoryType::Country => category
                .code
                .as_deref()
                .filter(|c| !c.is_empty())
                .or(Some(category.id.as_str()))
                .and_then(country_param)
                .map(|code| ("country", code)),
            CategoryType::Language => language_param(&category.id).map(|code| ("language", code)),
        };
        // A category the directory can't be asked for has no stations
        let Some(selected) = selected else {
            return Ok(SearchResults::empty());
        };
        params.push(selected);
        if !query.trim().is_empty() {
            match search_text(query) {
                Some(q) => params.push(("q", q)),
                None => return Ok(SearchResults::empty()),
            }
        }
        self.list(params, limit, offset)
    }

    fn search_filtered(
        &self,
        filter: &StationFilter,
        limit: usize,
        offset: usize,
    ) -> Result<SearchResults> {
        let text = |value: &Option<String>| {
            value
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let mut params = ListParams::new();
        if let Some(name) = text(&filter.name) {
            match search_text(&name) {
                Some(q) => params.push(("q", q)),
                None => return Ok(SearchResults::empty()),
            }
        }
        // A country or language the directory can't be asked for matches
        // no station
        if let Some(country) = text(&filter.country) {
            match country_param(&country) {
                Some(code) => params.push(("country", code)),
                None => return Ok(SearchResults::empty()),
            }
        }
        if let Some(language) = text(&filter.language) {
            match language_param(&language) {
                Some(code) => params.push(("language", code)),
                None => return Ok(SearchResults::empty()),
            }
        }
        if let Some(genre) = text(&filter.genre) {
            params.push(("genre", self.genre_slug(&genre)));
        }
        // Popular is the directory's default: most played, or the best
        // match first when a name is given
        if filter.order == SearchOrder::Name {
            params.push(("sort", "name".to_string()));
        }
        let mut results = self.list(params, limit, offset)?;
        // Codec and bitrate aren't directory filters: keep this page's
        // stations that pass them
        let audio = StationFilter {
            codec: filter.codec.clone(),
            min_bitrate: filter.min_bitrate,
            ..Default::default()
        };
        results.stations.retain(|s| audio.matches(s));
        Ok(results)
    }

    fn get_popular(&self, limit: usize) -> Result<Vec<Station>> {
        Ok(self.list(ListParams::new(), limit, 0)?.stations)
    }

    fn get_station(&self, id: &str) -> Result<Option<Station>> {
        // The id goes into the URL path: only an ID's shape is sent
        if is_station_id(id) {
            self.station_at(&format!("/stations/{id}"))
        } else if is_uuid(id) {
            self.station_at(&format!(
                "/stations/by-source/radio-browser/{}",
                id.to_ascii_lowercase()
            ))
        } else {
            Ok(None)
        }
    }

    fn report_click(&self, station: &Station) -> Result<()> {
        if station.provider != PROVIDER_ID {
            return Ok(());
        }
        if let Some(id) = station
            .provider_id
            .as_deref()
            .filter(|id| is_station_id(id))
        {
            let url = self.url(&format!("/stations/{id}/play"), &[])?;
            body_of(self.client.inner().post(url))?;
        }
        Ok(())
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    /// A directory on a local port: `answer` gets each request's path and
    /// query and returns (status, body). Returns the base URL and the
    /// requests seen.
    fn serve(
        answer: impl Fn(&str) -> (u16, String) + Send + Sync + 'static,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                // Skip the headers
                let mut header = String::new();
                while reader.read_line(&mut header).unwrap_or(0) > 2 {
                    header.clear();
                }
                let mut parts = line.split(' ');
                let method = parts.next().unwrap_or("");
                let target = parts.next().unwrap_or("").to_string();
                log.lock().unwrap().push(format!("{method} {target}"));
                let (status, body) = answer(&target);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (base, seen)
    }

    fn station_json(id: &str, name: &str) -> String {
        format!(
            r#"{{"id":"{id}","name":"{name}","country":"GR","region":"Attica","city":null,
               "languages":["ell","eng"],"homepage":"https://example.gr","geo":null,
               "category":null,"genres":[{{"slug":"pop","name":"Pop"}}],"status":"online",
               "logo":"https://assets.radiotrope.app/stations/{id}.webp?v=1",
               "streams":[{{"url":"https://stream.example.gr/{name}","codec":"aac",
               "bitrate_kbps":128,"hls":false,"status":"online"}},
               {{"url":"https://backup.example.gr/{name}","codec":"mp3",
               "bitrate_kbps":64,"hls":false,"status":"online"}}]}}"#
        )
    }

    /// A list of 5 stations, paged by a cursor that is the next index
    fn five_stations(target: &str) -> (u16, String) {
        let url = reqwest::Url::parse(&format!("http://x{target}")).unwrap();
        let get = |k: &str| {
            url.query_pairs()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.into_owned())
        };
        let limit: usize = get("limit").unwrap().parse().unwrap();
        let start: usize = get("cursor").map_or(0, |c| c.parse().unwrap());
        let end = (start + limit).min(5);
        let stations: Vec<String> = (start..end)
            .map(|i| station_json(&format!("st_000000000000{i}"), &format!("s{i}")))
            .collect();
        let next = if end < 5 {
            format!("\"{end}\"")
        } else {
            "null".into()
        };
        (
            200,
            format!(
                r#"{{"stations":[{}],"total":5,"next_cursor":{next},"snapshot":1}}"#,
                stations.join(",")
            ),
        )
    }

    #[test]
    fn a_station_reads_as_the_player_keeps_it() {
        let api: ApiStation =
            serde_json::from_str(&station_json("st_0000000000001", "Kiss")).unwrap();
        let s = api.into_station().unwrap();
        assert_eq!(s.name, "Kiss");
        assert_eq!(s.url, "https://stream.example.gr/Kiss");
        assert_eq!(s.provider, PROVIDER_ID);
        assert_eq!(s.provider_id.as_deref(), Some("st_0000000000001"));
        assert_eq!(s.country.as_deref(), Some("Greece, Attica"));
        assert_eq!(s.language.as_deref(), Some("Greek, English"));
        assert!(s.genres.contains("Pop"));
        assert_eq!(s.codec.as_deref(), Some("AAC"));
        assert_eq!(s.bitrate, Some(128));
        assert_eq!(s.homepage.as_deref(), Some("https://example.gr"));
        assert!(s.logo_url.unwrap().ends_with(".webp?v=1"));

        // Nulls everywhere they may be, and no streams: nothing to play
        let bare: ApiStation = serde_json::from_str(
            r#"{"id":"st_0000000000002","name":"Bare","country":null,"region":null,
                "languages":[],"homepage":null,"genres":[],"logo":null,
                "streams":[{"url":"http://bare.test","codec":"other","bitrate_kbps":null}]}"#,
        )
        .unwrap();
        let s = bare.into_station().unwrap();
        assert_eq!(
            (s.country, s.language, s.codec, s.bitrate),
            (None, None, None, None)
        );
        let silent: ApiStation =
            serde_json::from_str(r#"{"id":"st_0000000000003","name":"Silent","streams":[]}"#)
                .unwrap();
        assert!(silent.into_station().is_none());
    }

    #[test]
    fn offsets_page_through_cursors() {
        let (base, seen) = serve(five_stations);
        let provider = RadiotropeProvider::with_base_url(&base).unwrap();

        let first = provider.search("", 2, 0).unwrap();
        assert_eq!(first.stations.len(), 2);
        assert!(first.has_more);
        assert_eq!(first.total, Some(5));
        // The next page continues from the remembered cursor: one request
        let second = provider.search("", 2, 2).unwrap();
        assert_eq!(second.stations[0].name, "s2");
        let last = provider.search("", 2, 4).unwrap();
        assert_eq!(last.stations.len(), 1);
        assert!(!last.has_more);
        assert_eq!(seen.lock().unwrap().len(), 3);

        // An offset never reached walks there from the start
        let fresh = RadiotropeProvider::with_base_url(&base).unwrap();
        let page = fresh.search("", 1, 3).unwrap();
        assert_eq!(page.stations[0].name, "s3");
        // Past the end
        assert!(fresh.search("", 2, 9).unwrap().stations.is_empty());
    }

    #[test]
    fn filters_become_directory_parameters() {
        let (base, seen) = serve(|target| {
            if target.starts_with("/v1/genres") {
                (
                    200,
                    r#"{"items":[{"slug":"drum-and-bass","name":"Drum and Bass","stations":3}],"snapshot":1}"#
                        .into(),
                )
            } else {
                (
                    200,
                    r#"{"stations":[],"total":0,"next_cursor":null,"snapshot":1}"#.into(),
                )
            }
        });
        let provider = RadiotropeProvider::with_base_url(&base).unwrap();
        let filter = StationFilter {
            name: Some(" kiss ".into()),
            genre: Some("drum and bass".into()),
            country: Some("Greece".into()),
            language: Some("greek".into()),
            codec: Some("AAC".into()),
            min_bitrate: Some(96),
            order: SearchOrder::Name,
        };
        provider.search_filtered(&filter, 20, 0).unwrap();
        let seen = seen.lock().unwrap();
        let list = seen.last().unwrap();
        for part in [
            "q=kiss",
            "country=GR",
            "language=ell",
            "genre=drum-and-bass",
            "sort=name",
            "limit=20",
        ] {
            assert!(list.contains(part), "{part} not in {list}");
        }
        assert!(!list.contains("codec"), "{list}");
    }

    #[test]
    fn unknown_filters_and_bare_punctuation_find_nothing_without_asking() {
        let provider = RadiotropeProvider::with_base_url("http://127.0.0.1:9").unwrap();
        assert!(provider.search("!!!", 10, 0).unwrap().stations.is_empty());
        let nowhere = StationFilter {
            country: Some("Atlantis".into()),
            ..Default::default()
        };
        assert!(provider
            .search_filtered(&nowhere, 10, 0)
            .unwrap()
            .stations
            .is_empty());
    }

    #[test]
    fn codec_and_bitrate_are_kept_on_the_page() {
        let (base, _) = serve(|_| {
            (
                200,
                format!(
                    r#"{{"stations":[{}],"total":1,"next_cursor":null,"snapshot":1}}"#,
                    station_json("st_0000000000001", "a")
                ),
            )
        });
        let provider = RadiotropeProvider::with_base_url(&base).unwrap();
        let mp3 = StationFilter {
            codec: Some("mp3".into()),
            ..Default::default()
        };
        assert!(provider
            .search_filtered(&mp3, 5, 0)
            .unwrap()
            .stations
            .is_empty());
        let aac = StationFilter {
            codec: Some("aac".into()),
            min_bitrate: Some(128),
            ..Default::default()
        };
        assert_eq!(
            provider.search_filtered(&aac, 5, 0).unwrap().stations.len(),
            1
        );
    }

    #[test]
    fn stations_are_looked_up_by_id_or_radio_browser_uuid() {
        let (base, seen) = serve(|target| {
            if target == "/v1/stations/st_0000000000001"
                || target
                    == "/v1/stations/by-source/radio-browser/9617a958-0601-11e8-ae97-52543be04c81"
            {
                (200, station_json("st_0000000000001", "Found"))
            } else {
                (
                    404,
                    r#"{"error":{"code":"not_found","message":"No station"}}"#.into(),
                )
            }
        });
        let provider = RadiotropeProvider::with_base_url(&base).unwrap();
        let found = provider.get_station("st_0000000000001").unwrap().unwrap();
        assert_eq!(found.name, "Found");
        let moved = provider
            .get_station("9617A958-0601-11E8-AE97-52543BE04C81")
            .unwrap()
            .unwrap();
        assert_eq!(moved.provider_id.as_deref(), Some("st_0000000000001"));
        assert!(provider.get_station("st_000000000000z").unwrap().is_none());
        // Not an ID's shape: never sent
        let before = seen.lock().unwrap().len();
        assert!(provider.get_station("../countries").unwrap().is_none());
        assert!(provider
            .get_station("st_0000000000001/x")
            .unwrap()
            .is_none());
        assert_eq!(seen.lock().unwrap().len(), before);
    }

    #[test]
    fn categories_carry_names_and_codes() {
        let (base, _) = serve(|target| {
            let body = match target {
                "/v1/genres" => {
                    r#"{"items":[{"slug":"jazz","name":"Jazz","stations":4}],"snapshot":1}"#
                }
                "/v1/countries" => r#"{"items":[{"code":"GB","stations":9}],"snapshot":1}"#,
                "/v1/languages" => {
                    r#"{"items":[{"code":"eng","stations":8},{"code":"qqq","stations":1}],"snapshot":1}"#
                }
                _ => return (404, "{}".into()),
            };
            (200, body.into())
        });
        let provider = RadiotropeProvider::with_base_url(&base).unwrap();
        let all = provider.browse_categories().unwrap();
        let of = |t: CategoryType| all.iter().filter(move |c| c.category_type == t);
        let genre = of(CategoryType::Genre).next().unwrap();
        assert_eq!((genre.id.as_str(), genre.name.as_str()), ("jazz", "Jazz"));
        let country = of(CategoryType::Country).next().unwrap();
        assert_eq!(country.name, "United Kingdom");
        assert_eq!(country.code.as_deref(), Some("GB"));
        assert_eq!(country.station_count, Some(9));
        let names: Vec<&str> = of(CategoryType::Language)
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["English", "qqq"]);
    }

    #[test]
    fn plays_are_reported_for_directory_stations_only() {
        let (base, seen) = serve(|_| (204, String::new()));
        let provider = RadiotropeProvider::with_base_url(&base).unwrap();
        let ours = Station::new("A", "http://a.test")
            .with_provider(PROVIDER_ID, Some("st_0000000000001".into()));
        provider.report_click(&ours).unwrap();
        let manual = Station::new("B", "http://b.test");
        provider.report_click(&manual).unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            ["POST /v1/stations/st_0000000000001/play"]
        );
    }

    /// Against a running directory: `RADIOTROPE_API` (default the live
    /// one), run with `cargo test -p radiotrope-app -- --ignored directory`
    #[test]
    #[ignore]
    fn directory_answers_as_the_player_expects() {
        let base = std::env::var("RADIOTROPE_API").unwrap_or(RADIOTROPE_API_URL.into());
        let provider = RadiotropeProvider::with_base_url(base).unwrap();
        let top = provider.search("", 3, 0).unwrap();
        assert!(!top.stations.is_empty());
        let next = provider.search("", 3, 3).unwrap();
        if top.has_more {
            assert!(!next.stations.is_empty());
            assert_ne!(top.stations[0].provider_id, next.stations[0].provider_id);
        }
        let first = &top.stations[0];
        let id = first.provider_id.clone().unwrap();
        let again = provider.get_station(&id).unwrap().unwrap();
        assert_eq!(again.name, first.name);
        let typo: String = first.name.chars().take(4).collect();
        assert!(!provider.search(&typo, 5, 0).unwrap().stations.is_empty());
        let categories = provider.browse_categories().unwrap();
        let country = categories
            .iter()
            .find(|c| c.category_type == CategoryType::Country)
            .unwrap();
        let in_country = provider.browse_category(country, 5, 0).unwrap();
        assert!(!in_country.stations.is_empty());
        for kind in [CategoryType::Genre, CategoryType::Language] {
            if let Some(c) = categories.iter().find(|c| c.category_type == kind) {
                let filter = StationFilter {
                    genre: (kind == CategoryType::Genre).then(|| c.name.clone()),
                    language: (kind == CategoryType::Language).then(|| c.name.clone()),
                    ..Default::default()
                };
                assert!(
                    !provider
                        .search_filtered(&filter, 5, 0)
                        .unwrap()
                        .stations
                        .is_empty(),
                    "{c:?}"
                );
            }
        }
        provider.report_click(first).unwrap();
    }

    #[test]
    fn id_shapes() {
        assert!(is_station_id("st_8k3m9qx4ab72f"));
        assert!(!is_station_id("st_8k3m9qx4ab72"));
        assert!(!is_station_id("st_8k3m9qx4ab72i"));
        assert!(!is_station_id("8k3m9qx4ab72fab"));
        assert!(is_uuid("9617a958-0601-11e8-ae97-52543be04c81"));
        assert!(!is_uuid("9617a958-0601-11e8-ae97-52543be04c8"));
        assert_eq!(slug("Drum and Bass"), "drum-and-bass");
        assert_eq!(slug(" R&B "), "r-b");
    }
}
