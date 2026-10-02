//! Shared provider types
//!
//! Types used across all station providers.

use crate::data::flags::code_for_country;
use crate::data::types::Station;

/// Results from a station search or browse operation
#[derive(Debug, Clone)]
pub struct SearchResults {
    /// Matching stations
    pub stations: Vec<Station>,
    /// Total number of results (if the provider reports it)
    pub total: Option<usize>,
    /// Whether more results are available beyond this page
    pub has_more: bool,
}

impl SearchResults {
    /// Create an empty result set
    pub fn empty() -> Self {
        Self {
            stations: Vec::new(),
            total: Some(0),
            has_more: false,
        }
    }
}

/// What to look for in [`search_filtered`]: every field set must match.
/// All empty lists the most popular stations.
///
/// [`search_filtered`]: super::StationProvider::search_filtered
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StationFilter {
    /// Words in the station name
    pub name: Option<String>,
    /// Genre tag, e.g. "jazz"
    pub genre: Option<String>,
    /// Country name ("Greece") or ISO 3166-1 code ("GR")
    pub country: Option<String>,
    /// Language, e.g. "greek"
    pub language: Option<String>,
    /// Codec, e.g. "MP3", "AAC"
    pub codec: Option<String>,
    /// Lowest bitrate in kbps
    pub min_bitrate: Option<u32>,
    pub order: SearchOrder,
}

/// How [`StationFilter`] results are ordered
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchOrder {
    /// Most played first
    #[default]
    Popular,
    /// Most voted first
    Votes,
    /// Rising fastest first
    Trending,
    /// Highest bitrate first
    Bitrate,
    /// A to Z
    Name,
}

impl StationFilter {
    /// The country as an ISO code, when it was given as one
    pub fn country_code(&self) -> Option<String> {
        self.country
            .as_deref()
            .map(str::trim)
            .filter(|c| c.len() == 2 && c.chars().all(|ch| ch.is_ascii_alphabetic()))
            .map(|c| c.to_ascii_uppercase())
    }

    /// Whether a station passes the filter (for providers that can't filter
    /// on their side)
    pub fn matches(&self, station: &Station) -> bool {
        fn contains(haystack: &str, needle: &str) -> bool {
            haystack
                .to_lowercase()
                .contains(&needle.trim().to_lowercase())
        }
        let name_ok = self
            .name
            .as_deref()
            .is_none_or(|n| contains(&station.name, n));
        let genre_ok = self
            .genre
            .as_deref()
            .is_none_or(|g| station.genres.iter().any(|sg| contains(sg, g)));
        // Stations carry a country name ("Greece, Attica"): an ISO code
        // given is compared with the code that name stands for
        let country_ok = match (self.country_code(), self.country.as_deref()) {
            (Some(code), _) => station.country.as_deref().is_some_and(|c| {
                c.trim().eq_ignore_ascii_case(&code)
                    || code_for_country(c).is_some_and(|cc| cc.eq_ignore_ascii_case(&code))
            }),
            (None, Some(name)) => station
                .country
                .as_deref()
                .is_some_and(|c| contains(c, name)),
            (None, None) => true,
        };
        let language_ok = self.language.as_deref().is_none_or(|l| {
            station
                .language
                .as_deref()
                .is_some_and(|sl| contains(sl, l))
        });
        let codec_ok = self.codec.as_deref().is_none_or(|c| {
            station
                .codec
                .as_deref()
                .is_some_and(|sc| sc.eq_ignore_ascii_case(c.trim()))
        });
        let bitrate_ok = self
            .min_bitrate
            .is_none_or(|min| station.bitrate.is_some_and(|b| b >= min));
        name_ok && genre_ok && country_ok && language_ok && codec_ok && bitrate_ok
    }
}

/// A browsable category (genre, country, language)
#[derive(Debug, Clone)]
pub struct Category {
    /// Machine-readable identifier
    pub id: String,
    /// Display name
    pub name: String,
    /// What kind of category this is
    pub category_type: CategoryType,
    /// Number of stations in this category (if known)
    pub station_count: Option<usize>,
    /// ISO 3166-1 alpha-2 code, for country categories (if known)
    pub code: Option<String>,
}

