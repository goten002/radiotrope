//! Minimal HTTP server for stream tests (std only, so it runs on every platform)

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// A canned response: status, extra headers and body
#[derive(Clone)]
pub struct Route {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Send a Content-Length (otherwise the body ends when the connection
    /// closes, as a live stream's does)
    pub with_length: bool,
    /// Keep the connection open after the body, sending nothing more
    pub stall: bool,
}

impl Route {
    pub fn new(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
            with_length: true,
            stall: false,
        }
    }

    /// An empty response with this status code
    pub fn status(status: u16) -> Self {
        Self {
            status,
            ..Self::new(Vec::new())
        }
    }

    /// A 302 redirect to `location`
    pub fn redirect(location: &str) -> Self {
        Self::status(302).header("Location", location)
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// No Content-Length: the body is everything until the connection closes
    pub fn without_length(mut self) -> Self {
        self.with_length = false;
        self
    }

    /// Send the body, then nothing more for a minute without closing, as a
    /// station that stopped sending does
    pub fn stall(mut self) -> Self {
        self.stall = true;
        self
    }
}

/// Serves fixed routes on 127.0.0.1 until the test process exits.
pub struct TestServer {
    pub base_url: String,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    hits: Arc<Mutex<HashMap<String, usize>>>,
}

impl TestServer {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let routes: Arc<Mutex<HashMap<String, Route>>> = Arc::default();
        let hits: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
        let routes_clone = routes.clone();
        let hits_clone = hits.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let routes = routes_clone.clone();
                let hits = hits_clone.clone();
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).is_err() {
                        return;
                    }
                    // Skip the remaining request headers
                    let mut line = String::new();
                    while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                        line.clear();
                    }
                    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
                    *hits.lock().unwrap().entry(path.to_string()).or_default() += 1;
                    let route = routes.lock().unwrap().get(path).cloned();
                    let stall = route.as_ref().is_some_and(|r| r.stall);
                    let response = match route {
                        Some(route) => {
                            let mut head =
                                format!("HTTP/1.1 {} X\r\nConnection: close\r\n", route.status);
                            if route.with_length {
                                head.push_str(&format!("Content-Length: {}\r\n", route.body.len()));
                            }
                            for (name, value) in &route.headers {
                                head.push_str(&format!("{name}: {value}\r\n"));
                            }
                            head.push_str("\r\n");
                            [head.into_bytes(), route.body].concat()
                        }
                        None => {
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                .to_vec()
                        }
                    };
                    let _ = stream.write_all(&response);
                    if stall {
                        thread::sleep(Duration::from_secs(60));
                    }
                });
            }
        });
        Self {
            base_url,
            routes,
            hits,
        }
    }

    /// How many requests `path` has had
    pub fn hits(&self, path: &str) -> usize {
        self.hits.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    pub fn route(&self, path: &str, route: Route) {
        self.routes.lock().unwrap().insert(path.to_string(), route);
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}
