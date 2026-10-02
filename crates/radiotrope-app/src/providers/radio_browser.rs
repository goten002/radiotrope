//! Radio Browser API provider
//!
//! Implementation of `StationProvider` for the Radio Browser directory
//! (<https://www.radio-browser.info/>).

use crate::config::providers::{CATEGORY_CACHE_TTL, RADIO_BROWSER_SERVERS, STATION_CACHE_TTL};
use crate::data::types::Station;
use crate::error::Result;
use crate::network::client::body_of;
use crate::network::{ApiCache, HttpClient};

use super::radio_browser_servers::Servers;

use super::traits::StationProvider;
use super::types::{Category, CategoryType, SearchOrder, SearchResults, StationFilter};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

// =============================================================================
// Internal API response types (serde)
// =============================================================================

#[derive(Debug, Deserialize)]
struct RbStation {
    stationuuid: String,
    name: String,
    #[serde(default)]
    url_resolved: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    favicon: String,
    #[serde(default)]
    tags: String,
    #[serde(default)]
    country: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    language: String,
    #[serde(default)]
    codec: String,
    #[serde(default)]
    bitrate: u32,
    #[serde(default)]
    homepage: String,
}

#[derive(Debug, Deserialize)]
struct RbTag {
    name: String,
    stationcount: usize,
}

#[derive(Debug, Deserialize)]
struct RbCountry {
    name: String,
    #[serde(default)]
    iso_3166_1: String,
    stationcount: usize,
}

#[derive(Debug, Deserialize)]
struct RbLanguage {
    name: String,
    stationcount: usize,
}

// =============================================================================
// RbStation -> Station conversion
// =============================================================================

/// Convert an empty string to None
fn non_empty(s: &str) -> Option<String> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

impl From<RbStation> for Station {
    fn from(rb: RbStation) -> Self {
        // Prefer url_resolved, fall back to url
        let stream_url = if rb.url_resolved.is_empty() {
            rb.url.clone()
        } else {
            rb.url_resolved.clone()
        };

        // Parse comma-separated tags into a set
        let genres: HashSet<String> = rb
            .tags
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();

        let bitrate = if rb.bitrate == 0 {
            None
        } else {
            Some(rb.bitrate)
        };

        // Combine country + state (e.g. "United States, California")
        let country = match (non_empty(&rb.country), non_empty(&rb.state)) {
            (Some(c), Some(s)) => Some(format!("{c}, {s}")),
            (Some(c), None) => Some(c),
            (None, Some(s)) => Some(s),
            (None, None) => None,
        };

        Station::new(rb.name, stream_url)
            .with_provider("radio-browser", Some(rb.stationuuid))
            .with_logo_opt(non_empty(&rb.favicon))
            .with_metadata(country, non_empty(&rb.language), genres)
            .with_audio_info(non_empty(&rb.codec), bitrate)
            .with_homepage_opt(non_empty(&rb.homepage))
    }
}

// =============================================================================
// RadioBrowserProvider
// =============================================================================

/// Radio Browser API provider
///
/// Searches the [Radio Browser](https://www.radio-browser.info/) directory,
/// which is a free, open-source community database of internet radio stations.
pub struct RadioBrowserProvider {
    client: HttpClient,
    /// Server the cached responses are filed under, whichever one answered
    base_url: String,
    /// The servers requests go to
    servers: Arc<Servers>,
}

