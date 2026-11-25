//! Is a newer version out? One HTTPS request for the latest release, at
//! most once a day. The request carries nothing about the user or the
//! computer: the address, and the User-Agent every request of ours has
//! (`Radiotrope/<version>`).

use std::io::Read;
use std::time::Duration;

use super::{Mode, Version};

/// GitHub's latest release of the project: never a draft or a pre-release
pub const LATEST_URL: &str = "https://api.github.com/repos/goten002/radiotrope/releases/latest";

/// How often the player looks by itself
pub const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// Help > Check for Updates Automatically, before the user has chosen: on
/// in builds whose mode is at least this one, off in the others. `None`
/// turns it off everywhere and `Some(Mode::Notify)` on everywhere (never in
/// builds with updates off).
pub const AUTO_CHECK_BY_DEFAULT: Option<Mode> = Some(Mode::SelfUpdate);

/// The answer is a few kilobytes; more than this is not read
const MAX_ANSWER: u64 = 1024 * 1024;

const TIMEOUT: Duration = Duration::from_secs(20);

/// The automatic check's default for a build of `mode`
pub fn auto_check_default(mode: Mode) -> bool {
    mode != Mode::Off && AUTO_CHECK_BY_DEFAULT.is_some_and(|from| mode >= from)
}

/// Whether the player looks by itself: never with updates off, else the
/// user's choice, or the default while they have made none
pub fn auto_check_on(mode: Mode, choice: Option<bool>) -> bool {
    mode != Mode::Off && choice.unwrap_or_else(|| auto_check_default(mode))
}

/// A day has passed since the last check (`last` and `now` in seconds
/// since 1970). A last check in the future (the clock was set back) counts
/// as none.
pub fn is_due(last: Option<u64>, now: u64) -> bool {
    match last {
        Some(last) if last <= now => now - last >= CHECK_EVERY.as_secs(),
        _ => true,
    }
}

/// Now, in seconds since 1970
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[derive(Debug, PartialEq, Eq)]
pub enum CheckError {
    /// Updates are off in this build or on this computer: nothing was sent
    Disabled,
    /// No answer (offline, a timeout, an error from the server)
    Network(String),
    /// An answer that names no release version
    Answer(String),
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckError::Disabled => write!(f, "updates are off"),
            CheckError::Network(e) => write!(f, "no answer: {e}"),
            CheckError::Answer(e) => write!(f, "unexpected answer: {e}"),
        }
    }
}

/// What a check found
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    UpToDate,
    Newer(Version),
}

/// The version in the answer to [`LATEST_URL`]: its `tag_name`. Drafts
/// and pre-releases are refused even if the server sends one.
pub fn latest_in(answer: &[u8]) -> Result<Version, CheckError> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
        #[serde(default)]
        draft: bool,
        #[serde(default)]
        prerelease: bool,
    }
    let release: Release =
        serde_json::from_slice(answer).map_err(|e| CheckError::Answer(e.to_string()))?;
    if release.draft || release.prerelease {
        return Err(CheckError::Answer("not a published release".into()));
    }
    Version::parse(&release.tag_name)
        .filter(Version::is_release)
        .ok_or_else(|| CheckError::Answer("the tag is not a release version".into()))
}

/// `latest` against the version running
pub fn compare(current: &Version, latest: Version) -> Found {
    if latest > *current {
        Found::Newer(latest)
    } else {
        Found::UpToDate
    }
}

/// Look for a newer version than `current`, getting the answer with
/// `fetch`. With updates off `fetch` is never called.
pub fn check_with(
    mode: Mode,
    current: &Version,
    fetch: impl FnOnce() -> Result<Vec<u8>, CheckError>,
) -> Result<Found, CheckError> {
    if mode == Mode::Off {
        return Err(CheckError::Disabled);
    }
    Ok(compare(current, latest_in(&fetch()?)?))
}

/// Look for a newer version than this build, over the network
pub fn check(mode: Mode) -> Result<Found, CheckError> {
    check_with(mode, &Version::current(), || fetch(LATEST_URL))
}

/// The client for the check and the download: HTTPS only, with our
/// User-Agent and nothing else of ours
pub(super) fn client(timeout: Option<Duration>) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(radiotrope::config::network::user_agent())
        .https_only(true)
        .connect_timeout(TIMEOUT)
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())
}

