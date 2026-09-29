//! Text helpers for the UI

/// Digit with the widest advance in the bundled Inter font (SemiBold and
/// Regular); "1" is the narrowest
const WIDEST_DIGIT: char = '4';

/// `text` with every ASCII digit replaced by the widest one, for sizing a
/// label whose digits change (a timer, a file size) so it doesn't shift
/// width on every tick. Inter's default digits are proportional and Slint
/// can't turn on its tabular figures.
pub fn steady_digits(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_digit() { WIDEST_DIGIT } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_digits_replaces_only_digits() {
        assert_eq!(steady_digits("Playing 01:17"), "Playing 44:44");
        assert_eq!(steady_digits("REC 0:01 · 71.1 KB"), "REC 4:44 · 44.4 KB");
        assert_eq!(steady_digits("Stopped"), "Stopped");
        assert_eq!(steady_digits(""), "");
    }
}
