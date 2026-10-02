//! Which radio-browser server to ask
//!
//! radio-browser runs its API on a changing set of servers
//! (`de1.api.radio-browser.info`, ...). The app starts from the servers in
//! [`RADIO_BROWSER_SERVERS`], learns the current list from the first server
//! that answers (`/json/servers`, kept on disk for a day), checks which
//! servers are up before picking one, and moves to another when the one in
//! use times out, can't be reached, is busy or fails.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::config::providers::{
    RADIO_BROWSER_SERVERS, RADIO_BROWSER_SERVER_LIST_TTL, RADIO_BROWSER_SERVER_PROBE_TIMEOUT,
};
use crate::error::{AppError, Result, ServiceProblem};
use crate::network::{ApiCache, HttpClient};

/// A server that failed is left alone this long, unless no other answers
const DOWN_FOR: Duration = Duration::from_secs(10 * 60);

/// Cache key of the server list learned from radio-browser
const SERVER_LIST_KEY: &str = "radio-browser servers";

/// Suffix every radio-browser API server name ends with
const SERVER_DOMAIN: &str = ".api.radio-browser.info";

#[derive(Default)]
struct State {
    /// Base URLs of the servers known, in the order they are tried
    known: Vec<String>,
    /// The server list was asked for (or found fresh on disk)
    listed: bool,
    /// The server in use
    current: Option<String>,
    /// When each server last failed
    down: HashMap<String, Instant>,
}

/// radio-browser's servers, and the one in use
pub struct Servers {
    client: reqwest::blocking::Client,
    cache: Option<ApiCache>,
    /// Learn more servers from the server list
    discover: bool,
    state: Arc<Mutex<State>>,
}

impl Servers {
    /// The servers shared by every radio-browser provider in the app
    pub fn shared() -> Result<Arc<Self>> {
        static SHARED: OnceLock<Arc<Servers>> = OnceLock::new();
        if let Some(servers) = SHARED.get() {
            return Ok(servers.clone());
        }
        let seeds = RADIO_BROWSER_SERVERS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let servers = Arc::new(Self::new(
            HttpClient::shared()?.inner().clone(),
            seeds,
            ApiCache::open_default(),
            true,
        ));
        Ok(SHARED.get_or_init(|| servers).clone())
    }

    /// Servers starting from `seeds` (base URLs). With `discover`, more are
    /// learned from the server list, which `cache` keeps between runs.
    pub fn new(
        client: reqwest::blocking::Client,
        seeds: Vec<String>,
        cache: Option<ApiCache>,
        discover: bool,
    ) -> Self {
        let mut known = seeds;
        let mut listed = !discover;
        if discover {
            if let Some(cache) = &cache {
                if let Some(body) = cache.get_any(SERVER_LIST_KEY) {
                    merge(&mut known, parse_server_list(&body));
                }
                listed = cache
                    .get_fresh(SERVER_LIST_KEY, RADIO_BROWSER_SERVER_LIST_TTL)
                    .is_some();
            }
        }
        Self {
            client,
            cache,
            discover,
            state: Arc::new(Mutex::new(State {
                known,
                listed,
                ..Default::default()
            })),
        }
    }

    /// Run `request` against the server in use. If that server times out,
    /// can't be reached, is busy, fails or answers with something other than
    /// the JSON asked for, move to another one that answers and run it
    /// again there. The error of the last server tried is returned
    /// when none of them manage.
    pub fn run<T>(&self, mut request: impl FnMut(&str) -> Result<T>) -> Result<T> {
        let mut tried: Vec<String> = Vec::new();
        let mut last = None;
        while let Some(base) = self.pick(&tried) {
            match request(&base) {
                Ok(value) => return Ok(value),
                Err(e) if worth_another_server(&e) => {
                    self.failed(&base);
                    tried.push(base);
                    last = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| AppError::Config("no radio-browser server known".into())))
    }

    /// The server in use, if any yet
    pub fn current(&self) -> Option<String> {
        self.lock().current.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The server to try next: the one in use, or else the first of the
    /// others to answer a quick check
    fn pick(&self, tried: &[String]) -> Option<String> {
        let candidates = {
            let state = self.lock();
            if let Some(current) = &state.current {
                if !tried.contains(current) {
                    return Some(current.clone());
                }
            }
            let untried: Vec<String> = state
                .known
                .iter()
                .filter(|s| !tried.contains(s))
                .cloned()
                .collect();
            let up: Vec<String> = untried
                .iter()
                .filter(|s| {
                    state
                        .down
                        .get(*s)
                        .is_none_or(|since| since.elapsed() >= DOWN_FOR)
                })
                .cloned()
                .collect();
            // Every server failed lately: give them another chance
            if up.is_empty() {
                untried
            } else {
                up
            }
        };
        let first = candidates.first()?.clone();
        let chosen = if candidates.len() == 1 {
            first
        } else {
            // None answered: still try one, so its own error is reported
            self.first_to_answer(&candidates).unwrap_or(first)
        };
        self.lock().current = Some(chosen.clone());
        self.learn_servers(&chosen);
        Some(chosen)
    }

    /// Ask every candidate at once and take the first that answers. Those
    /// that fail are marked down; slower ones are left as they are.
    fn first_to_answer(&self, candidates: &[String]) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        for base in candidates {
            let tx = tx.clone();
            let base = base.clone();
            let client = self.client.clone();
            std::thread::Builder::new()
                .name("rb-probe".into())
                .spawn(move || {
                    let ok = client
                        .get(format!("{base}/json/stats"))
                        .timeout(RADIO_BROWSER_SERVER_PROBE_TIMEOUT)
                        .send()
                        .and_then(|r| r.error_for_status())
                        .is_ok();
                    let _ = tx.send((base, ok));
                })
                .ok();
        }
        drop(tx);
        let deadline = Instant::now() + RADIO_BROWSER_SERVER_PROBE_TIMEOUT;
        while let Ok((base, ok)) =
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            if ok {
                return Some(base);
            }
            self.failed(&base);
        }
        None
    }

