//! Station directory providers
//!
//! Providers for discovering radio stations (Radio Browser, SHOUTcast, etc.)

pub mod radio_browser;
pub mod traits;
pub mod types;

// Re-exports
pub use radio_browser::RadioBrowserProvider;
pub use traits::StationProvider;
pub use types::{Category, CategoryType, SearchResults};

use crate::data::types::Station;
use crate::error::Result;

/// Registry of available station providers
///
/// Manages multiple providers and supports searching across all of them.
pub struct ProviderRegistry {
    providers: Vec<Box<dyn StationProvider>>,
}

impl ProviderRegistry {
    /// Create an empty registry
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// Create a registry with the default providers
    pub fn with_defaults() -> Result<Self> {
        let mut registry = Self::new();
        registry.register(Box::new(RadioBrowserProvider::new()?));
        Ok(registry)
    }

    /// Register a provider
    pub fn register(&mut self, provider: Box<dyn StationProvider>) {
        self.providers.push(provider);
    }

    /// Get a provider by ID
    pub fn get(&self, id: &str) -> Option<&dyn StationProvider> {
        self.providers
            .iter()
            .find(|p| p.id() == id)
            .map(|p| p.as_ref())
    }

    /// List all provider IDs
    pub fn list_ids(&self) -> Vec<&'static str> {
        self.providers.iter().map(|p| p.id()).collect()
    }

    /// List all provider display names
    pub fn list_names(&self) -> Vec<&'static str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    /// Search across all providers, merging results
    pub fn search_all(&self, query: &str, limit: usize) -> Result<Vec<Station>> {
        let mut all_stations = Vec::new();
        for provider in &self.providers {
            match provider.search(query, limit, 0) {
                Ok(results) => all_stations.extend(results.stations),
                Err(_) => continue, // skip failing providers
            }
        }
        Ok(all_stations)
    }

    /// Number of registered providers
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Whether the registry has no providers
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::types::Station;
    use crate::error::Result;

    /// A mock provider for testing the registry
    struct MockProvider {
        stations: Vec<Station>,
    }

    impl MockProvider {
        fn new(stations: Vec<Station>) -> Self {
            Self { stations }
        }
    }

    impl StationProvider for MockProvider {
        fn name(&self) -> &'static str {
            "Mock Provider"
        }

        fn id(&self) -> &'static str {
            "mock"
        }

        fn search(&self, query: &str, limit: usize, offset: usize) -> Result<SearchResults> {
            let query_lower = query.to_lowercase();
            let matching: Vec<Station> = self
                .stations
                .iter()
                .filter(|s| s.name.to_lowercase().contains(&query_lower))
                .skip(offset)
                .take(limit)
                .cloned()
                .collect();
            let has_more = offset + matching.len() < self.stations.len();
            Ok(SearchResults {
                total: Some(matching.len()),
                has_more,
                stations: matching,
            })
        }

        fn browse_categories(&self) -> Result<Vec<Category>> {
            Ok(vec![Category::new("rock", "Rock", CategoryType::Genre)])
        }

        fn browse_category(
            &self,
            _category: &Category,
            limit: usize,
            offset: usize,
        ) -> Result<SearchResults> {
            let stations: Vec<Station> = self
                .stations
                .iter()
                .skip(offset)
                .take(limit)
                .cloned()
                .collect();
            Ok(SearchResults {
                total: Some(stations.len()),
                has_more: false,
                stations,
            })
        }

        fn get_popular(&self, limit: usize) -> Result<Vec<Station>> {
            Ok(self.stations.iter().take(limit).cloned().collect())
        }

        fn get_station(&self, id: &str) -> Result<Option<Station>> {
            Ok(self
                .stations
                .iter()
                .find(|s| s.provider_id.as_deref() == Some(id))
                .cloned())
        }
    }

    fn test_stations() -> Vec<Station> {
        vec![
            Station::new("Rock FM", "http://rock.fm/stream")
                .with_provider("mock", Some("1".to_string())),
            Station::new("Jazz Radio", "http://jazz.fm/stream")
                .with_provider("mock", Some("2".to_string())),
            Station::new("Pop Hits", "http://pop.fm/stream")
                .with_provider("mock", Some("3".to_string())),
        ]
    }

    #[test]
    fn test_registry_new_is_empty() {
        let registry = ProviderRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn test_registry_default_is_empty() {
        let registry = ProviderRegistry::default();
        assert!(registry.is_empty());
    }

    #[test]
    fn test_registry_register() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new(test_stations())));
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());
    }

    #[test]
    fn test_registry_get() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new(test_stations())));

        assert!(registry.get("mock").is_some());
        assert_eq!(registry.get("mock").unwrap().name(), "Mock Provider");
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_registry_list_ids() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new(test_stations())));
        assert_eq!(registry.list_ids(), vec!["mock"]);
    }

    #[test]
    fn test_registry_list_names() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new(test_stations())));
        assert_eq!(registry.list_names(), vec!["Mock Provider"]);
    }

    #[test]
    fn test_registry_search_all() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new(test_stations())));

        let results = registry.search_all("rock", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Rock FM");
    }

    #[test]
    fn test_registry_search_all_no_results() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new(test_stations())));

        let results = registry.search_all("classical", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_registry_search_all_empty_registry() {
        let registry = ProviderRegistry::new();
        let results = registry.search_all("anything", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_mock_provider_search_with_offset() {
        let provider = MockProvider::new(test_stations());
        let results = provider.search("", 2, 1).unwrap();
        assert_eq!(results.stations.len(), 2);
        assert_eq!(results.stations[0].name, "Jazz Radio");
    }

    #[test]
    fn test_mock_provider_get_station() {
        let provider = MockProvider::new(test_stations());
        let station = provider.get_station("2").unwrap();
        assert!(station.is_some());
        assert_eq!(station.unwrap().name, "Jazz Radio");

        let missing = provider.get_station("999").unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn test_mock_provider_get_popular() {
        let provider = MockProvider::new(test_stations());
        let popular = provider.get_popular(2).unwrap();
        assert_eq!(popular.len(), 2);
    }

    #[test]
    fn test_mock_provider_browse_categories() {
        let provider = MockProvider::new(test_stations());
        let categories = provider.browse_categories().unwrap();
        assert_eq!(categories.len(), 1);
        assert_eq!(categories[0].id, "rock");
    }

    #[test]
    fn test_mock_provider_report_click_default() {
        let provider = MockProvider::new(test_stations());
        let station = Station::new("Test", "http://test.com");
        assert!(provider.report_click(&station).is_ok());
    }

    #[test]
    fn test_mock_provider_browse_category() {
        let provider = MockProvider::new(test_stations());
        let category = Category::new("rock", "Rock", CategoryType::Genre);
        let results = provider.browse_category(&category, 2, 0).unwrap();
        assert_eq!(results.stations.len(), 2);
        assert!(!results.has_more);
    }

    // --- Multiple providers ---

    /// A second mock provider to test registry with 2+ providers
    struct MockProvider2;

    impl StationProvider for MockProvider2 {
        fn name(&self) -> &'static str {
            "Mock Provider 2"
        }

        fn id(&self) -> &'static str {
            "mock-2"
        }

        fn search(&self, _query: &str, limit: usize, _offset: usize) -> Result<SearchResults> {
            let stations: Vec<Station> = vec![Station::new("Extra FM", "http://extra.fm/stream")
                .with_provider("mock-2", Some("10".to_string()))]
            .into_iter()
            .take(limit)
            .collect();
            Ok(SearchResults {
                total: Some(stations.len()),
                has_more: false,
                stations,
            })
        }

        fn browse_categories(&self) -> Result<Vec<Category>> {
            Ok(vec![])
        }

        fn browse_category(
            &self,
            _category: &Category,
            _limit: usize,
            _offset: usize,
        ) -> Result<SearchResults> {
            Ok(SearchResults::empty())
        }

        fn get_popular(&self, _limit: usize) -> Result<Vec<Station>> {
            Ok(vec![])
        }

        fn get_station(&self, _id: &str) -> Result<Option<Station>> {
            Ok(None)
        }
    }

    /// A provider that always errors on search
    struct FailingProvider;

    impl StationProvider for FailingProvider {
        fn name(&self) -> &'static str {
            "Failing Provider"
        }

        fn id(&self) -> &'static str {
            "failing"
        }

        fn search(&self, _query: &str, _limit: usize, _offset: usize) -> Result<SearchResults> {
            let err = reqwest::blocking::Client::new()
                .get("http://invalid.invalid.invalid")
                .send()
                .unwrap_err();
            Err(err.into())
        }

        fn browse_categories(&self) -> Result<Vec<Category>> {
            Ok(vec![])
        }

        fn browse_category(
            &self,
            _category: &Category,
            _limit: usize,
            _offset: usize,
        ) -> Result<SearchResults> {
            Ok(SearchResults::empty())
        }

        fn get_popular(&self, _limit: usize) -> Result<Vec<Station>> {
            Ok(vec![])
        }

        fn get_station(&self, _id: &str) -> Result<Option<Station>> {
            Ok(None)
        }
    }

    #[test]
    fn test_registry_multiple_providers_list_ids() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new(test_stations())));
        registry.register(Box::new(MockProvider2));
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.list_ids(), vec!["mock", "mock-2"]);
        assert_eq!(
            registry.list_names(),
            vec!["Mock Provider", "Mock Provider 2"]
        );
    }

    #[test]
    fn test_registry_search_all_merges_multiple_providers() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new(test_stations())));
        registry.register(Box::new(MockProvider2));

        // Empty query matches all in MockProvider, MockProvider2 always returns "Extra FM"
        let results = registry.search_all("", 10).unwrap();
        assert_eq!(results.len(), 4); // 3 from mock + 1 from mock-2
        assert!(results.iter().any(|s| s.name == "Rock FM"));
        assert!(results.iter().any(|s| s.name == "Extra FM"));
    }

    #[test]
    fn test_registry_search_all_skips_failing_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(FailingProvider));
        registry.register(Box::new(MockProvider::new(test_stations())));

        // FailingProvider errors, but MockProvider results still come through
        let results = registry.search_all("rock", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Rock FM");
    }
}
