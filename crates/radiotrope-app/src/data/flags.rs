//! Bundled country flags
//!
//! Small PNG flags (40x30) compiled into the binary from `assets/flags/`, so
//! showing a flag never touches the network or the disk.

include!(concat!(env!("OUT_DIR"), "/flags.rs"));

/// Country names used by radio-browser (after [`normalize`]) that differ from
/// the names in the bundled table.
const ALIASES: &[(&str, &str)] = &[
    ("brunei", "bn"),
    ("cape verde", "cv"),
    ("congo", "cg"),
    ("cote d'ivoire", "ci"),
    ("czechia", "cz"),
    ("democratic people's republic of korea", "kp"),
    ("ivory coast", "ci"),
    ("lao people's democratic republic", "la"),
    ("micronesia", "fm"),
    ("palestine", "ps"),
    ("republic of korea", "kr"),
    ("republic of moldova", "md"),
    ("republic of north macedonia", "mk"),
    ("russian federation", "ru"),
    ("swaziland", "sz"),
    ("syrian arab republic", "sy"),
    ("turkey", "tr"),
    ("united kingdom of great britain and northern ireland", "gb"),
    ("united republic of tanzania", "tz"),
    ("united states", "us"),
    ("usa", "us"),
    ("vatican", "va"),
    ("viet nam", "vn"),
];

/// Get the PNG flag for an ISO 3166-1 alpha-2 code (case-insensitive)
pub fn flag_png(code: &str) -> Option<&'static [u8]> {
    let code = code.trim().to_ascii_lowercase();
    FLAGS
        .binary_search_by(|(c, _)| (*c).cmp(code.as_str()))
        .ok()
        .map(|i| FLAGS[i].1)
}

/// Best-effort ISO 3166-1 alpha-2 code for a country name
///
/// Accepts names like "Germany", "The United States Of America" or
/// "Germany, Bavaria" (country plus state, as stored on stations).
pub fn code_for_country(name: &str) -> Option<&'static str> {
    let name = normalize(name);
    if name.is_empty() {
        return None;
    }
    if let Some((_, code)) = ALIASES.iter().find(|(n, _)| *n == name) {
        return Some(code);
    }
    COUNTRY_NAMES
        .binary_search_by(|(n, _)| (*n).cmp(name.as_str()))
        .ok()
        .map(|i| COUNTRY_NAMES[i].1)
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

/// Lowercase, drop a ", state" suffix, a "(...)" qualifier and a leading "the "
fn normalize(name: &str) -> String {
    let name = name.split(',').next().unwrap_or("");
    let name = name.split('(').next().unwrap_or("");
    let name = name.trim().to_lowercase();
    let name = name.strip_prefix("the ").unwrap_or(&name);
    name.replace('ô', "o")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_sorted_for_binary_search() {
        assert!(FLAGS.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(COUNTRY_NAMES.windows(2).all(|w| w[0].0 < w[1].0));
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
    fn every_name_and_alias_has_a_flag() {
        for (name, code) in COUNTRY_NAMES.iter().chain(ALIASES) {
            assert!(flag_png(code).is_some(), "{name} -> {code} has no flag");
        }
    }

    #[test]
    fn code_for_plain_names() {
        assert_eq!(code_for_country("Greece"), Some("gr"));
        assert_eq!(code_for_country("germany"), Some("de"));
    }

    #[test]
    fn code_for_radio_browser_names() {
        assert_eq!(code_for_country("The United States Of America"), Some("us"));
        assert_eq!(
            code_for_country("The United Kingdom Of Great Britain And Northern Ireland"),
            Some("gb")
        );
        assert_eq!(code_for_country("The Russian Federation"), Some("ru"));
        assert_eq!(code_for_country("The Netherlands"), Some("nl"));
        assert_eq!(code_for_country("Iran (Islamic Republic Of)"), Some("ir"));
        assert_eq!(code_for_country("Côte D'Ivoire"), Some("ci"));
    }

    #[test]
    fn code_for_country_with_state() {
        assert_eq!(
            code_for_country("The United States Of America, California"),
            Some("us")
        );
    }

    #[test]
    fn code_for_unknown_country() {
        assert_eq!(code_for_country("Atlantis"), None);
        assert_eq!(code_for_country(""), None);
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