impl Category {
    /// Create a new category
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        category_type: CategoryType,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            category_type,
            station_count: None,
            code: None,
        }
    }

    /// Set the station count
    pub fn with_station_count(mut self, count: usize) -> Self {
        self.station_count = Some(count);
        self
    }

    /// Set the ISO country code
    pub fn with_code(mut self, code: Option<String>) -> Self {
        self.code = code;
        self
    }
}

/// The type of a browsable category
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CategoryType {
    Genre,
    Country,
    Language,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filter_matches_every_field_it_sets() {
        let mut station = Station::new("Jazz FM", "http://jazz.test");
        // As radio-browser names it
        station.country =
            Some("The United Kingdom Of Great Britain And Northern Ireland, London".into());
        station.language = Some("english".into());
        station.genres = ["smooth jazz".to_string()].into();
        station.codec = Some("MP3".into());
        station.bitrate = Some(128);

        assert!(StationFilter::default().matches(&station));
        let filter = StationFilter {
            name: Some("jazz".into()),
            genre: Some("Jazz".into()),
            country: Some("gb".into()),
            language: Some("English".into()),
            codec: Some("mp3".into()),
            min_bitrate: Some(128),
            order: SearchOrder::Popular,
        };
        assert!(filter.matches(&station));
        let by_name = StationFilter {
            country: Some("united kingdom".into()),
            ..Default::default()
        };
        assert!(by_name.matches(&station));
        for miss in [
            StationFilter {
                name: Some("news".into()),
                ..Default::default()
            },
            StationFilter {
                country: Some("GR".into()),
                ..Default::default()
            },
            StationFilter {
                codec: Some("AAC".into()),
                ..Default::default()
            },
            StationFilter {
                min_bitrate: Some(192),
                ..Default::default()
            },
        ] {
            assert!(!miss.matches(&station), "{miss:?}");
        }
        assert_eq!(filter.country_code().as_deref(), Some("GB"));
    }

    #[test]
    fn test_search_results_empty() {
        let results = SearchResults::empty();
        assert!(results.stations.is_empty());
        assert_eq!(results.total, Some(0));
        assert!(!results.has_more);
    }

    #[test]
    fn test_search_results_with_data() {
        let results = SearchResults {
            stations: vec![
                Station::new("Radio 1", "http://r1.com"),
                Station::new("Radio 2", "http://r2.com"),
            ],
            total: Some(50),
            has_more: true,
        };
        assert_eq!(results.stations.len(), 2);
        assert_eq!(results.total, Some(50));
        assert!(results.has_more);
    }

    #[test]
    fn test_search_results_unknown_total() {
        let results = SearchResults {
            stations: vec![Station::new("Radio", "http://r.com")],
            total: None,
            has_more: false,
        };
        assert_eq!(results.total, None);
    }

    #[test]
    fn test_category_creation() {
        let cat = Category::new("rock", "Rock", CategoryType::Genre);
        assert_eq!(cat.id, "rock");
        assert_eq!(cat.name, "Rock");
        assert_eq!(cat.category_type, CategoryType::Genre);
        assert_eq!(cat.station_count, None);
    }

    #[test]
    fn test_category_with_station_count() {
        let cat =
            Category::new("us", "United States", CategoryType::Country).with_station_count(5000);
        assert_eq!(cat.station_count, Some(5000));
    }

    #[test]
    fn test_category_types() {
        assert_eq!(CategoryType::Genre, CategoryType::Genre);
        assert_ne!(CategoryType::Genre, CategoryType::Country);
        assert_ne!(CategoryType::Country, CategoryType::Language);
    }

    #[test]
    fn test_category_debug() {
        let cat = Category::new("jazz", "Jazz", CategoryType::Genre);
        let debug = format!("{:?}", cat);
        assert!(debug.contains("jazz"));
        assert!(debug.contains("Jazz"));
    }

    #[test]
    fn test_search_results_clone() {
        let results = SearchResults {
            stations: vec![Station::new("Radio", "http://r.com")],
            total: Some(1),
            has_more: false,
        };
        let cloned = results.clone();
        assert_eq!(cloned.stations.len(), 1);
        assert_eq!(cloned.total, Some(1));
    }

    #[test]
    fn test_category_clone() {
        let cat = Category::new("pop", "Pop", CategoryType::Genre).with_station_count(100);
        let cloned = cat.clone();
        assert_eq!(cloned.id, "pop");
        assert_eq!(cloned.station_count, Some(100));
    }
}
