//! Bundled country flags
//!
//! Small PNG flags (40x30) compiled into the binary from `assets/flags/`, so
//! showing a flag never touches the network or the disk.

include!(concat!(env!("OUT_DIR"), "/flags.rs"));

pub use radiotrope_library::data::countries::{code_for_country, country_name};

/// Get the PNG flag for an ISO 3166-1 alpha-2 code (case-insensitive)
pub fn flag_png(code: &str) -> Option<&'static [u8]> {
    let code = code.trim().to_ascii_lowercase();
    FLAGS
        .binary_search_by(|(c, _)| (*c).cmp(code.as_str()))
        .ok()
        .map(|i| FLAGS[i].1)
}

/// Pick the flag code for a station: its ISO code if a flag exists for it,
/// otherwise a lookup by country name.
pub fn flag_code(country_code: Option<&str>, country: Option<&str>) -> Option<String> {
    if let Some(code) = country_code {
        if flag_png(code).is_some() {
            return Some(code.trim().to_ascii_lowercase());
        }
    }
    country
        .and_then(code_for_country)
        .map(|code| code.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_are_sorted_for_binary_search() {
        assert!(FLAGS.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn flag_png_is_a_png() {
        let png = flag_png("GR").unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[test]
    fn flag_png_unknown_code() {
        assert!(flag_png("zz").is_none());
        assert!(flag_png("").is_none());
    }

    #[test]
    fn every_country_name_has_a_flag() {
        let names = include_str!("../../../radiotrope-library/assets/country-names.tsv");
        for (name, code) in names.lines().filter_map(|line| line.split_once('\t')) {
            assert!(flag_png(code).is_some(), "{name} -> {code} has no flag");
        }
    }

    #[test]
    fn flag_code_prefers_iso_code() {
        assert_eq!(flag_code(Some("DE"), Some("Greece")), Some("de".into()));
    }

    #[test]
    fn flag_code_falls_back_to_name() {
        assert_eq!(flag_code(None, Some("Greece")), Some("gr".into()));
        assert_eq!(flag_code(Some("??"), Some("Greece")), Some("gr".into()));
        assert_eq!(flag_code(None, None), None);
    }
}
