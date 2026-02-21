//! Radio Browser API provider
//!
//! Implementation of `StationProvider` for the Radio Browser directory
//! (<https://www.radio-browser.info/>).

use crate::config::providers::RADIO_BROWSER_DEFAULT_SERVER;
use crate::data::types::Station;
use crate::error::Result;
use crate::network::HttpClient;

use super::traits::StationProvider;
use super::types::{Category, CategoryType, SearchResults};

use serde::Deserialize;
use std::collections::HashSet;

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
    base_url: String,
}

impl RadioBrowserProvider {
    /// Create a provider using the default server
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: HttpClient::new()?,
            base_url: RADIO_BROWSER_DEFAULT_SERVER.to_string(),
        })
    }

    /// Create a provider with a custom base URL (for testing or mirrors)
    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: HttpClient::new()?,
            base_url: base_url.into(),
        })
    }

    /// Build a full API URL from an endpoint path
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Search stations via POST /json/stations/search
    fn search_stations(&self, params: &[(&str, &str)]) -> Result<SearchResults> {
        let rb_stations: Vec<RbStation> = self
            .client
            .post_form_json(&self.url("/json/stations/search"), params)?;

        let has_more = !rb_stations.is_empty();
        let stations: Vec<Station> = rb_stations.into_iter().map(Station::from).collect();

        Ok(SearchResults {
            total: None, // Radio Browser doesn't report total count
            has_more,
            stations,
        })
    }
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
        let tags: Vec<RbTag> = self
            .client
            .get_json(&self.url("/json/tags?limit=100&order=stationcount&reverse=true"))?;
        for tag in tags {
            if !tag.name.is_empty() {
                categories.push(
                    Category::new(&tag.name, &tag.name, CategoryType::Genre)
                        .with_station_count(tag.stationcount),
                );
            }
        }

        // Countries
        let countries: Vec<RbCountry> = self
            .client
            .get_json(&self.url("/json/countries?limit=100&order=stationcount&reverse=true"))?;
        for country in countries {
            if !country.name.is_empty() {
                categories.push(
                    Category::new(&country.name, &country.name, CategoryType::Country)
                        .with_station_count(country.stationcount),
                );
            }
        }

        // Languages
        let languages: Vec<RbLanguage> = self
            .client
            .get_json(&self.url("/json/languages?limit=100&order=stationcount&reverse=true"))?;
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
        let limit_str = limit.to_string();
        let offset_str = offset.to_string();

        let filter_key = match category.category_type {
            CategoryType::Genre => "tag",
            CategoryType::Country => "country",
            CategoryType::Language => "language",
        };

        self.search_stations(&[
            (filter_key, &category.id),
            ("limit", &limit_str),
            ("offset", &offset_str),
            ("order", "clickcount"),
            ("reverse", "true"),
            ("hidebroken", "true"),
        ])
    }

    fn get_popular(&self, limit: usize) -> Result<Vec<Station>> {
        let url = self.url(&format!("/json/stations/topclick/{}", limit));
        let rb_stations: Vec<RbStation> = self.client.get_json(&url)?;
        Ok(rb_stations.into_iter().map(Station::from).collect())
    }

    fn get_station(&self, id: &str) -> Result<Option<Station>> {
        let url = self.url(&format!("/json/stations/byuuid/{}", id));
        let rb_stations: Vec<RbStation> = self.client.get_json(&url)?;
        Ok(rb_stations.into_iter().next().map(Station::from))
    }

    fn report_click(&self, station: &Station) -> Result<()> {
        if let Some(ref provider_id) = station.provider_id {
            let url = self.url(&format!("/json/url/{}", provider_id));
            // Fire and forget — ignore the response body
            let _: serde_json::Value = self.client.get_json(&url)?;
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
