//! Shared provider types
//!
//! Types used across all station providers.

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
        }
    }

    /// Set the station count
    pub fn with_station_count(mut self, count: usize) -> Self {
        self.station_count = Some(count);
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