impl RadioBrowserProvider {
    /// Create a provider using radio-browser's servers (see [`Servers`])
    ///
    /// Responses are cached on disk (see [`ApiCache`]) and the HTTP
    /// connections are shared with every other provider instance.
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: HttpClient::shared()?.with_cache(ApiCache::open_default()),
            base_url: RADIO_BROWSER_SERVERS[0].to_string(),
            servers: Servers::shared()?,
        })
    }

    /// Create a provider using only the server at `base_url` (for testing),
    /// without a response cache
    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self> {
        let base_url = base_url.into();
        let client = HttpClient::new()?;
        let servers = Servers::new(client.inner().clone(), vec![base_url.clone()], None, false);
        Ok(Self {
            client,
            base_url,
            servers: Arc::new(servers),
        })
    }

    /// Build a full API URL from an endpoint path
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// GET `path` from a server that answers, through the response cache
    fn get_cached<T: DeserializeOwned>(&self, path: &str, ttl: Duration) -> Result<T> {
        self.client
            .get_or_fetch(&format!("GET {}", self.url(path)), ttl, || {
                self.servers
                    .run(|base| body_of(self.client.inner().get(format!("{base}{path}"))))
            })
    }

    /// POST a form to `path` on a server that answers, through the response
    /// cache
    fn post_cached<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, &str)],
        ttl: Duration,
    ) -> Result<T> {
        let body = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let key = format!("POST {} {body}", self.url(path));
        self.client.get_or_fetch(&key, ttl, || {
            self.servers.run(|base| {
                body_of(
                    self.client
                        .inner()
                        .post(format!("{base}{path}"))
                        .form(params),
                )
            })
        })
    }

    /// Search stations via POST /json/stations/search
    fn search_stations(&self, params: &[(&str, &str)]) -> Result<SearchResults> {
        let rb_stations: Vec<RbStation> =
            self.post_cached("/json/stations/search", params, STATION_CACHE_TTL)?;

        let has_more = !rb_stations.is_empty();
        let stations: Vec<Station> = rb_stations.into_iter().map(Station::from).collect();

        Ok(SearchResults {
            total: None, // Radio Browser doesn't report total count
            has_more,
            stations,
        })
    }
}

/// Search parameter that selects a category's stations
///
/// Countries use the exact ISO code when known; the `country` name filter is
/// a substring match ("Congo" also matches "The Democratic Republic Of The Congo").
fn category_filter(category: &Category) -> (&'static str, &str) {
    match category.category_type {
        CategoryType::Genre => ("tag", &category.id),
        CategoryType::Country => match category.code.as_deref().filter(|c| !c.is_empty()) {
            Some(code) => ("countrycode", code),
            None => ("country", &category.id),
        },
        CategoryType::Language => ("language", &category.id),
    }
}

/// `/json/stations/search` parameters for a [`StationFilter`]
fn filtered_search_params(
    filter: &StationFilter,
    limit: usize,
    offset: usize,
) -> Vec<(&'static str, String)> {
    let mut params = Vec::new();
    let mut text = |key: &'static str, value: &Option<String>| {
        if let Some(v) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            params.push((key, v.to_string()));
        }
    };
    text("name", &filter.name);
    text("tag", &filter.genre);
    text("language", &filter.language);
    text("codec", &filter.codec);
    match filter.country_code() {
        Some(code) => params.push(("countrycode", code)),
        None => text("country", &filter.country),
    }
    if let Some(min) = filter.min_bitrate {
        params.push(("bitrateMin", min.to_string()));
    }
    let (order, reverse) = match filter.order {
        SearchOrder::Popular => ("clickcount", true),
        SearchOrder::Votes => ("votes", true),
        SearchOrder::Trending => ("clicktrend", true),
        SearchOrder::Bitrate => ("bitrate", true),
        SearchOrder::Name => ("name", false),
    };
    params.push(("order", order.to_string()));
    params.push(("reverse", reverse.to_string()));
    params.push(("hidebroken", "true".to_string()));
    params.push(("limit", limit.to_string()));
    params.push(("offset", offset.to_string()));
    params
}

