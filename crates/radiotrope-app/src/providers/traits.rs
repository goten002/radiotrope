//! Station provider trait
//!
//! Defines the interface that all station directory providers must implement.

use crate::data::types::Station;
use crate::error::Result;

use super::types::{Category, SearchResults};

/// A source of radio station listings
///
/// Implementations provide search, browse, and lookup capabilities
/// for a specific station directory service.
pub trait StationProvider: Send + Sync {
    /// Display name for the provider (e.g., "Radio Browser")
    fn name(&self) -> &'static str;

    /// Machine-readable identifier (e.g., "radio-browser")
    ///
    /// Stored in `Station.provider` to track origin.
    fn id(&self) -> &'static str;

    /// Optional provider icon (PNG bytes)
    fn icon(&self) -> Option<&'static [u8]> {
        None
    }

    /// Search for stations by text query
    fn search(&self, query: &str, limit: usize, offset: usize) -> Result<SearchResults>;

    /// List available browse categories (genres, countries, languages)
    fn browse_categories(&self) -> Result<Vec<Category>>;

    /// Browse stations within a specific category
    fn browse_category(
        &self,
        category: &Category,
        limit: usize,
        offset: usize,
    ) -> Result<SearchResults>;

    /// Get popular/trending stations
    fn get_popular(&self, limit: usize) -> Result<Vec<Station>>;

    /// Look up a single station by its provider-specific ID
    fn get_station(&self, id: &str) -> Result<Option<Station>>;

    /// Report that a station was clicked/played (for popularity tracking)
    fn report_click(&self, _station: &Station) -> Result<()> {
        Ok(())
    }
}
