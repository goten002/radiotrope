//! The MCP server end to end: JSON-RPC lines in, JSON-RPC lines out, over an
//! in-memory pipe, for both the handshake (legacy) and the stateless
//! (2026-07-28) kind of client

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver};
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};

use radiotrope_app::data::favorites::FavoritesManager;
use radiotrope_app::data::types::{url_to_id, Favorite, Station};
use radiotrope_app::providers::{Category, ProviderRegistry, SearchResults, StationProvider};

use super::tools::RadioTools;
use crate::app::state::{AppCommand, AppSnapshot};

/// A directory with two stations; searching it takes `delay`
struct FakeDirectory {
    delay: Duration,
}

impl StationProvider for FakeDirectory {
    fn name(&self) -> &'static str {
        "Fake"
    }
    fn id(&self) -> &'static str {
        "fake"
    }
    fn search(
        &self,
        query: &str,
        limit: usize,
        _: usize,
    ) -> radiotrope_app::error::Result<SearchResults> {
        std::thread::sleep(self.delay);
        let mut jazz = Station::new("Jazz FM", "http://jazz.test/stream");
        jazz.codec = Some("MP3".into());
        jazz.bitrate = Some(128);
        jazz.genres = ["smooth".to_string(), "jazz".to_string()].into();
        let news = Station::new("News 24", "http://news.test/stream");
        let stations: Vec<Station> = [jazz, news]
            .into_iter()
            .filter(|s| s.name.to_lowercase().contains(&query.to_lowercase()))
            .take(limit)
            .collect();
        Ok(SearchResults {
            total: Some(stations.len()),
            stations,
            has_more: false,
        })
    }
    fn browse_categories(&self) -> radiotrope_app::error::Result<Vec<Category>> {
        Ok(vec![])
    }
    fn browse_category(
        &self,
        _: &Category,
        _: usize,
        _: usize,
    ) -> radiotrope_app::error::Result<SearchResults> {
        Ok(SearchResults::empty())
    }
    fn get_popular(&self, _: usize) -> radiotrope_app::error::Result<Vec<Station>> {
        Ok(vec![])
    }
    fn get_station(&self, _: &str) -> radiotrope_app::error::Result<Option<Station>> {
        Ok(None)
    }
}

struct Session {
    write: WriteHalf<DuplexStream>,
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    next_id: u64,
    commands: Receiver<AppCommand>,
    state: Arc<Mutex<AppSnapshot>>,
    favorites: Arc<Mutex<FavoritesManager>>,
    _dir: TempDir,
}

/// A temporary folder, removed on drop
struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const MODERN: &str = "2026-07-28";

impl Session {
    async fn start(search_delay: Duration) -> Self {
        let (cmd_tx, commands) = unbounded();
        let state = Arc::new(Mutex::new(AppSnapshot::default()));
        let mut favs = FavoritesManager::new();
        favs.add(Favorite::new("Jazz FM", "http://jazz.test/stream"))
            .unwrap();
        let favorites = Arc::new(Mutex::new(favs));

        let dir = std::env::temp_dir().join(format!(
            "radiotrope-mcp-test-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(FakeDirectory {
            delay: search_delay,
        }));
        let tools = RadioTools::new(cmd_tx, state.clone(), favorites.clone())
            .with_test_setup(providers, dir.join("favorites.json"));

        let (client, server) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            if let Ok(service) = tools.serve(server).await {
                let _ = service.waiting().await;
            }
        });
        let (read, write) = tokio::io::split(client);
        Session {
            write,
            lines: BufReader::new(read).lines(),
            next_id: 0,
            commands,
            state,
            favorites,
            _dir: TempDir(dir),
        }
    }

    async fn send(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.write.write_all(line.as_bytes()).await.unwrap();
    }

    async fn read(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(5), self.lines.next_line())
            .await
            .expect("no reply within 5 s")
            .unwrap()
            .expect("server closed");
        serde_json::from_str(&line).unwrap()
    }

    /// Send a request and return its reply (the whole JSON-RPC message)
    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        let reply = self.read().await;
        assert_eq!(reply["id"], id, "reply to another request: {reply}");
        reply
    }

    /// Open the way a client of 2025-11-25 or older does
    async fn legacy_handshake(&mut self, version: &str) -> Value {
        let reply = self
            .request(
                "initialize",
                json!({"protocolVersion": version, "capabilities": {},
                       "clientInfo": {"name": "test", "version": "1"}}),
            )
            .await;
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        reply["result"].clone()
    }

    async fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": arguments}))
            .await["result"]
            .clone()
    }
}

fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

fn modern_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": MODERN,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1"}
    })
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn a_legacy_client_gets_its_own_version_and_no_reply_to_notifications() {
    let mut s = Session::start(Duration::ZERO).await;
    let init = s.legacy_handshake("2025-06-18").await;
    assert_eq!(init["protocolVersion"], "2025-06-18");
    assert_eq!(init["serverInfo"]["name"], "radiotrope");
    assert!(init["instructions"]
        .as_str()
        .unwrap()
        .contains("get_status"));

    // A cancel for an unknown request is a notification too: no reply, so
    // the next line read is the answer to ping
    s.send(
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                  "params": {"requestId": 99}}),
    )
    .await;
    let pong = s.request("ping", json!({})).await;
    assert!(pong.get("result").is_some());
}

