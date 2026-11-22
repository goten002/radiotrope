//! Hex colors typed or pasted into the Accent Color dialog

/// Parse a hex color as a person types or pastes it: `#c97718`,
/// `c97718` or the short `#abc` (`#aabbcc`), in any case, with spaces
/// around it. Anything else is `None`.
pub fn parse_hex_color(text: &str) -> Option<(u8, u8, u8)> {
    let text = text.trim();
    let hex = text.strip_prefix('#').unwrap_or(text);
    // Checked byte by byte first, so non-ASCII text is never sliced
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let digit = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok();
    match hex.len() {
        3 => {
            let (r, g, b) = (digit(0)?, digit(1)?, digit(2)?);
            Some((r * 17, g * 17, b * 17))
        }
        6 => {
            let rgb = u32::from_str_radix(hex, 16).ok()?;
            Some(((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8))
        }
        _ => None,
    }
}

/// `#rrggbb` in lower case, as the dialog shows it and settings save it
pub fn format_hex_color(r: u8, g: u8, b: u8) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_and_short_forms_parse_in_any_case() {
        assert_eq!(parse_hex_color("#c97718"), Some((0xc9, 0x77, 0x18)));
        assert_eq!(parse_hex_color("c97718"), Some((0xc9, 0x77, 0x18)));
        assert_eq!(parse_hex_color("#C97718"), Some((0xc9, 0x77, 0x18)));
        assert_eq!(parse_hex_color("#abc"), Some((0xaa, 0xbb, 0xcc)));
        assert_eq!(parse_hex_color("ABC"), Some((0xaa, 0xbb, 0xcc)));
        assert_eq!(parse_hex_color("#000"), Some((0, 0, 0)));
        assert_eq!(parse_hex_color("#FfFfFf"), Some((255, 255, 255)));
    }

    #[test]
    fn spaces_around_are_trimmed() {
        assert_eq!(parse_hex_color("  #c97718 "), Some((0xc9, 0x77, 0x18)));
        assert_eq!(parse_hex_color("\tabc\n"), Some((0xaa, 0xbb, 0xcc)));
    }

    #[test]
    fn invalid_text_is_refused() {
        for text in [
            "", " ", "#", "##abc", "#ab", "#abcd", "#abcde", "#1234567", "#gg0000", "#12 345",
            "# 123456", "red", "#+f+f+f", "0x123456", "#aé123", "αβγ", "#ébc",
        ] {
            assert_eq!(parse_hex_color(text), None, "{text:?}");
        }
    }

    #[test]
    fn formats_lower_case_with_a_hash() {
        assert_eq!(format_hex_color(0xc9, 0x77, 0x18), "#c97718");
        assert_eq!(format_hex_color(0, 10, 255), "#000aff");
    }

    #[test]
    fn formatted_colors_parse_back() {
        let s = format_hex_color(1, 2, 3);
        assert_eq!(parse_hex_color(&s), Some((1, 2, 3)));
    }
}