fn fetch(url: &str) -> Result<Vec<u8>, CheckError> {
    let network = |e: String| CheckError::Network(e);
    let response = client(Some(TIMEOUT))
        .map_err(network)?
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|e| network(e.to_string()))?;
    let mut answer = Vec::new();
    response
        .take(MAX_ANSWER)
        .read_to_end(&mut answer)
        .map_err(|e| network(e.to_string()))?;
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn answer(tag: &str) -> Result<Vec<u8>, CheckError> {
        Ok(
            format!(r#"{{"tag_name":"{tag}","draft":false,"prerelease":false,"assets":[]}}"#)
                .into(),
        )
    }

    #[test]
    fn the_check_is_due_once_a_day() {
        let now = 1_800_000_000;
        assert!(is_due(None, now), "never checked");
        assert!(!is_due(Some(now), now));
        assert!(!is_due(Some(now - 60), now));
        assert!(!is_due(Some(now - DAY + 1), now));
        assert!(is_due(Some(now - DAY), now));
        assert!(is_due(Some(now - 30 * DAY), now));
        // The clock was set back: don't wait for it to catch up
        assert!(is_due(Some(now + 1), now));
        assert!(is_due(Some(now + 400 * DAY), now));
    }

    #[test]
    fn a_build_with_updates_off_never_asks() {
        let asked = std::cell::Cell::new(false);
        let found = check_with(Mode::Off, &v("0.1.0"), || {
            asked.set(true);
            answer("v9.9.9")
        });
        assert_eq!(found, Err(CheckError::Disabled));
        assert!(!asked.get(), "nothing may be sent with updates off");
        // And never by itself, whatever the settings file says
        assert!(!auto_check_on(Mode::Off, Some(true)));
        assert!(!auto_check_on(Mode::Off, None));
        assert!(!auto_check_default(Mode::Off));
    }

    #[test]
    fn the_other_builds_ask_once_per_check() {
        for mode in [Mode::Notify, Mode::SelfUpdate] {
            let asked = std::cell::Cell::new(0);
            let found = check_with(mode, &v("0.1.0"), || {
                asked.set(asked.get() + 1);
                answer("v0.2.0")
            });
            assert_eq!(found, Ok(Found::Newer(v("0.2.0"))));
            assert_eq!(asked.get(), 1);
        }
    }

    #[test]
    fn only_a_newer_release_is_an_update() {
        let check =
            |current: &str, tag: &str| check_with(Mode::Notify, &v(current), || answer(tag));
        assert_eq!(check("0.1.0", "v0.2.0"), Ok(Found::Newer(v("0.2.0"))));
        assert_eq!(check("0.9.0", "v0.10.0"), Ok(Found::Newer(v("0.10.0"))));
        assert_eq!(check("0.2.0", "v0.2.0"), Ok(Found::UpToDate));
        // A build newer than the latest release (from source, a pre-release)
        assert_eq!(check("0.3.0", "v0.2.0"), Ok(Found::UpToDate));
        assert_eq!(check("0.2.0-rc.1", "v0.2.0"), Ok(Found::Newer(v("0.2.0"))));
        assert_eq!(check("0.3.0-dev", "v0.2.0"), Ok(Found::UpToDate));
    }

    #[test]
    fn an_answer_without_a_release_version_is_an_error() {
        let check =
            |body: &str| check_with(Mode::Notify, &v("0.1.0"), || Ok(body.as_bytes().to_vec()));
        for body in [
            "",
            "not json",
            "{}",
            r#"{"message":"API rate limit exceeded"}"#,
            r#"{"tag_name":"nightly"}"#,
            r#"{"tag_name":"v0.2.0","draft":true}"#,
            r#"{"tag_name":"v0.2.0","prerelease":true}"#,
            r#"{"tag_name":"v0.2.0-beta.1"}"#,
            r#"{"tag_name":42}"#,
        ] {
            assert!(
                matches!(check(body), Err(CheckError::Answer(_))),
                "{body:?}"
            );
        }
        assert_eq!(
            check_with(Mode::Notify, &v("0.1.0"), || Err(CheckError::Network(
                "offline".into()
            ))),
            Err(CheckError::Network("offline".into()))
        );
    }

    #[test]
    fn the_default_is_one_constant() {
        // On where the build installs updates itself; the user's choice wins
        assert_eq!(AUTO_CHECK_BY_DEFAULT, Some(Mode::SelfUpdate));
        assert!(auto_check_default(Mode::SelfUpdate));
        assert!(!auto_check_default(Mode::Notify));
        assert!(auto_check_on(Mode::SelfUpdate, None));
        assert!(!auto_check_on(Mode::SelfUpdate, Some(false)));
        assert!(auto_check_on(Mode::Notify, Some(true)));
        assert!(!auto_check_on(Mode::Notify, None));
    }

    #[test]
    fn the_check_goes_to_the_projects_releases_over_https() {
        assert!(LATEST_URL.starts_with("https://api.github.com/repos/goten002/radiotrope/"));
        assert!(LATEST_URL.ends_with("/releases/latest"));
    }
}
