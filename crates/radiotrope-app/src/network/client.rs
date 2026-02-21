//! Shared HTTP client wrapper
//!
//! Thin wrapper around `reqwest::blocking::Client` that centralizes
//! USER_AGENT and timeout configuration.

use radiotrope::config::network::{CONNECT_TIMEOUT_SECS, READ_TIMEOUT_SECS, USER_AGENT};
use crate::error::Result;
use serde::de::DeserializeOwned;
use std::time::Duration;

/// Shared HTTP client with standard configuration
pub struct HttpClient {
    inner: reqwest::blocking::Client,
}

impl HttpClient {
    /// Create a new client with default Radiotrope settings
    pub fn new() -> Result<Self> {
        let inner = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(READ_TIMEOUT_SECS))
            .build()?;
        Ok(Self { inner })
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

    /// Access the underlying reqwest client
    pub fn inner(&self) -> &reqwest::blocking::Client {
        &self.inner
    }
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
}
