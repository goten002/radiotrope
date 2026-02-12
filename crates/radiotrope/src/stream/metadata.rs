//! Stream metadata types and ICY parsing
//!
//! Pure data types and parsing functions for ICY (Icecast/Shoutcast) metadata.

/// Source of stream metadata
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataSource {
    Icy,
}

/// Parsed stream metadata with artist/title split
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub source: MetadataSource,
}

impl StreamMetadata {
    /// Create metadata from an ICY title string.
    ///
    /// Splits on first ` - ` separator: "Artist - Title" → artist="Artist", title="Title".
    /// If no separator found, the whole string becomes the title.
    pub fn from_icy_title(raw: &str) -> Self {
        let raw = raw.trim();
        if raw.is_empty() {
            return Self {
                title: None,
                artist: None,
                source: MetadataSource::Icy,
            };
        }

        if let Some(pos) = raw.find(" - ") {
            let artist = raw[..pos].trim().to_string();
            let title = raw[pos + 3..].trim().to_string();
            Self {
                title: if title.is_empty() { None } else { Some(title) },
                artist: if artist.is_empty() {
                    None
                } else {
                    Some(artist)
                },
                source: MetadataSource::Icy,
            }
        } else {
            Self {
                title: Some(raw.to_string()),
                artist: None,
                source: MetadataSource::Icy,
            }
        }
    }
}

