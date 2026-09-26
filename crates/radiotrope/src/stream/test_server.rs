//! Minimal HTTP server for stream tests (std only, so it runs on every platform)

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

/// A canned response: extra headers and body
#[derive(Clone)]
pub struct Route {
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Route {
    pub fn new(body: impl Into<Vec<u8>>) -> Self {
        Self {
            headers: Vec::new(),
            body: body.into(),
        }
    }
}

/// Serves fixed routes on 127.0.0.1 until the test process exits.
pub struct TestServer {
    pub base_url: String,
    routes: Arc<Mutex<HashMap<String, Route>>>,
}

impl TestServer {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let routes: Arc<Mutex<HashMap<String, Route>>> = Arc::default();
        let routes_clone = routes.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let routes = routes_clone.clone();
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
                    let route = routes.lock().unwrap().get(path).cloned();
                    let response = match route {
                        Some(route) => {
                            let mut head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n",
                                route.body.len()
                            );
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
                });
            }
        });
        Self { base_url, routes }
    }

    pub fn route(&self, path: &str, route: Route) {
        self.routes.lock().unwrap().insert(path.to_string(), route);
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}
