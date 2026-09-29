//! Error types for Radiotrope app services
//!
//! Application-level errors that wrap engine errors and add app-specific variants.

use radiotrope::error::RadioError;
use thiserror::Error;

/// Application error type
#[derive(Error, Debug)]
pub enum AppError {
    #[error(transparent)]
    Engine(#[from] RadioError),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Image error: {0}")]
    Image(String),

    #[error("Invalid response: {0}")]
    InvalidResponse(String),
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        AppError::Engine(RadioError::Network(e))
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Engine(RadioError::Io(e))
    }
}

/// Result type alias for Radiotrope app services
pub type Result<T> = std::result::Result<T, AppError>;

/// Why a request to an online service failed, as the user should hear it
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceProblem {
    /// The server's name could not be looked up: most likely no internet
    Offline,
    /// The server could not be reached (refused, unreachable)
    Unreachable,
    /// The server took too long to answer
    TimedOut,
    /// The server asked us to slow down (HTTP 429)
    Busy,
    /// The server failed (HTTP 5xx)
    ServerError(u16),
    /// The server refused the request (HTTP 4xx)
    Refused(u16),
    /// Anything else, already in words
    Other(String),
}

impl ServiceProblem {
    /// Classify an error from a request to an online service
    pub fn of(e: &AppError) -> Self {
        let AppError::Engine(RadioError::Network(re)) = e else {
            return Self::Other(e.to_string());
        };
        let mut chain = String::new();
        let mut source: Option<&dyn std::error::Error> = Some(re);
        while let Some(s) = source {
            chain.push_str(&s.to_string().to_lowercase());
            chain.push('\n');
            source = s.source();
        }
        Self::classify(
            re.is_timeout(),
            re.is_connect(),
            re.status().map(|s| s.as_u16()),
            &chain,
        )
        .unwrap_or_else(|| Self::Other(e.to_string()))
    }

    fn classify(timeout: bool, connect: bool, status: Option<u16>, chain: &str) -> Option<Self> {
        // Name lookup failures read differently on each system
        const LOOKUP_FAILED: [&str; 5] = [
            "dns error",
            "failed to lookup address",
            "name or service not known",
            "temporary failure in name resolution",
            "no such host is known",
        ];
        if let Some(code) = status {
            return Some(match code {
                429 => Self::Busy,
                500..=599 => Self::ServerError(code),
                400..=499 => Self::Refused(code),
                _ => return None,
            });
        }
        if timeout {
            return Some(Self::TimedOut);
        }
        if LOOKUP_FAILED.iter().any(|m| chain.contains(m)) {
            return Some(Self::Offline);
        }
        connect.then_some(Self::Unreachable)
    }

    /// Seconds to wait before trying again after `failures` failures in a
    /// row: 5, 10, 20, 40, then every minute. A server asking us to slow
    /// down gets at least half a minute.
    pub fn retry_delay_secs(&self, failures: u32) -> u32 {
        let delay = (5u32 << failures.saturating_sub(1).min(4)).min(60);
        if *self == Self::Busy {
            delay.max(30)
        } else {
            delay
        }
    }

    /// Short words for the problem, fit for a status line
    pub fn message(&self) -> String {
        match self {
            Self::Offline => "No internet connection".into(),
            Self::Unreachable => "Can't reach the station directory".into(),
            Self::TimedOut => "Connection timed out".into(),
            Self::Busy => "The station directory is busy".into(),
            Self::ServerError(code) => format!("The station directory failed (error {code})"),
            Self::Refused(code) => format!("The station directory refused the request ({code})"),
            Self::Other(text) => text.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ServiceProblem as P;

    #[test]
    fn test_status_codes_win() {
        assert_eq!(P::classify(false, false, Some(429), ""), Some(P::Busy));
        assert_eq!(
            P::classify(false, false, Some(503), ""),
            Some(P::ServerError(503))
        );
        assert_eq!(
            P::classify(false, false, Some(404), ""),
            Some(P::Refused(404))
        );
    }

    #[test]
    fn test_retry_delay_backs_off_to_a_minute() {
        let delays: Vec<u32> = (1..=7).map(|n| P::TimedOut.retry_delay_secs(n)).collect();
        assert_eq!(delays, [5, 10, 20, 40, 60, 60, 60]);
        assert_eq!(P::Busy.retry_delay_secs(1), 30);
        assert_eq!(P::Busy.retry_delay_secs(5), 60);
    }

    #[test]
    fn test_timeout() {
        assert_eq!(P::classify(true, false, None, ""), Some(P::TimedOut));
    }

    #[test]
    fn test_lookup_failure_means_offline() {
        let linux = "error sending request\nclient error (connect)\ndns error\nfailed to lookup address information: temporary failure in name resolution\n";
        assert_eq!(P::classify(false, true, None, linux), Some(P::Offline));
        let windows = "error sending request\ndns error\nno such host is known. (os error 11001)\n";
        assert_eq!(P::classify(false, true, None, windows), Some(P::Offline));
    }

    #[test]
    fn test_other_connect_failure_is_unreachable() {
        let refused =
            "error sending request\ntcp connect error\nconnection refused (os error 111)\n";
        assert_eq!(
            P::classify(false, true, None, refused),
            Some(P::Unreachable)
        );
    }

    #[test]
    fn test_unknown_is_left_to_caller() {
        assert_eq!(P::classify(false, false, None, "body error"), None);
    }

    #[test]
    fn test_other_error_keeps_its_words() {
        let e = super::AppError::InvalidResponse("bad json".into());
        assert_eq!(P::of(&e).message(), "Invalid response: bad json");
    }

    #[test]
    fn test_real_refused_connection() {
        // Nothing listens on a port that was just freed
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        let e = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .unwrap_err();
        assert_eq!(P::of(&e.into()), P::Unreachable);
    }
}