impl StationProvider for RadioBrowserProvider {
    fn name(&self) -> &'static str {
        "Radio Browser"
    }

    fn id(&self) -> &'static str {
        "radio-browser"
    }

    fn search(&self, query: &str, limit: usize, offset: usize) -> Result<SearchResults> {
        let limit_str = limit.to_string();
        let offset_str = offset.to_string();
        self.search_stations(&[
            ("name", query),
            ("limit", &limit_str),
            ("offset", &offset_str),
            ("order", "clickcount"),
            ("reverse", "true"),
            ("hidebroken", "true"),
        ])
    }

    fn browse_categories(&self) -> Result<Vec<Category>> {
        let mut categories = Vec::new();

        // Genres (tags)
        let tags: Vec<RbTag> = self.get_cached(
            "/json/tags?limit=100&order=stationcount&reverse=true",
            CATEGORY_CACHE_TTL,
        )?;
        for tag in tags {
            if !tag.name.is_empty() {
                categories.push(
                    Category::new(&tag.name, &tag.name, CategoryType::Genre)
                        .with_station_count(tag.stationcount),
                );
            }
        }

        // Countries
        let countries: Vec<RbCountry> = self.get_cached(
            "/json/countries?order=stationcount&reverse=true&hidebroken=true",
            CATEGORY_CACHE_TTL,
        )?;
        for country in countries {
            if !country.name.is_empty() {
                categories.push(
                    Category::new(&country.name, &country.name, CategoryType::Country)
                        .with_station_count(country.stationcount)
                        .with_code(non_empty(&country.iso_3166_1)),
                );
            }
        }

        // Languages
        let languages: Vec<RbLanguage> = self.get_cached(
            "/json/languages?limit=100&order=stationcount&reverse=true",
            CATEGORY_CACHE_TTL,
        )?;
        for lang in languages {
            if !lang.name.is_empty() {
                categories.push(
                    Category::new(&lang.name, &lang.name, CategoryType::Language)
                        .with_station_count(lang.stationcount),
                );
            }
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
        let limit_str = limit.to_string();
        let offset_str = offset.to_string();
        let mut params = vec![
            category_filter(category),
            ("limit", &limit_str),
            ("offset", &offset_str),
            ("order", "clickcount"),
            ("reverse", "true"),
            ("hidebroken", "true"),
        ];
        let query = query.trim();
        if !query.is_empty() {
            params.push(("name", query));
        }
        self.search_stations(&params)
    }

    fn search_filtered(
        &self,
        filter: &StationFilter,
        limit: usize,
        offset: usize,
    ) -> Result<SearchResults> {
        let params = filtered_search_params(filter, limit, offset);
        let params: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let mut results = self.search_stations(&params)?;
        results.has_more = results.stations.len() >= limit;
        Ok(results)
    }

    fn get_popular(&self, limit: usize) -> Result<Vec<Station>> {
        let rb_stations: Vec<RbStation> = self.get_cached(
            &format!("/json/stations/topclick/{limit}"),
            STATION_CACHE_TTL,
        )?;
        Ok(rb_stations.into_iter().map(Station::from).collect())
    }

    fn get_station(&self, id: &str) -> Result<Option<Station>> {
        // The id goes into the URL path: anything but a station's UUID
        // could reach another endpoint, and would leave a cache entry
        if !is_station_uuid(id) {
            return Ok(None);
        }
        let rb_stations: Vec<RbStation> =
            self.get_cached(&format!("/json/stations/byuuid/{id}"), STATION_CACHE_TTL)?;
        Ok(rb_stations.into_iter().next().map(Station::from))
    }

    fn report_click(&self, station: &Station) -> Result<()> {
        if let Some(ref provider_id) = station.provider_id {
            let path = format!("/json/url/{provider_id}");
            // Fire and forget — ignore the response body
            self.servers
                .run(|base| body_of(self.client.inner().get(format!("{base}{path}"))))?;
        }
        Ok(())
    }
}