#[tokio::test]
async fn a_modern_client_discovers_and_calls_without_a_handshake() {
    let mut s = Session::start(Duration::ZERO).await;
    let discover = s
        .request("server/discover", json!({"_meta": modern_meta()}))
        .await;
    let versions = discover["result"]["supportedVersions"].as_array().unwrap();
    assert!(versions.contains(&json!(MODERN)));
    assert!(versions.contains(&json!("2024-11-05")));

    let reply = s
        .request(
            "tools/call",
            json!({"name": "stop", "arguments": {}, "_meta": modern_meta()}),
        )
        .await;
    assert_eq!(reply["result"]["resultType"], "complete");
    assert!(matches!(s.commands.try_recv(), Ok(AppCommand::Stop)));
}

#[tokio::test]
async fn every_tool_has_a_title_and_behaviour_hints() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;
    let list = s.request("tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 9);
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        assert!(tool["title"].is_string(), "{name} has no title");
        assert!(tool["annotations"].is_object(), "{name} has no annotations");
    }
    let by_name = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap().clone();
    assert_eq!(by_name("get_status")["annotations"]["readOnlyHint"], true);
    assert_eq!(
        by_name("remove_favorite")["annotations"]["destructiveHint"],
        true
    );
    assert_eq!(
        by_name("search_stations")["annotations"]["openWorldHint"],
        true
    );
    assert!(by_name("get_status")["outputSchema"].is_object());
    assert!(by_name("search_stations")["outputSchema"].is_object());
}

#[tokio::test]
async fn an_unknown_tool_is_a_protocol_error() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;
    let reply = s
        .request("tools/call", json!({"name": "nope", "arguments": {}}))
        .await;
    assert_eq!(reply["error"]["code"], -32602);
}

#[tokio::test]
async fn volume_takes_numbers_as_text_but_not_nan() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;

    let result = s.call("set_volume", json!({"volume": "45"})).await;
    assert_eq!(text(&result), "Volume set to 45%");
    assert!(
        matches!(s.commands.try_recv(), Ok(AppCommand::SetVolume(v)) if (v - 0.45).abs() < 1e-6)
    );

    let result = s.call("set_volume", json!({"volume": "NaN"})).await;
    assert_eq!(result["isError"], true);
    assert!(s.commands.try_recv().is_err());
    assert!((s.state.lock().unwrap().volume - 0.45).abs() < 1e-6);
}

#[tokio::test]
async fn status_is_structured() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;
    {
        let mut st = s.state.lock().unwrap();
        st.station_name = Some("Jazz FM".into());
        st.station_url = Some("http://jazz.test/stream".into());
        st.is_resolving = true;
        st.volume = 0.3;
    }
    let result = s.call("get_status", json!({})).await;
    let status = &result["structuredContent"];
    assert_eq!(status["playback"], "resolving");
    assert_eq!(status["volume"], 30);
    assert_eq!(
        status["station"]["favorite_id"],
        url_to_id("http://jazz.test/stream")
    );
    // The same JSON as text, for clients that only read text
    let as_text: Value = serde_json::from_str(text(&result)).unwrap();
    assert_eq!(&as_text, status);
}

#[tokio::test]
async fn search_returns_details_and_marks_favorites() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;
    let result = s.call("search_stations", json!({"query": "jazz"})).await;
    let found = &result["structuredContent"];
    assert_eq!(found["count"], 1);
    let jazz = &found["stations"][0];
    assert_eq!(jazz["codec"], "MP3");
    assert_eq!(jazz["bitrate_kbps"], 128);
    assert_eq!(jazz["genres"], json!(["jazz", "smooth"]));
    assert_eq!(jazz["favorite_id"], url_to_id("http://jazz.test/stream"));

    let result = s.call("search_stations", json!({"query": " "})).await;
    assert_eq!(result["isError"], true);
}

#[tokio::test]
async fn a_slow_search_does_not_hold_up_other_calls() {
    let mut s = Session::start(Duration::from_millis(800)).await;
    s.legacy_handshake("2025-11-25").await;
    s.send(json!({"jsonrpc": "2.0", "id": 100, "method": "tools/call",
                  "params": {"name": "search_stations", "arguments": {"query": "news"}}}))
        .await;
    s.send(json!({"jsonrpc": "2.0", "id": 101, "method": "ping"}))
        .await;
    assert_eq!(
        s.read().await["id"],
        101,
        "ping must not wait for the search"
    );
    assert_eq!(s.read().await["id"], 100);
}

#[tokio::test]
async fn favorites_can_be_added_listed_played_and_removed() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;

    let added = s
        .call(
            "add_favorite",
            json!({"url": "http://news.test/stream", "name": "News 24"}),
        )
        .await;
    assert!(text(&added).contains(&url_to_id("http://news.test/stream")));

    let list = s.call("list_favorites", json!({})).await;
    let names: Vec<&str> = list["structuredContent"]["favorites"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"News 24"));

    let id = url_to_id("http://news.test/stream");
    s.call("play_favorite", json!({"id": id})).await;
    assert!(matches!(
        s.commands.try_recv(),
        Ok(AppCommand::Play { url, .. }) if url == "http://news.test/stream"
    ));

    let missing = s.call("play_favorite", json!({"id": "nope"})).await;
    assert_eq!(missing["isError"], true);

    s.call("remove_favorite", json!({"id": id})).await;
    assert!(!s
        .favorites
        .lock()
        .unwrap()
        .is_favorite("http://news.test/stream"));
    let by_url = s
        .call("remove_favorite", json!({"url": "http://jazz.test/stream"}))
        .await;
    assert_eq!(text(&by_url), "Removed \"Jazz FM\" from favorites");
    assert!(s.favorites.lock().unwrap().is_empty());
}