    /// Note that `base` failed, so the next request picks another server
    fn failed(&self, base: &str) {
        let mut state = self.lock();
        state.down.insert(base.to_string(), Instant::now());
        if state.current.as_deref() == Some(base) {
            state.current = None;
        }
    }

    /// Once a session, ask `base` for radio-browser's current server list,
    /// in the background, and remember it
    fn learn_servers(&self, base: &str) {
        {
            let mut state = self.lock();
            if state.listed || !self.discover {
                return;
            }
            state.listed = true;
        }
        let url = format!("{base}/json/servers");
        let client = self.client.clone();
        let cache = self.cache.clone();
        let state = self.state.clone();
        // The list only matters for later requests; don't hold this one up
        std::thread::Builder::new()
            .name("rb-servers".into())
            .spawn(move || {
                let Ok(body) = client
                    .get(url)
                    .timeout(RADIO_BROWSER_SERVER_PROBE_TIMEOUT)
                    .send()
                    .and_then(|r| r.error_for_status())
                    .and_then(|r| r.bytes())
                else {
                    return;
                };
                let names = parse_server_list(&body);
                if names.is_empty() {
                    return;
                }
                if let Some(cache) = &cache {
                    cache.put(SERVER_LIST_KEY, &body);
                }
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                merge(&mut state.known, names);
            })
            .ok();
    }
}

/// Whether another server might do better than the one that gave `e`
fn worth_another_server(e: &AppError) -> bool {
    // An answer that isn't the JSON asked for: a page from a proxy, or a
    // server in maintenance answering 200
    if matches!(e, AppError::InvalidResponse(_)) {
        return true;
    }
    matches!(
        ServiceProblem::of(e),
        ServiceProblem::Offline
            | ServiceProblem::Unreachable
            | ServiceProblem::TimedOut
            | ServiceProblem::Busy
            | ServiceProblem::ServerError(_)
    )
}

/// Base URLs of the servers in a `/json/servers` answer. Only radio-browser
/// API hosts are taken, whatever else the answer holds.
fn parse_server_list(body: &[u8]) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
    }
    let entries: Vec<Entry> = serde_json::from_slice(body).unwrap_or_default();
    let mut bases = Vec::new();
    for entry in entries {
        let name = entry.name.trim().trim_end_matches('.').to_ascii_lowercase();
        let valid = name.ends_with(SERVER_DOMAIN)
            && name.len() > SERVER_DOMAIN.len()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
        let base = format!("https://{name}");
        if valid && !bases.contains(&base) {
            bases.push(base);
        }
    }
    bases
}