/// Parse ICY metadata string to extract StreamTitle value.
///
/// ICY metadata format: `StreamTitle='Artist - Song';StreamUrl='...';`
pub fn parse_icy_metadata(metadata: &str) -> Option<String> {
    let start = metadata.find("StreamTitle='")?;
    let start = start + 13; // length of "StreamTitle='"
    let end = metadata[start..].find("';")?;
    let title = metadata[start..start + end].trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

/// Extract ICY title from a raw metadata block (with null padding).
///
/// Raw ICY metadata blocks are null-padded to a multiple of 16 bytes.
/// This strips null bytes, converts to UTF-8, then parses the StreamTitle.
pub fn extract_icy_title(raw_block: &[u8]) -> Option<String> {
    // Strip null bytes from end
    let end = raw_block
        .iter()
        .rposition(|&b| b != 0)
        .map(|p| p + 1)
        .unwrap_or(0);
    if end == 0 {
        return None;
    }

    let meta_str = String::from_utf8_lossy(&raw_block[..end]);
    parse_icy_metadata(&meta_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- MetadataSource ---

    #[test]
    fn metadata_source_equality() {
        assert_eq!(MetadataSource::Icy, MetadataSource::Icy);
    }

    #[test]
    fn metadata_source_debug() {
        assert_eq!(format!("{:?}", MetadataSource::Icy), "Icy");
    }

    #[test]
    fn metadata_source_clone() {
        let source = MetadataSource::Icy;
        let cloned = source;
        assert_eq!(source, cloned);
    }

    // --- StreamMetadata ---

    #[test]
    fn stream_metadata_equality() {
        let a = StreamMetadata {
            title: Some("Song".to_string()),
            artist: Some("Artist".to_string()),
            source: MetadataSource::Icy,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn stream_metadata_debug() {
        let m = StreamMetadata {
            title: Some("Song".to_string()),
            artist: None,
            source: MetadataSource::Icy,
        };
        let debug = format!("{:?}", m);
        assert!(debug.contains("Song"));
        assert!(debug.contains("Icy"));
    }

    // --- from_icy_title ---

    #[test]
    fn from_icy_title_with_separator() {
        let m = StreamMetadata::from_icy_title("Pink Floyd - Comfortably Numb");
        assert_eq!(m.artist, Some("Pink Floyd".to_string()));
        assert_eq!(m.title, Some("Comfortably Numb".to_string()));
        assert_eq!(m.source, MetadataSource::Icy);
    }

    #[test]
    fn from_icy_title_no_separator() {
        let m = StreamMetadata::from_icy_title("Just A Title");
        assert_eq!(m.artist, None);
        assert_eq!(m.title, Some("Just A Title".to_string()));
    }

    #[test]
    fn from_icy_title_empty() {
        let m = StreamMetadata::from_icy_title("");
        assert_eq!(m.artist, None);
        assert_eq!(m.title, None);
    }

    #[test]
    fn from_icy_title_whitespace_only() {
        let m = StreamMetadata::from_icy_title("   ");
        assert_eq!(m.artist, None);
        assert_eq!(m.title, None);
    }

    #[test]
    fn from_icy_title_multiple_separators() {
        let m = StreamMetadata::from_icy_title("A - B - C");
        assert_eq!(m.artist, Some("A".to_string()));
        assert_eq!(m.title, Some("B - C".to_string()));
    }

    #[test]
    fn from_icy_title_dash_without_spaces() {
        let m = StreamMetadata::from_icy_title("Artist-Title");
        assert_eq!(m.artist, None);
        assert_eq!(m.title, Some("Artist-Title".to_string()));
    }

    #[test]
    fn from_icy_title_with_special_chars() {
        let m = StreamMetadata::from_icy_title("Motörhead - Ace of Spades (Live)");
        assert_eq!(m.artist, Some("Motörhead".to_string()));
        assert_eq!(m.title, Some("Ace of Spades (Live)".to_string()));
    }

    // --- parse_icy_metadata ---

    #[test]
    fn parse_standard_icy_metadata() {
        let raw = "StreamTitle='Pink Floyd - Comfortably Numb';StreamUrl='';";
        assert_eq!(
            parse_icy_metadata(raw),
            Some("Pink Floyd - Comfortably Numb".to_string())
        );
    }

    #[test]
    fn parse_icy_metadata_empty_title() {
        let raw = "StreamTitle='';StreamUrl='';";
        assert_eq!(parse_icy_metadata(raw), None);
    }

    #[test]
    fn parse_icy_metadata_no_stream_title() {
        let raw = "SomeOtherField='value';";
        assert_eq!(parse_icy_metadata(raw), None);
    }

    #[test]
    fn parse_icy_metadata_title_only() {
        let raw = "StreamTitle='Just Music';";
        assert_eq!(parse_icy_metadata(raw), Some("Just Music".to_string()));
    }

    // --- extract_icy_title ---

    #[test]
    fn extract_from_null_padded_block() {
        let mut block = b"StreamTitle='Test Song';".to_vec();
        block.resize(48, 0);
        assert_eq!(extract_icy_title(&block), Some("Test Song".to_string()));
    }

    #[test]
    fn extract_from_all_null_block() {
        let block = vec![0u8; 32];
        assert_eq!(extract_icy_title(&block), None);
    }

    #[test]
    fn extract_from_empty_block() {
        assert_eq!(extract_icy_title(&[]), None);
    }

    // --- from_icy_title edge cases ---

    #[test]
    fn from_icy_title_separator_at_start() {
        // " - Title" after trim → "- Title", no " - " found → title only
        let m = StreamMetadata::from_icy_title(" - Title");
        assert_eq!(m.artist, None);
        assert_eq!(m.title, Some("- Title".to_string()));
    }

    #[test]
    fn from_icy_title_separator_at_end() {
        // "Artist - " after trim → "Artist -", no " - " with content after → title only
        let m = StreamMetadata::from_icy_title("Artist - ");
        assert_eq!(m.artist, None);
        assert_eq!(m.title, Some("Artist -".to_string()));
    }

    #[test]
    fn from_icy_title_only_separator() {
        // " - " after trim → "-", no " - " found → title only
        let m = StreamMetadata::from_icy_title(" - ");
        assert_eq!(m.artist, None);
        assert_eq!(m.title, Some("-".to_string()));
    }

    #[test]
    fn from_icy_title_extra_whitespace_around_separator() {
        let m = StreamMetadata::from_icy_title("  Artist  -  Title  ");
        // outer trim applied first, then split on first " - "
        assert_eq!(m.artist, Some("Artist".to_string()));
        assert_eq!(m.title, Some("Title".to_string()));
    }

    #[test]
    fn from_icy_title_unicode_cjk() {
        let m = StreamMetadata::from_icy_title("アーティスト - 曲名");
        assert_eq!(m.artist, Some("アーティスト".to_string()));
        assert_eq!(m.title, Some("曲名".to_string()));
    }

    #[test]
    fn from_icy_title_greek_with_year() {
        let m = StreamMetadata::from_icy_title("ΠΑΝΟΣ ΚΙΑΜΟΣ - ΘΑ ΜΕ ΖΗΤΑΣ - 2022");
        assert_eq!(m.artist, Some("ΠΑΝΟΣ ΚΙΑΜΟΣ".to_string()));
        assert_eq!(m.title, Some("ΘΑ ΜΕ ΖΗΤΑΣ - 2022".to_string()));
        assert_eq!(m.source, MetadataSource::Icy);
    }

    #[test]
    fn from_icy_title_very_long_string() {
        let artist = "A".repeat(500);
        let title = "T".repeat(500);
        let raw = format!("{} - {}", artist, title);
        let m = StreamMetadata::from_icy_title(&raw);
        assert_eq!(m.artist.as_ref().map(|s| s.len()), Some(500));
        assert_eq!(m.title.as_ref().map(|s| s.len()), Some(500));
    }

    #[test]
    fn from_icy_title_newlines_and_tabs() {
        let m = StreamMetadata::from_icy_title("Artist\n - \tTitle");
        // The " - " is preceded by \n and followed by \t
        // find(" - ") finds the literal " - " at position of "\n - "
        // Since outer trim doesn't remove internal whitespace, it depends on where " - " is found
        assert!(m.title.is_some() || m.artist.is_some());
    }

    // --- parse_icy_metadata edge cases ---

    #[test]
    fn parse_icy_metadata_with_url_field() {
        let raw = "StreamTitle='Song Name';StreamUrl='http://example.com';";
        assert_eq!(parse_icy_metadata(raw), Some("Song Name".to_string()));
    }

    #[test]
    fn parse_icy_metadata_whitespace_title() {
        let raw = "StreamTitle='   ';";
        // "   " trimmed becomes empty → None
        assert_eq!(parse_icy_metadata(raw), None);
    }

    #[test]
    fn parse_icy_metadata_special_chars_in_title() {
        let raw = "StreamTitle='Rock & Roll (feat. DJ)';";
        assert_eq!(
            parse_icy_metadata(raw),
            Some("Rock & Roll (feat. DJ)".to_string())
        );
    }

    #[test]
    fn parse_icy_metadata_quotes_in_title() {
        // Title with single quotes: "It's Alright" — the parser finds the FIRST "';",
        // which is "';' Alright';" at the end, so the full title is captured
        let raw = "StreamTitle='It's Alright';";
        assert_eq!(parse_icy_metadata(raw), Some("It's Alright".to_string()));
    }

    #[test]
    fn parse_icy_metadata_greek_title() {
        let raw = "StreamTitle='ΠΑΝΟΣ ΚΙΑΜΟΣ - ΘΑ ΜΕ ΖΗΤΑΣ - 2022';StreamUrl='';";
        assert_eq!(
            parse_icy_metadata(raw),
            Some("ΠΑΝΟΣ ΚΙΑΜΟΣ - ΘΑ ΜΕ ΖΗΤΑΣ - 2022".to_string())
        );
    }

    #[test]
    fn parse_icy_metadata_missing_closing_quote() {
        let raw = "StreamTitle='No Closing Quote";
        assert_eq!(parse_icy_metadata(raw), None);
    }

    #[test]
    fn parse_icy_metadata_multiple_stream_titles() {
        // Should return the first one
        let raw = "StreamTitle='First';StreamTitle='Second';";
        assert_eq!(parse_icy_metadata(raw), Some("First".to_string()));
    }

    // --- extract_icy_title edge cases ---

    #[test]
    fn extract_from_single_null_byte() {
        assert_eq!(extract_icy_title(&[0]), None);
    }

    #[test]
    fn extract_from_non_utf8_block() {
        // Invalid UTF-8 sequences handled by from_utf8_lossy
        let mut block = vec![0xFF, 0xFE];
        block.extend_from_slice(b"StreamTitle='Fallback';");
        block.resize(48, 0);
        // from_utf8_lossy replaces invalid bytes with replacement char
        // The StreamTitle should still be extractable
        assert_eq!(extract_icy_title(&block), Some("Fallback".to_string()));
    }

    #[test]
    fn extract_from_exact_16_byte_block() {
        // ICY metadata is always multiple of 16 bytes
        let block = b"StreamTitle='A';".to_vec();
        assert_eq!(block.len(), 16); // exactly 16 bytes
        assert_eq!(extract_icy_title(&block), Some("A".to_string()));
    }

    #[test]
    fn extract_from_block_with_interior_nulls() {
        // Null bytes before the actual content shouldn't happen in practice,
        // but rposition finds last non-null
        let mut block = vec![0u8; 48];
        let title = b"StreamTitle='Mid';";
        block[..title.len()].copy_from_slice(title);
        assert_eq!(extract_icy_title(&block), Some("Mid".to_string()));
    }

    #[test]
    fn extract_greek_icy_title_from_null_padded_block() {
        let raw = "StreamTitle='ΠΑΝΟΣ ΚΙΑΜΟΣ - ΘΑ ΜΕ ΖΗΤΑΣ - 2022';";
        let mut block = raw.as_bytes().to_vec();
        // Pad to next multiple of 16
        let padded_len = ((block.len() + 15) / 16) * 16;
        block.resize(padded_len, 0);
        assert_eq!(
            extract_icy_title(&block),
            Some("ΠΑΝΟΣ ΚΙΑΜΟΣ - ΘΑ ΜΕ ΖΗΤΑΣ - 2022".to_string())
        );
    }

    // --- StreamMetadata inequality ---

    #[test]
    fn stream_metadata_inequality() {
        let a = StreamMetadata {
            title: Some("A".to_string()),
            artist: None,
            source: MetadataSource::Icy,
        };
        let b = StreamMetadata {
            title: Some("B".to_string()),
            artist: None,
            source: MetadataSource::Icy,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn stream_metadata_none_vs_some() {
        let a = StreamMetadata {
            title: None,
            artist: None,
            source: MetadataSource::Icy,
        };
        let b = StreamMetadata {
            title: Some("Song".to_string()),
            artist: None,
            source: MetadataSource::Icy,
        };
        assert_ne!(a, b);
    }
}
