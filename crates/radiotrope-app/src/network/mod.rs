//! Network operations
//!
//! HTTP client and utilities.

pub mod api_cache;
pub mod browse_logos;
pub mod client;
pub mod failed_logos;
pub mod logo;

// Re-export commonly used types
pub use api_cache::ApiCache;
pub use client::HttpClient;
pub use logo::LogoService;

/// A local HTTP server for tests
#[cfg(test)]
pub(crate) mod test_http {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Answer every request with `response` (status line, headers and
    /// body) after `delay`, then close. Returns the server's base URL.
    pub fn serve(response: Vec<u8>, delay: Duration) -> String {
        serve_counted(response, delay).0
    }

    /// [`serve`], also counting the connections made to it
    pub fn serve_counted(response: Vec<u8>, delay: Duration) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = connections.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                counter.fetch_add(1, Ordering::SeqCst);
                let response = response.clone();
                std::thread::spawn(move || {
                    let mut request = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    while request.read_line(&mut line).is_ok_and(|n| n > 2) {
                        line.clear();
                    }
                    std::thread::sleep(delay);
                    let _ = stream.write_all(&response);
                });
            }
        });
        (url, connections)
    }

    /// A 200 response carrying `body`, with its length given or not
    pub fn ok(body: &[u8], with_length: bool) -> Vec<u8> {
        let length = if with_length {
            format!("Content-Length: {}\r\n", body.len())
        } else {
            String::new()
        };
        let mut response =
            format!("HTTP/1.1 200 OK\r\n{length}Connection: close\r\n\r\n").into_bytes();
        response.extend_from_slice(body);
        response
    }
}
