//! Language names for ISO 639-3 codes
//!
//! The station directory gives languages as ISO 639-3 codes (`eng`, `ell`);
//! people read and type their names.

/// ISO 639-3 code to English name, sorted by code
const LANGUAGES: &[(&str, &str)] = &[
    ("afr", "Afrikaans"),
    ("amh", "Amharic"),
    ("ara", "Arabic"),
    ("arb", "Arabic"),
    ("aze", "Azerbaijani"),
    ("bel", "Belarusian"),
    ("ben", "Bengali"),
    ("bos", "Bosnian"),
    ("bre", "Breton"),
    ("bul", "Bulgarian"),
    ("cat", "Catalan"),
    ("ces", "Czech"),
    ("cmn", "Mandarin"),
    ("cym", "Welsh"),
    ("dan", "Danish"),
    ("deu", "German"),
    ("ell", "Greek"),
    ("eng", "English"),
    ("epo", "Esperanto"),
    ("est", "Estonian"),
    ("eus", "Basque"),
    ("fao", "Faroese"),
    ("fas", "Persian"),
    ("fil", "Filipino"),
    ("fin", "Finnish"),
    ("fra", "French"),
    ("fry", "Frisian"),
    ("gla", "Scottish Gaelic"),
    ("gle", "Irish"),
    ("glg", "Galician"),
    ("guj", "Gujarati"),
    ("hat", "Haitian Creole"),
    ("hau", "Hausa"),
    ("heb", "Hebrew"),
    ("hin", "Hindi"),
    ("hrv", "Croatian"),
    ("hun", "Hungarian"),
    ("hye", "Armenian"),
    ("ind", "Indonesian"),
    ("isl", "Icelandic"),
    ("ita", "Italian"),
    ("jpn", "Japanese"),
    ("kan", "Kannada"),
    ("kat", "Georgian"),
    ("kaz", "Kazakh"),
    ("khm", "Khmer"),
    ("kir", "Kyrgyz"),
    ("kor", "Korean"),
    ("kur", "Kurdish"),
    ("lao", "Lao"),
    ("lat", "Latin"),
    ("lav", "Latvian"),
    ("lit", "Lithuanian"),
    ("ltz", "Luxembourgish"),
    ("mal", "Malayalam"),
    ("mar", "Marathi"),
    ("mkd", "Macedonian"),
    ("mlt", "Maltese"),
    ("mon", "Mongolian"),
    ("msa", "Malay"),
    ("mya", "Burmese"),
    ("nep", "Nepali"),
    ("nld", "Dutch"),
    ("nno", "Norwegian Nynorsk"),
    ("nob", "Norwegian Bokmål"),
    ("nor", "Norwegian"),
    ("oci", "Occitan"),
    ("pan", "Punjabi"),
    ("pol", "Polish"),
    ("por", "Portuguese"),
    ("pus", "Pashto"),
    ("que", "Quechua"),
    ("roh", "Romansh"),
    ("ron", "Romanian"),
    ("rus", "Russian"),
    ("sin", "Sinhala"),
    ("slk", "Slovak"),
    ("slv", "Slovenian"),
    ("sme", "Northern Sami"),
    ("som", "Somali"),
    ("spa", "Spanish"),
    ("sqi", "Albanian"),
    ("srp", "Serbian"),
    ("swa", "Swahili"),
    ("swe", "Swedish"),
    ("tam", "Tamil"),
    ("tel", "Telugu"),
    ("tgk", "Tajik"),
    ("tha", "Thai"),
    ("tur", "Turkish"),
    ("ukr", "Ukrainian"),
    ("urd", "Urdu"),
    ("uzb", "Uzbek"),
    ("vie", "Vietnamese"),
    ("yid", "Yiddish"),
    ("yor", "Yoruba"),
    ("yue", "Cantonese"),
    ("zho", "Chinese"),
    ("zul", "Zulu"),
];

/// English name of an ISO 639-3 code (case-insensitive)
pub fn language_name(code: &str) -> Option<&'static str> {
    let code = code.trim().to_ascii_lowercase();
    LANGUAGES
        .binary_search_by(|(c, _)| (*c).cmp(code.as_str()))
        .ok()
        .map(|i| LANGUAGES[i].1)
}

/// ISO 639-3 code for a language name ("Greek", "greek") or a code
/// itself ("ell"), when it is one this table knows
pub fn language_code(name: &str) -> Option<&'static str> {
    let name = name.trim();
    if let Some(i) = LANGUAGES
        .iter()
        .position(|(c, _)| c.eq_ignore_ascii_case(name))
    {
        return Some(LANGUAGES[i].0);
    }
    LANGUAGES
        .iter()
        .find(|(_, n)| n.eq_ignore_ascii_case(name))
        .map(|(c, _)| *c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_for_binary_search() {
        assert!(LANGUAGES.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn names_and_codes_go_both_ways() {
        assert_eq!(language_name("ell"), Some("Greek"));
        assert_eq!(language_name("ENG"), Some("English"));
        assert_eq!(language_name("xxx"), None);
        assert_eq!(language_code("greek"), Some("ell"));
        assert_eq!(language_code(" English "), Some("eng"));
        assert_eq!(language_code("deu"), Some("deu"));
        assert_eq!(language_code("Klingon"), None);
        // Two codes share a name: the first one is used
        assert_eq!(language_code("Arabic"), Some("ara"));
    }
}
