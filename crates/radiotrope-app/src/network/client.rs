//! Shared HTTP client wrapper
//!
//! Thin wrapper around `reqwest::blocking::Client` that centralizes
//! USER_AGENT and timeout configuration.

use super::api_cache::ApiCache;
use crate::error::{AppError, Result};
use radiotrope::config::network::{CONNECT_TIMEOUT_SECS, READ_TIMEOUT_SECS, USER_AGENT};
use serde::de::DeserializeOwned;
use std::sync::OnceLock;
use std::time::Duration;

/// Shared HTTP client with standard configuration
pub struct HttpClient {
    inner: reqwest::blocking::Client,
    cache: Option<ApiCache>,
}

impl HttpClient {
    /// Create a new client with default Radiotrope settings
    pub fn new() -> Result<Self> {
        let inner = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(READ_TIMEOUT_SECS))
            .build()?;
        Ok(Self { inner, cache: None })
    }

    /// A client sharing one connection pool with every other `shared()` client
    ///
    /// Reusing the pool keeps connections (and their TLS sessions) open between
    /// requests, instead of a new handshake for every search.
    pub fn shared() -> Result<Self> {
        static SHARED: OnceLock<reqwest::blocking::Client> = OnceLock::new();
        if let Some(inner) = SHARED.get() {
            return Ok(Self {
                inner: inner.clone(),
                cache: None,
            });
        }
        let client = Self::new()?;
        let inner = SHARED.get_or_init(|| client.inner).clone();
        Ok(Self { inner, cache: None })
    }

    /// Use `cache` for the `*_cached` requests
    pub fn with_cache(mut self, cache: Option<ApiCache>) -> Self {
        self.cache = cache;
        self
    }

    /// GET a URL and deserialize the JSON response
    pub fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self.inner.get(url).send()?;
        let data = resp.json::<T>()?;
        Ok(data)
    }

    /// POST form-encoded data and deserialize the JSON response
    pub fn post_form_json<T: DeserializeOwned>(
        &self,
        url: &str,
        params: &[(&str, &str)],
    ) -> Result<T> {
        let resp = self.inner.post(url).form(params).send()?;
        let data = resp.json::<T>()?;
        Ok(data)
    }

    /// Like [`get_json`](Self::get_json), but answered from the cache while
    /// the stored response is younger than `ttl`
    pub fn get_json_cached<T: DeserializeOwned>(&self, url: &str, ttl: Duration) -> Result<T> {
        self.cached(&format!("GET {url}"), ttl, || self.inner.get(url))
    }

    /// Like [`post_form_json`](Self::post_form_json), but answered from the
    /// cache while the stored response is younger than `ttl`
    pub fn post_form_json_cached<T: DeserializeOwned>(
        &self,
        url: &str,
        params: &[(&str, &str)],
        ttl: Duration,
    ) -> Result<T> {
        let body = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        self.cached(&format!("POST {url} {body}"), ttl, || {
            self.inner.post(url).form(params)
        })
    }

    /// Serve `key` from the cache if fresh; otherwise send the request and
    /// store the response. If the request fails, fall back to a stale entry.
    fn cached<T: DeserializeOwned>(
        &self,
        key: &str,
        ttl: Duration,
        request: impl FnOnce() -> reqwest::blocking::RequestBuilder,
    ) -> Result<T> {
        self.get_or_fetch(key, ttl, || body_of(request()))
    }

    /// Serve `key` from the cache if fresh; otherwise `fetch` the body and
    /// store it. If fetching fails, fall back to a stale entry.
    pub fn get_or_fetch<T: DeserializeOwned>(
        &self,
        key: &str,
        ttl: Duration,
        fetch: impl FnOnce() -> Result<Vec<u8>>,
    ) -> Result<T> {
        let Some(cache) = &self.cache else {
            return parse(&fetch()?);
        };
        if let Some(data) = cache.get_fresh(key, ttl).and_then(|b| parse(&b).ok()) {
            return Ok(data);
        }
        match fetch() {
            Ok(bytes) => {
                let data = parse(&bytes)?;
                cache.put(key, &bytes);
                Ok(data)
            }
            Err(e) => cache.get_any(key).and_then(|b| parse(&b).ok()).ok_or(e),
        }
    }

    /// Access the underlying reqwest client
    pub fn inner(&self) -> &reqwest::blocking::Client {
        &self.inner
    }
}

/// Send `request` and return the body of a successful response; an HTTP
/// error status is an error
pub fn body_of(request: reqwest::blocking::RequestBuilder) -> Result<Vec<u8>> {
    Ok(request
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.bytes())?
        .to_vec())
}

fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|e| AppError::InvalidResponse(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let client = HttpClient::new();
        assert!(client.is_ok());
    }

    #[test]
    fn test_client_inner_access() {
        let client = HttpClient::new().unwrap();
        let _inner = client.inner();
    }

    #[test]
    fn test_get_json_invalid_url() {
        let client = HttpClient::new().unwrap();
        let result: Result<serde_json::Value> = client.get_json("http://invalid.invalid.invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_post_form_json_invalid_url() {
        let client = HttpClient::new().unwrap();
        let result: Result<serde_json::Value> =
            client.post_form_json("http://invalid.invalid.invalid", &[("key", "value")]);
        assert!(result.is_err());
    }

    /// Serve `body` once on a local port, returning the URL
    fn serve_once(body: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/json", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
            }
        });
        url
    }

    fn temp_cache(name: &str) -> ApiCache {
        let dir = std::env::temp_dir().join(format!(
            "radiotrope-client-cache-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        ApiCache::with_dir(dir).unwrap()
    }

    #[test]
    fn test_cached_get_is_served_from_cache() {
        let cache = temp_cache("hit");
        let client = HttpClient::new().unwrap().with_cache(Some(cache.clone()));
        // The server answers only once; the second call must hit the cache
        let url = serve_once("[1,2,3]");
        let ttl = Duration::from_secs(60);
        let first: Vec<u32> = client.get_json_cached(&url, ttl).unwrap();
        let second: Vec<u32> = client.get_json_cached(&url, ttl).unwrap();
        assert_eq!(first, vec![1, 2, 3]);
        assert_eq!(second, first);
        let _ = std::fs::remove_dir_all(cache.dir());
    }

    #[test]
    fn test_stale_entry_used_when_offline() {
        let cache = temp_cache("stale");
        let client = HttpClient::new().unwrap().with_cache(Some(cache.clone()));
        let url = serve_once("[7]");
        let first: Vec<u32> = client.get_json_cached(&url, Duration::ZERO).unwrap();
        // The server is gone and the entry expired, so the stale copy is used
        let second: Vec<u32> = client.get_json_cached(&url, Duration::ZERO).unwrap();
        assert_eq!(first, vec![7]);
        assert_eq!(second, vec![7]);
        let _ = std::fs::remove_dir_all(cache.dir());
    }

    #[test]
    fn test_cached_post_keys_include_params() {
        let cache = temp_cache("post");
        let client = HttpClient::new().unwrap().with_cache(Some(cache.clone()));
        let url = serve_once("[1]");
        let ttl = Duration::from_secs(60);
        let a: Vec<u32> = client
            .post_form_json_cached(&url, &[("name", "jazz")], ttl)
            .unwrap();
        assert_eq!(a, vec![1]);
        // Different params: not in the cache, and the server is gone
        let b: Result<Vec<u32>> = client.post_form_json_cached(&url, &[("name", "rock")], ttl);
        assert!(b.is_err());
        let _ = std::fs::remove_dir_all(cache.dir());
    }
}