/// Whether `id` has the shape of a radio-browser station id: a UUID, such
/// as `9617a958-0601-11e8-ae97-52543be04c81`
fn is_station_uuid(id: &str) -> bool {
    let groups: Vec<&str> = id.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtered_search_sends_every_filter() {
        let filter = StationFilter {
            name: Some(" kiss ".into()),
            genre: Some("pop".into()),
            country: Some("gr".into()),
            language: None,
            codec: Some("AAC".into()),
            min_bitrate: Some(96),
            order: SearchOrder::Name,
        };
        let params = filtered_search_params(&filter, 20, 40);
        let get = |k: &str| {
            params
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("name"), Some("kiss"));
        assert_eq!(get("tag"), Some("pop"));
        assert_eq!(get("countrycode"), Some("GR"));
        assert_eq!(get("country"), None);
        assert_eq!(get("language"), None);
        assert_eq!(get("codec"), Some("AAC"));
        assert_eq!(get("bitrateMin"), Some("96"));
        assert_eq!(get("order"), Some("name"));
        assert_eq!(get("reverse"), Some("false"));
        assert_eq!(get("limit"), Some("20"));
        assert_eq!(get("offset"), Some("40"));

        // A country name is a name filter; no filter at all is the most played
        let by_name = StationFilter {
            country: Some("Greece".into()),
            ..Default::default()
        };
        let params = filtered_search_params(&by_name, 5, 0);
        assert!(params.contains(&("country", "Greece".to_string())));
        assert!(params.contains(&("order", "clickcount".to_string())));
        assert!(params.contains(&("reverse", "true".to_string())));
    }

    #[test]
    fn only_a_uuid_is_looked_up_as_a_station_id() {
        assert!(is_station_uuid("9617a958-0601-11e8-ae97-52543be04c81"));
        assert!(is_station_uuid("9617A958-0601-11E8-AE97-52543BE04C81"));
        assert!(!is_station_uuid(""));
        assert!(!is_station_uuid(
            "x/../../url/9617a958-0601-11e8-ae97-52543be04c81"
        ));
        assert!(!is_station_uuid("9617a958-0601-11e8-ae97-52543be04c81/x"));
        assert!(!is_station_uuid("9617a958-0601-11e8-ae97-52543be04c8"));
        assert!(!is_station_uuid("9617a958-0601-11e8-ae97-52543be04c8g"));
        assert!(!is_station_uuid("9617a95806-01-11e8-ae97-52543be04c81"));
        // Refused before any request: an unreachable server is never asked
        let provider = RadioBrowserProvider::with_base_url("http://127.0.0.1:9").unwrap();
        assert!(provider.get_station("../stats").unwrap().is_none());
    }

    // ---- RbStation -> Station conversion tests ----

    fn sample_rb_station() -> RbStation {
        RbStation {
            stationuuid: "abc-123".to_string(),
            name: "Test Radio".to_string(),
            url_resolved: "http://stream.test.com/live".to_string(),
            url: "http://test.com/stream".to_string(),
            favicon: "http://test.com/logo.png".to_string(),
            tags: "rock,pop,indie".to_string(),
            country: "Germany".to_string(),
            state: String::new(),
            language: "german".to_string(),
            codec: "MP3".to_string(),
            bitrate: 128,
            homepage: "http://test.com".to_string(),
        }
    }

    #[test]
    fn test_rb_station_to_station_basic() {
        let station: Station = sample_rb_station().into();
        assert_eq!(station.name, "Test Radio");
        assert_eq!(station.provider, "radio-browser");
        assert_eq!(station.provider_id, Some("abc-123".to_string()));
    }

    #[test]
    fn test_rb_station_prefers_url_resolved() {
        let station: Station = sample_rb_station().into();
        assert_eq!(station.url, "http://stream.test.com/live");
    }

    #[test]
    fn test_rb_station_falls_back_to_url() {
        let mut rb = sample_rb_station();
        rb.url_resolved = String::new();
        let station: Station = rb.into();
        assert_eq!(station.url, "http://test.com/stream");
    }

    #[test]
    fn test_rb_station_logo() {
        let station: Station = sample_rb_station().into();
        assert_eq!(
            station.logo_url,
            Some("http://test.com/logo.png".to_string())
        );
    }

    #[test]
    fn test_rb_station_empty_logo() {
        let mut rb = sample_rb_station();
        rb.favicon = String::new();
        let station: Station = rb.into();
        assert_eq!(station.logo_url, None);
    }

    #[test]
    fn test_rb_station_tags_parsed() {
        let station: Station = sample_rb_station().into();
        assert_eq!(station.genres.len(), 3);
        assert!(station.genres.contains("rock"));
        assert!(station.genres.contains("pop"));
        assert!(station.genres.contains("indie"));
    }

    #[test]
    fn test_rb_station_empty_tags() {
        let mut rb = sample_rb_station();
        rb.tags = String::new();
        let station: Station = rb.into();
        assert!(station.genres.is_empty());
    }

    #[test]
    fn test_rb_station_tags_with_whitespace() {
        let mut rb = sample_rb_station();
        rb.tags = " rock , pop , , indie ".to_string();
        let station: Station = rb.into();
        assert_eq!(station.genres.len(), 3);
        assert!(station.genres.contains("rock"));
        assert!(station.genres.contains("pop"));
        assert!(station.genres.contains("indie"));
    }

    #[test]
    fn test_rb_station_country_and_language() {
        let station: Station = sample_rb_station().into();
        assert_eq!(station.country, Some("Germany".to_string()));
        assert_eq!(station.language, Some("german".to_string()));
    }

    #[test]
    fn test_rb_station_country_with_state() {
        let mut rb = sample_rb_station();
        rb.country = "United States".to_string();
        rb.state = "California".to_string();
        let station: Station = rb.into();
        assert_eq!(
            station.country,
            Some("United States, California".to_string())
        );
    }

    #[test]
    fn test_rb_station_state_only() {
        let mut rb = sample_rb_station();
        rb.country = String::new();
        rb.state = "Bavaria".to_string();
        let station: Station = rb.into();
        assert_eq!(station.country, Some("Bavaria".to_string()));
    }

    #[test]
    fn test_rb_station_empty_country() {
        let mut rb = sample_rb_station();
        rb.country = String::new();
        let station: Station = rb.into();
        assert_eq!(station.country, None);
    }

    #[test]
    fn test_rb_station_empty_language() {
        let mut rb = sample_rb_station();
        rb.language = String::new();
        let station: Station = rb.into();
        assert_eq!(station.language, None);
    }

    #[test]
    fn test_rb_station_codec() {
        let station: Station = sample_rb_station().into();
        assert_eq!(station.codec, Some("MP3".to_string()));
    }

    #[test]
    fn test_rb_station_empty_codec() {
        let mut rb = sample_rb_station();
        rb.codec = String::new();
        let station: Station = rb.into();
        assert_eq!(station.codec, None);
    }

    #[test]
    fn test_rb_station_bitrate() {
        let station: Station = sample_rb_station().into();
        assert_eq!(station.bitrate, Some(128));
    }

    #[test]
    fn test_rb_station_zero_bitrate() {
        let mut rb = sample_rb_station();
        rb.bitrate = 0;
        let station: Station = rb.into();
        assert_eq!(station.bitrate, None);
    }

    #[test]
    fn test_rb_station_homepage() {
        let station: Station = sample_rb_station().into();
        assert_eq!(station.homepage, Some("http://test.com".to_string()));
    }

    #[test]
    fn test_rb_station_empty_homepage() {
        let mut rb = sample_rb_station();
        rb.homepage = String::new();
        let station: Station = rb.into();
        assert_eq!(station.homepage, None);
    }

    #[test]
    fn test_rb_station_all_empty_strings() {
        let rb = RbStation {
            stationuuid: "id-1".to_string(),
            name: "Minimal".to_string(),
            url_resolved: String::new(),
            url: "http://min.com/stream".to_string(),
            favicon: String::new(),
            tags: String::new(),
            country: String::new(),
            state: String::new(),
            language: String::new(),
            codec: String::new(),
            bitrate: 0,
            homepage: String::new(),
        };
        let station: Station = rb.into();
        assert_eq!(station.name, "Minimal");
        assert_eq!(station.url, "http://min.com/stream");
        assert_eq!(station.logo_url, None);
        assert!(station.genres.is_empty());
        assert_eq!(station.country, None);
        assert_eq!(station.language, None);
        assert_eq!(station.codec, None);
        assert_eq!(station.bitrate, None);
        assert_eq!(station.homepage, None);
        assert_eq!(station.provider, "radio-browser");
        assert_eq!(station.provider_id, Some("id-1".to_string()));
    }

    #[test]
    fn test_rb_station_whitespace_only_fields() {
        let rb = RbStation {
            stationuuid: "id-2".to_string(),
            name: "Whitespace".to_string(),
            url_resolved: "http://ws.com/stream".to_string(),
            url: String::new(),
            favicon: "  ".to_string(),
            tags: " , , ".to_string(),
            country: "  ".to_string(),
            state: "  ".to_string(),
            language: "  ".to_string(),
            codec: "  ".to_string(),
            bitrate: 0,
            homepage: "  ".to_string(),
        };
        let station: Station = rb.into();
        assert_eq!(station.logo_url, None);
        assert!(station.genres.is_empty());
        assert_eq!(station.country, None);
        assert_eq!(station.language, None);
        assert_eq!(station.codec, None);
        assert_eq!(station.homepage, None);
    }

    // ---- non_empty helper ----

    #[test]
    fn test_non_empty_with_content() {
        assert_eq!(non_empty("hello"), Some("hello".to_string()));
    }

    #[test]
    fn test_non_empty_with_empty() {
        assert_eq!(non_empty(""), None);
    }

    #[test]
    fn test_non_empty_with_whitespace() {
        assert_eq!(non_empty("  "), None);
    }

    // ---- Provider construction ----

    #[test]
    fn test_provider_creation() {
        let provider = RadioBrowserProvider::new();
        assert!(provider.is_ok());
    }

    #[test]
    fn test_provider_with_custom_base_url() {
        let provider = RadioBrowserProvider::with_base_url("http://localhost:8080").unwrap();
        assert_eq!(provider.base_url, "http://localhost:8080");
    }

    #[test]
    fn test_provider_id() {
        let provider = RadioBrowserProvider::new().unwrap();
        assert_eq!(provider.id(), "radio-browser");
    }

    #[test]
    fn test_provider_name() {
        let provider = RadioBrowserProvider::new().unwrap();
        assert_eq!(provider.name(), "Radio Browser");
    }

    #[test]
    fn test_provider_icon_none() {
        let provider = RadioBrowserProvider::new().unwrap();
        assert!(provider.icon().is_none());
    }

    #[test]
    fn test_provider_url_building() {
        let provider = RadioBrowserProvider::with_base_url("https://api.example.com").unwrap();
        assert_eq!(
            provider.url("/json/tags"),
            "https://api.example.com/json/tags"
        );
    }

    // ---- RbStation JSON deserialization ----

    #[test]
    fn test_rb_station_deserialize_full() {
        let json = r#"{
            "stationuuid": "uuid-1",
            "name": "JSON Radio",
            "url_resolved": "http://resolved.com/stream",
            "url": "http://original.com/stream",
            "favicon": "http://img.com/logo.png",
            "tags": "jazz,blues",
            "country": "France",
            "language": "french",
            "codec": "AAC",
            "bitrate": 256,
            "homepage": "http://jsonradio.com"
        }"#;
        let rb: RbStation = serde_json::from_str(json).unwrap();
        assert_eq!(rb.stationuuid, "uuid-1");
        assert_eq!(rb.name, "JSON Radio");
        assert_eq!(rb.bitrate, 256);

        let station: Station = rb.into();
        assert_eq!(station.url, "http://resolved.com/stream");
        assert_eq!(station.codec, Some("AAC".to_string()));
        assert!(station.genres.contains("jazz"));
        assert!(station.genres.contains("blues"));
    }

    #[test]
    fn test_rb_station_deserialize_missing_optional_fields() {
        // Only required fields: stationuuid and name
        let json = r#"{
            "stationuuid": "uuid-2",
            "name": "Minimal JSON Radio"
        }"#;
        let rb: RbStation = serde_json::from_str(json).unwrap();
        assert_eq!(rb.name, "Minimal JSON Radio");
        assert_eq!(rb.url_resolved, "");
        assert_eq!(rb.url, "");
        assert_eq!(rb.favicon, "");
        assert_eq!(rb.tags, "");
        assert_eq!(rb.bitrate, 0);

        let station: Station = rb.into();
        assert_eq!(station.logo_url, None);
        assert_eq!(station.bitrate, None);
        assert!(station.genres.is_empty());
    }

    #[test]
    fn test_rb_station_deserialize_extra_fields_ignored() {
        let json = r#"{
            "stationuuid": "uuid-3",
            "name": "Extra Fields Radio",
            "clickcount": 9999,
            "votes": 500,
            "lastchangetime_iso8601": "2025-01-01T00:00:00Z"
        }"#;
        let rb: RbStation = serde_json::from_str(json).unwrap();
        assert_eq!(rb.name, "Extra Fields Radio");
    }

    // ---- Tag edge cases ----

    #[test]
    fn test_rb_station_single_tag() {
        let mut rb = sample_rb_station();
        rb.tags = "electronic".to_string();
        let station: Station = rb.into();
        assert_eq!(station.genres.len(), 1);
        assert!(station.genres.contains("electronic"));
    }

    #[test]
    fn test_rb_station_duplicate_tags() {
        let mut rb = sample_rb_station();
        rb.tags = "rock,rock,pop".to_string();
        let station: Station = rb.into();
        assert_eq!(station.genres.len(), 2); // HashSet deduplicates
        assert!(station.genres.contains("rock"));
        assert!(station.genres.contains("pop"));
    }

    // ---- report_click edge case ----

    #[test]
    fn test_category_filter_country_prefers_code() {
        let cat =
            Category::new("Greece", "Greece", CategoryType::Country).with_code(Some("GR".into()));
        assert_eq!(category_filter(&cat), ("countrycode", "GR"));
    }

    #[test]
    fn test_category_filter_country_without_code() {
        let cat = Category::new("Greece", "Greece", CategoryType::Country);
        assert_eq!(category_filter(&cat), ("country", "Greece"));
        let cat = cat.with_code(Some(String::new()));
        assert_eq!(category_filter(&cat), ("country", "Greece"));
    }

    #[test]
    fn test_category_filter_genre_and_language() {
        let tag = Category::new("jazz", "jazz", CategoryType::Genre);
        assert_eq!(category_filter(&tag), ("tag", "jazz"));
        let lang = Category::new("greek", "greek", CategoryType::Language);
        assert_eq!(category_filter(&lang), ("language", "greek"));
    }

    #[test]
    fn test_report_click_no_provider_id() {
        let provider = RadioBrowserProvider::new().unwrap();
        let station = Station::new("No ID", "http://test.com");
        // Should succeed without making any HTTP request
        assert!(provider.report_click(&station).is_ok());
    }

    // ---- Integration tests (require network, marked #[ignore]) ----

    #[test]
    #[ignore]
    fn test_integration_search() {
        let provider = RadioBrowserProvider::new().unwrap();
        let results = provider.search("BBC", 5, 0).unwrap();
        assert!(!results.stations.is_empty());
        assert!(results.stations[0].provider == "radio-browser");
    }

    #[test]
    #[ignore]
    fn test_integration_get_popular() {
        let provider = RadioBrowserProvider::new().unwrap();
        let stations = provider.get_popular(5).unwrap();
        assert!(!stations.is_empty());
        assert!(stations.len() <= 5);
    }

    #[test]
    #[ignore]
    fn test_integration_browse_categories() {
        let provider = RadioBrowserProvider::new().unwrap();
        let categories = provider.browse_categories().unwrap();
        assert!(!categories.is_empty());
        // Should have genres, countries, and languages
        assert!(categories
            .iter()
            .any(|c| c.category_type == CategoryType::Genre));
        assert!(categories
            .iter()
            .any(|c| c.category_type == CategoryType::Country));
        assert!(categories
            .iter()
            .any(|c| c.category_type == CategoryType::Language));
    }

    #[test]
    #[ignore]
    fn test_integration_browse_category() {
        let provider = RadioBrowserProvider::new().unwrap();
        let category = Category::new("rock", "rock", CategoryType::Genre);
        let results = provider.browse_category(&category, 5, 0).unwrap();
        assert!(!results.stations.is_empty());
    }

    #[test]
    #[ignore]
    fn test_integration_get_station() {
        let provider = RadioBrowserProvider::new().unwrap();
        // First search for a station to get a valid UUID
        let results = provider.search("BBC Radio 1", 1, 0).unwrap();
        if let Some(station) = results.stations.first() {
            if let Some(ref id) = station.provider_id {
                let found = provider.get_station(id).unwrap();
                assert!(found.is_some());
                assert_eq!(found.unwrap().provider_id.as_deref(), Some(id.as_str()));
            }
        }
    }

    #[test]
    #[ignore]
    fn test_integration_get_station_not_found() {
        let provider = RadioBrowserProvider::new().unwrap();
        let result = provider
            .get_station("00000000-0000-0000-0000-000000000000")
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    #[ignore]
    fn test_integration_report_click() {
        let provider = RadioBrowserProvider::new().unwrap();
        let results = provider.search("BBC", 1, 0).unwrap();
        if let Some(station) = results.stations.first() {
            let result = provider.report_click(station);
            assert!(result.is_ok());
        }
    }
}
