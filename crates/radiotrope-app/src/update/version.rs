//! Release versions (`0.2.0`, `v1.0.0-beta.2`) and their order

use std::cmp::Ordering;
use std::fmt;

/// A semantic version. Build metadata (`+...`) is read and dropped: it
/// doesn't take part in the order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// The part after `-`, e.g. "beta.2"; empty for a release
    pub pre: String,
}

impl Version {
    /// Read `1.2.3`, with an optional leading `v` and `-pre` and `+build`
    /// parts. `None` for anything else: a tag that isn't a version is
    /// never offered as an update.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix(['v', 'V']).unwrap_or(text);
        let text = text.split_once('+').map_or(text, |(version, _)| version);
        let (numbers, pre) = match text.split_once('-') {
            Some((numbers, pre)) => (numbers, pre),
            None => (text, ""),
        };
        if text.contains('-') && !valid_pre(pre) {
            return None;
        }
        let mut parts = numbers.split('.');
        let mut number = || {
            let part = parts.next()?;
            let plain = !part.is_empty()
                && part.len() <= 9
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part == "0" || !part.starts_with('0'));
            plain.then(|| part.parse().ok())?
        };
        let version = Version {
            major: number()?,
            minor: number()?,
            patch: number()?,
            pre: pre.to_string(),
        };
        parts.next().is_none().then_some(version)
    }

    /// The version of this build
    pub fn current() -> Self {
        Self::parse(env!("CARGO_PKG_VERSION")).expect("the package version is a version")
    }

    /// A release, not a pre-release
    pub fn is_release(&self) -> bool {
        self.pre.is_empty()
    }
}

/// Dot-separated identifiers of ASCII letters, digits and hyphens
fn valid_pre(pre: &str) -> bool {
    pre.len() <= 64
        && pre
            .split('.')
            .all(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre)?;
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // A release comes after its pre-releases
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => cmp_pre(&self.pre, &other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Pre-release order (semver 2.0, rule 11): identifier by identifier,
/// numbers by value and before words, and the shorter list first
fn cmp_pre(a: &str, b: &str) -> Ordering {
    let mut a = a.split('.');
    let mut b = b.split('.');
    loop {
        match (a.next(), b.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let order = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("{text} is a version"))
    }

    #[test]
    fn versions_are_read_with_or_without_the_v() {
        assert_eq!(v("0.2.0"), v("v0.2.0"));
        assert_eq!(v(" v1.10.3\n").to_string(), "1.10.3");
        assert_eq!(v("1.0.0-beta.2+build5").to_string(), "1.0.0-beta.2");
        assert!(v("1.0.0").is_release());
        assert!(!v("1.0.0-rc.1").is_release());
    }

    #[test]
    fn what_is_not_a_version_is_refused() {
        for bad in [
            "",
            "v",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "01.2.3",
            "1.2.3-",
            "1.2.3-a..b",
            "1.2.3-a b",
            "latest",
            "-1.2.3",
            "1.2.99999999999999999999",
            "1.2.3 ; rm -rf",
            "../1.2.3",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn versions_compare_by_number_not_by_text() {
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.2.1") > v("0.2.0"));
        assert_eq!(v("0.2.0").cmp(&v("v0.2.0")), Ordering::Equal);
        assert!(v("0.1.0") < v("0.2.0"));
    }

    #[test]
    fn a_release_comes_after_its_pre_releases() {
        // The order in the semver specification
        let order = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
        ];
        for pair in order.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert!(v("1.0.1-alpha") > v("1.0.0"));
    }

    #[test]
    fn this_build_has_a_version() {
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }
}