/// Add the servers not known yet, after those known
fn merge(known: &mut Vec<String>, found: Vec<String>) {
    for base in found {
        if !known.contains(&base) {
            known.push(base);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::client::fetch_json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn client() -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    /// A server answering every request with `status` and `body`; returns
    /// its base URL and how many requests other than probes it got
    fn serve(status: u16, body: &'static str) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]);
                if !head.contains("/json/stats") {
                    counted.fetch_add(1, Ordering::Relaxed);
                }
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (base, hits)
    }

    /// A base URL where nothing listens
    fn dead() -> String {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        format!("http://127.0.0.1:{port}")
    }

    fn get(servers: &Servers, path: &str) -> Result<String> {
        servers.run(|base| {
            Ok(client()
                .get(format!("{base}{path}"))
                .send()
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.text())?)
        })
    }

    #[test]
    fn test_moves_to_a_server_that_answers() {
        let down = dead();
        let (up, hits) = serve(200, "[]");
        let servers = Servers::new(client(), vec![down.clone(), up.clone()], None, false);
        // Force the dead one first, as if it had been in use
        servers.lock().current = Some(down);
        assert_eq!(get(&servers, "/json/x").unwrap(), "[]");
        assert_eq!(servers.current(), Some(up));
        // The next request goes straight to the working server
        get(&servers, "/json/x").unwrap();
        assert_eq!(hits.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_picks_the_server_that_is_up() {
        let down = dead();
        let (up, _) = serve(200, "[]");
        let servers = Servers::new(client(), vec![down, up.clone()], None, false);
        get(&servers, "/json/x").unwrap();
        assert_eq!(servers.current(), Some(up));
    }

    #[test]
    fn test_busy_or_failing_server_is_left() {
        let (busy, _) = serve(429, "");
        let (failing, _) = serve(503, "");
        let (up, _) = serve(200, "[1]");
        let servers = Servers::new(
            client(),
            vec![busy.clone(), failing.clone(), up],
            None,
            false,
        );
        servers.lock().current = Some(busy);
        assert_eq!(get(&servers, "/json/x").unwrap(), "[1]");
    }

    #[test]
    fn test_answer_that_is_not_json_is_left() {
        let (odd, _) = serve(200, "<html>Down for maintenance</html>");
        let (up, _) = serve(200, "[1]");
        let servers = Servers::new(client(), vec![odd.clone(), up.clone()], None, false);
        servers.lock().current = Some(odd);
        let (answer, _) = servers
            .run(|base| fetch_json::<Vec<u32>>(client().get(format!("{base}/json/x"))))
            .unwrap();
        assert_eq!(answer, vec![1]);
        assert_eq!(servers.current(), Some(up));
    }

    #[test]
    fn test_refused_request_is_not_repeated_elsewhere() {
        let (not_found, _) = serve(404, "");
        let (up, hits) = serve(200, "[]");
        let servers = Servers::new(client(), vec![not_found.clone(), up], None, false);
        servers.lock().current = Some(not_found);
        assert!(get(&servers, "/json/x").is_err());
        assert_eq!(hits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_all_down_reports_the_error() {
        let servers = Servers::new(client(), vec![dead(), dead()], None, false);
        let e = get(&servers, "/json/x").unwrap_err();
        assert_eq!(ServiceProblem::of(&e), ServiceProblem::Unreachable);
        // Failed servers get another chance on the next request
        assert!(servers.pick(&[]).is_some());
    }

    #[test]
    fn test_parse_server_list_keeps_radio_browser_hosts_only() {
        let body = br#"[
            {"ip": "1.2.3.4", "name": "de1.api.radio-browser.info"},
            {"ip": "::1", "name": "de1.api.radio-browser.info"},
            {"ip": "5.6.7.8", "name": "FI1.api.radio-browser.info."},
            {"ip": "6.6.6.6", "name": "evil.example.com"},
            {"ip": "6.6.6.7", "name": "x/y.api.radio-browser.info"},
            {"ip": "6.6.6.8", "name": ".api.radio-browser.info"}
        ]"#;
        assert_eq!(
            parse_server_list(body),
            [
                "https://de1.api.radio-browser.info",
                "https://fi1.api.radio-browser.info"
            ]
        );
        assert!(parse_server_list(b"not json").is_empty());
    }

    #[test]
    fn test_learns_servers_from_the_list() {
        let dir =
            std::env::temp_dir().join(format!("radiotrope-rb-servers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = ApiCache::with_dir(dir.clone()).unwrap();
        let (up, _) = serve(200, r#"[{"name": "nl1.api.radio-browser.info"}]"#);
        let servers = Servers::new(client(), vec![up.clone()], Some(cache.clone()), true);
        servers.pick(&[]);
        let nl1 = "https://nl1.api.radio-browser.info".to_string();
        // The list is fetched in the background
        for _ in 0..50 {
            if servers.lock().known.contains(&nl1) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(servers.lock().known.contains(&nl1));
        // Kept for the next run, after the configured servers
        let next = Servers::new(client(), vec![up.clone()], Some(cache), true);
        assert_eq!(next.lock().known, [up, nl1]);
        assert!(next.lock().listed);
        let _ = std::fs::remove_dir_all(dir);
    }
}
