//! The MCP server end to end: JSON-RPC lines in, JSON-RPC lines out, over an
//! in-memory pipe, for both the handshake (legacy) and the stateless
//! (2026-07-28) kind of client

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};

use radiotrope_app::data::favorites::FavoritesManager;
use radiotrope_app::data::types::{url_to_id, Favorite, Station};
use radiotrope_app::providers::{
    Category, CategoryType, ProviderRegistry, RadiotropeProvider, SearchResults, StationProvider,
};

use super::tools::RadioTools;
use crate::app::state::{AppCommand, AppSnapshot};

/// A directory with two stations; searching it takes `delay`, and it
/// counts the plays reported to it
struct FakeDirectory {
    delay: Duration,
    plays: Arc<AtomicUsize>,
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
        Ok(vec![
            Category::new("jazz", "jazz", CategoryType::Genre).with_station_count(900),
            Category::new("pop", "pop", CategoryType::Genre).with_station_count(5000),
            Category::new("Greece", "Greece", CategoryType::Country)
                .with_station_count(700)
                .with_code(Some("GR".into())),
            Category::new("FR", "France", CategoryType::Country)
                .with_station_count(900)
                .with_code(Some("FR".into())),
            Category::new("ell", "Greek", CategoryType::Language).with_station_count(600),
            Category::new("24", "Jazz music", CategoryType::ProgrammeType).with_station_count(18),
            Category::new("1", "News", CategoryType::ProgrammeType).with_station_count(40),
        ])
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
    fn get_station(&self, id: &str) -> radiotrope_app::error::Result<Option<Station>> {
        Ok((id == "jazz-id").then(|| {
            Station::new("Jazz FM", "http://jazz.test/stream")
                .with_provider("fake", Some("jazz-id".to_string()))
                .with_logo("http://jazz.test/logo.png")
                .with_metadata(
                    Some("Greece, Attica".into()),
                    Some("Greek".into()),
                    ["Jazz".to_string()].into(),
                )
        }))
    }
    fn report_click(&self, _: &Station) -> radiotrope_app::error::Result<()> {
        self.plays.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Session {
    write: WriteHalf<DuplexStream>,
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    next_id: u64,
    commands: Receiver<AppCommand>,
    /// The window's end of the command channel
    window: Sender<AppCommand>,
    state: Arc<Mutex<AppSnapshot>>,
    favorites: Arc<Mutex<FavoritesManager>>,
    /// Plays reported to the fake directory
    plays: Arc<AtomicUsize>,
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
        Self::with_channel(search_delay, unbounded()).await
    }

    async fn with_channel(
        search_delay: Duration,
        channel: (Sender<AppCommand>, Receiver<AppCommand>),
    ) -> Self {
        let plays = Arc::new(AtomicUsize::new(0));
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(FakeDirectory {
            delay: search_delay,
            plays: plays.clone(),
        }));
        Self::with_providers(providers, plays, channel).await
    }

    /// Over the Radiotrope directory's own client, asking the directory
    /// at `base` (see [`serve_directory`])
    async fn with_directory(base: String) -> Self {
        // A blocking HTTP client can't be built on the async thread
        let provider =
            tokio::task::spawn_blocking(move || RadiotropeProvider::with_base_url(base).unwrap())
                .await
                .unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(provider));
        Self::with_providers(providers, Arc::default(), unbounded()).await
    }

    async fn with_providers(
        providers: ProviderRegistry,
        plays: Arc<AtomicUsize>,
        (cmd_tx, commands): (Sender<AppCommand>, Receiver<AppCommand>),
    ) -> Self {
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
        let tools = RadioTools::new(cmd_tx.clone(), state.clone(), favorites.clone())
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
            window: cmd_tx,
            state,
            favorites,
            plays,
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
    assert_eq!(tools.len(), 26);
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

    // Nothing given lists the most popular stations
    let result = s.call("search_stations", json!({"query": " "})).await;
    assert_eq!(result["structuredContent"]["count"], 2);
}

#[tokio::test]
async fn search_filters_combine() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;
    let count = |r: &Value| r["structuredContent"]["count"].as_u64().unwrap();

    let r = s.call("search_stations", json!({"genre": "jazz"})).await;
    assert_eq!(count(&r), 1);
    let r = s
        .call(
            "search_stations",
            json!({"codec": "mp3", "min_bitrate": "128"}),
        )
        .await;
    assert_eq!(count(&r), 1);
    let r = s
        .call(
            "search_stations",
            json!({"genre": "jazz", "min_bitrate": 192}),
        )
        .await;
    assert_eq!(count(&r), 0);
    let r = s.call("search_stations", json!({"sort": "sideways"})).await;
    assert_eq!(r["isError"], true, "{r}");
}

#[tokio::test]
async fn genres_countries_languages_and_programme_types_are_listed_largest_first() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;
    let r = s.call("list_genres", json!({})).await;
    assert_eq!(
        r["structuredContent"]["genres"],
        json!([{"name": "pop", "slug": "pop", "stations": 5000},
               {"name": "jazz", "slug": "jazz", "stations": 900}])
    );
    // The same as text, for clients that only read text
    let as_text: Value = serde_json::from_str(text(&r)).unwrap();
    assert_eq!(as_text, r["structuredContent"]);

    let r = s.call("list_countries", json!({"limit": "1"})).await;
    assert_eq!(
        r["structuredContent"]["countries"],
        json!([{"name": "France", "code": "FR", "stations": 900}])
    );
    // By a piece of the name, or by the code
    for filter in ["gree", "gr"] {
        let r = s.call("list_countries", json!({"filter": filter})).await;
        assert_eq!(
            r["structuredContent"]["countries"],
            json!([{"name": "Greece", "code": "GR", "stations": 700}]),
            "{filter}"
        );
    }

    let r = s.call("list_languages", json!({"filter": "ELL"})).await;
    assert_eq!(
        r["structuredContent"]["languages"],
        json!([{"name": "Greek", "code": "ell", "stations": 600}])
    );

    let r = s.call("list_categories", json!({})).await;
    assert_eq!(
        r["structuredContent"]["categories"],
        json!([{"name": "News", "code": "1", "stations": 40},
               {"name": "Jazz music", "code": "24", "stations": 18}])
    );
}

/// A directory on a local port, as the Radiotrope API answers: `answer`
/// gets each request's path and query and returns (status, body). Returns
/// the base URL and the requests seen.
fn serve_directory(
    answer: impl Fn(&str) -> (u16, String) + Send + Sync + 'static,
) -> (String, Arc<Mutex<Vec<String>>>) {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            // Skip the headers
            let mut header = String::new();
            while reader.read_line(&mut header).unwrap_or(0) > 2 {
                header.clear();
            }
            let mut parts = line.split(' ');
            let method = parts.next().unwrap_or("");
            let target = parts.next().unwrap_or("").to_string();
            log.lock().unwrap().push(format!("{method} {target}"));
            let (status, body) = answer(&target);
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (base, seen)
}

const KISS_ID: &str = "st_0000000000001";

/// The directory's facets, one station (Kiss, in Greece), and nothing for
/// a search with `q=nothing`
fn small_directory(target: &str) -> (u16, String) {
    let kiss = format!(
        r#"{{"id":"{KISS_ID}","name":"Kiss","country":"GR","region":"Attica",
           "languages":["ell","eng"],"homepage":"https://kiss.example.gr",
           "category":{{"code":10,"name":"Pop music"}},
           "genres":[{{"slug":"pop","name":"Pop"}},{{"slug":"drum-and-bass","name":"Drum and Bass"}}],
           "logo":null,"streams":[{{"url":"https://stream.example.gr/kiss","codec":"aac",
           "bitrate_kbps":128,"hls":false,"status":"online"}}]}}"#
    );
    let path = target.split('?').next().unwrap_or(target);
    let body = match path {
        "/v1/genres" => r#"{"items":[{"slug":"pop","name":"Pop","stations":41},
            {"slug":"drum-and-bass","name":"Drum and Bass","stations":3}],"snapshot":1}"#
            .to_string(),
        "/v1/countries" => r#"{"items":[{"code":"GR","stations":312}],"snapshot":1}"#.to_string(),
        "/v1/languages" => r#"{"items":[{"code":"ell","stations":300}],"snapshot":1}"#.to_string(),
        "/v1/categories" => {
            r#"{"items":[{"code":10,"name":"Pop music","stations":18}],"snapshot":1}"#.to_string()
        }
        "/v1/stations" if target.contains("q=nothing") => {
            r#"{"stations":[],"total":0,"next_cursor":null,"snapshot":1}"#.to_string()
        }
        "/v1/stations" => {
            format!(r#"{{"stations":[{kiss}],"total":7,"next_cursor":"next","snapshot":1}}"#)
        }
        "/v1/stations/st_0000000000001" => kiss,
        "/v1/stations/st_0000000000001/play" => return (204, String::new()),
        _ => {
            return (
                404,
                r#"{"error":{"code":"not_found","message":"No station"}}"#.into(),
            )
        }
    };
    (200, body)
}

#[tokio::test]
async fn search_filters_reach_the_directory_by_name_or_code() {
    let (base, seen) = serve_directory(small_directory);
    let mut s = Session::with_directory(base).await;
    s.legacy_handshake("2025-11-25").await;

    // Names, as a person would say them
    let r = s
        .call(
            "search_stations",
            json!({"query": "kiss", "country": "Greece", "genre": "Drum and Bass, pop",
                   "language": "Greek", "category": "pop music", "sort": "popular",
                   "limit": 5}),
        )
        .await;
    assert_ne!(r["isError"], true, "{r}");
    let asked = seen.lock().unwrap().last().unwrap().clone();
    for part in [
        "q=kiss",
        "country=GR",
        "language=ell",
        "genre=drum-and-bass&genre=pop",
        "category=10",
        "sort=popularity",
        "limit=5",
    ] {
        assert!(asked.contains(part), "{part} not in {asked}");
    }

    // The station comes back with names, and what follow-up calls take
    let found = &r["structuredContent"];
    assert_eq!(found["count"], 1);
    assert_eq!(found["total"], 7);
    assert_eq!(found["has_more"], true);
    assert!(found["note"].is_null());
    let kiss = &found["stations"][0];
    assert_eq!(kiss["id"], KISS_ID);
    assert_eq!(kiss["name"], "Kiss");
    assert_eq!(kiss["country"], "Greece, Attica");
    assert_eq!(kiss["country_code"], "GR");
    assert_eq!(kiss["language"], "Greek, English");
    assert_eq!(kiss["genres"], json!(["Drum and Bass", "Pop"]));
    assert_eq!(kiss["codec"], "AAC");

    // Codes and slugs, and the code as a number; `order` is the old name
    // of `sort`
    s.call(
        "search_stations",
        json!({"country": "gr", "genre": "pop", "language": "ell", "category": 10,
               "order": "name"}),
    )
    .await;
    let asked = seen.lock().unwrap().last().unwrap().clone();
    for part in [
        "country=GR",
        "language=ell",
        "genre=pop",
        "category=10",
        "sort=name",
    ] {
        assert!(asked.contains(part), "{part} not in {asked}");
    }

    // The best match first is the directory's own order for a query
    s.call(
        "search_stations",
        json!({"query": "kiss", "sort": "relevance"}),
    )
    .await;
    let asked = seen.lock().unwrap().last().unwrap().clone();
    assert!(!asked.contains("sort="), "{asked}");

    // Codec and bitrate are checked page by page: no total then
    let r = s.call("search_stations", json!({"codec": "aac"})).await;
    assert_eq!(r["structuredContent"]["count"], 1);
    assert!(r["structuredContent"]["total"].is_null());
}

#[tokio::test]
async fn a_search_that_finds_nothing_says_where_the_filter_values_are() {
    let (base, seen) = serve_directory(small_directory);
    let mut s = Session::with_directory(base).await;
    s.legacy_handshake("2025-11-25").await;

    let r = s.call("search_stations", json!({"query": "nothing"})).await;
    let found = &r["structuredContent"];
    assert_eq!(found["count"], 0);
    let note = found["note"].as_str().unwrap();
    assert!(
        note.contains("list_genres") && !note.contains("  "),
        "{note}"
    );

    // A country, language or programme type the directory doesn't have
    // finds nothing, without asking for stations
    let before = seen.lock().unwrap().len();
    for filter in [
        json!({"country": "Atlantis"}),
        json!({"language": "Elvish"}),
        json!({"category": "Polka music"}),
        json!({"category": 99}),
    ] {
        let r = s.call("search_stations", filter.clone()).await;
        assert_eq!(r["structuredContent"]["count"], 0, "{filter}");
        assert!(r["structuredContent"]["note"].is_string(), "{filter}");
    }
    assert!(
        seen.lock().unwrap()[before..]
            .iter()
            .all(|asked| !asked.contains("/v1/stations")),
        "no station list was asked for"
    );
}

#[tokio::test]
async fn the_directory_lists_come_with_names_codes_and_counts() {
    let (base, _) = serve_directory(small_directory);
    let mut s = Session::with_directory(base).await;
    s.legacy_handshake("2025-11-25").await;
    for (tool, key, first) in [
        (
            "list_genres",
            "genres",
            json!({"name": "Pop", "slug": "pop", "stations": 41}),
        ),
        (
            "list_countries",
            "countries",
            json!({"name": "Greece", "code": "GR", "stations": 312}),
        ),
        (
            "list_languages",
            "languages",
            json!({"name": "Greek", "code": "ell", "stations": 300}),
        ),
        (
            "list_categories",
            "categories",
            json!({"name": "Pop music", "code": "10", "stations": 18}),
        ),
    ] {
        let r = s.call(tool, json!({})).await;
        assert_eq!(r["structuredContent"][key][0], first, "{tool}: {r}");
    }
    // A genre is found by its slug too
    let r = s
        .call("list_genres", json!({"filter": "drum-and-bass"}))
        .await;
    assert_eq!(r["structuredContent"]["genres"][0]["name"], "Drum and Bass");
}

#[tokio::test]
async fn a_station_is_looked_up_by_id_without_playing_it() {
    let (base, seen) = serve_directory(small_directory);
    let mut s = Session::with_directory(base).await;
    s.legacy_handshake("2025-11-25").await;
    s.favorites
        .lock()
        .unwrap()
        .add(Favorite::new("Kiss", "https://stream.example.gr/kiss"))
        .unwrap();

    let r = s.call("get_station", json!({"id": KISS_ID})).await;
    let kiss = &r["structuredContent"];
    assert_eq!(kiss["name"], "Kiss");
    assert_eq!(kiss["url"], "https://stream.example.gr/kiss");
    assert_eq!(kiss["country_code"], "GR");
    assert_eq!(kiss["bitrate_kbps"], 128);
    assert_eq!(
        kiss["favorite_id"],
        url_to_id("https://stream.example.gr/kiss")
    );
    assert!(s.commands.try_recv().is_err(), "nothing was played");
    assert!(
        seen.lock().unwrap().iter().all(|r| r.starts_with("GET ")),
        "and no play was reported"
    );

    for id in ["st_000000000000z", "not an id", " "] {
        let r = s.call("get_station", json!({"id": id})).await;
        assert_eq!(r["isError"], true, "{id}: {r}");
    }
    let r = s
        .call("get_station", json!({"id": "st_000000000000z"}))
        .await;
    assert!(text(&r).contains("search_stations"), "{r}");
}

#[tokio::test]
async fn an_agents_play_is_counted_only_if_the_user_allowed_it() {
    let _switch = crate::control::REPORT_PLAYS_TEST.lock().await;
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;

    // As the window starts: off until the user allows it
    crate::view_options::set_report_plays(false);
    let r = s
        .call("play_station", json!({"id": "jazz-id", "wait_seconds": 0}))
        .await;
    assert_ne!(r["isError"], true, "{r}");
    assert!(matches!(s.commands.try_recv(), Ok(AppCommand::Play { .. })));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        s.plays.load(Ordering::SeqCst),
        0,
        "no report without consent"
    );
    assert!(
        crate::app::controller::play_report::play_to_report(
            "http://jazz.test/stream",
            crate::view_options::report_plays(),
            &s.favorites.lock().unwrap(),
        )
        .is_none(),
        "nor once it plays"
    );
    let settings = s.call("get_settings", json!({})).await;
    assert_eq!(
        settings["structuredContent"]["report_play_count"]["allowed"],
        false
    );

    // No tool takes it as an argument, and one slipped in changes nothing
    let list = s.request("tools/list", json!({})).await;
    for tool in list["result"]["tools"].as_array().unwrap() {
        let input = tool["inputSchema"].to_string().to_lowercase();
        assert!(
            !input.contains("report") && !input.contains("consent"),
            "{} takes it",
            tool["name"]
        );
    }
    for (tool, mut args) in [
        ("set_equalizer", json!({"enabled": false})),
        ("get_settings", json!({})),
        ("play_station", json!({"id": "jazz-id", "wait_seconds": 0})),
    ] {
        args["report_plays"] = json!(true);
        args["allowed"] = json!(true);
        s.call(tool, args).await;
    }
    assert!(
        !crate::view_options::report_plays(),
        "still the user's choice"
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(s.plays.load(Ordering::SeqCst), 0);

    // The user allows it in the window. The play is counted by the
    // controller once the station plays (app::controller::play_report,
    // whose tests count it once), not by the tool as well
    crate::view_options::set_report_plays(true);
    s.call("play_station", json!({"id": "jazz-id", "wait_seconds": 0}))
        .await;
    let settings = s.call("get_settings", json!({})).await;
    assert_eq!(
        settings["structuredContent"]["report_play_count"]["allowed"],
        true
    );
    // Looking a station up is not a play
    s.call("get_station", json!({"id": "jazz-id"})).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        s.plays.load(Ordering::SeqCst),
        0,
        "the controller counts it"
    );

    crate::view_options::set_report_plays(false);
}

#[tokio::test]
async fn favorites_are_found_by_text_and_put_in_a_new_order() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;
    {
        let mut favs = s.favorites.lock().unwrap();
        favs.add(Favorite::from_station(
            Station::new("News 24", "http://news.test/stream").with_metadata(
                Some("Greece".into()),
                None,
                Default::default(),
            ),
        ))
        .unwrap();
        favs.add(Favorite::from_station(
            Station::new("Rock FM", "http://rock.test/stream").with_metadata(
                None,
                None,
                ["Classic Rock".to_string()].into(),
            ),
        ))
        .unwrap();
    }
    let names = |r: &Value| -> Vec<String> {
        r["structuredContent"]["favorites"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap().to_string())
            .collect()
    };
    let all = s.call("list_favorites", json!({})).await;
    assert_eq!(names(&all), ["Jazz FM", "News 24", "Rock FM"]);
    // By name, country or genre
    for (query, found) in [
        ("JAZZ", "Jazz FM"),
        ("greece", "News 24"),
        ("classic", "Rock FM"),
    ] {
        let r = s.call("list_favorites", json!({"query": query})).await;
        assert_eq!(names(&r), [found], "{query}");
    }
    let none = s.call("list_favorites", json!({"query": "polka"})).await;
    assert!(names(&none).is_empty());

    // The ones given come first; the rest keep their order
    let rock = url_to_id("http://rock.test/stream");
    let r = s.call("reorder_favorites", json!({"ids": [rock]})).await;
    assert_eq!(text(&r), "Moved 1 favorite to the top");
    let all = s.call("list_favorites", json!({})).await;
    assert_eq!(names(&all), ["Rock FM", "Jazz FM", "News 24"]);
    let news = url_to_id("http://news.test/stream");
    let jazz = url_to_id("http://jazz.test/stream");
    s.call("reorder_favorites", json!({"ids": [news, jazz, rock]}))
        .await;
    let all = s.call("list_favorites", json!({})).await;
    assert_eq!(names(&all), ["News 24", "Jazz FM", "Rock FM"]);

    // An unknown id changes nothing and says where ids come from
    let r = s
        .call("reorder_favorites", json!({"ids": [rock, "nope"]}))
        .await;
    assert_eq!(r["isError"], true, "{r}");
    assert!(text(&r).contains("list_favorites"), "{r}");
    let r = s.call("reorder_favorites", json!({"ids": []})).await;
    assert_eq!(r["isError"], true, "{r}");
    let all = s.call("list_favorites", json!({})).await;
    assert_eq!(names(&all), ["News 24", "Jazz FM", "Rock FM"]);
}

#[tokio::test]
async fn the_equalizer_is_switched_and_given_a_preset() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;
    let presets = radiotrope::audio::PRESETS;
    let preset = presets[1].name;

    let r = s
        .call(
            "set_equalizer",
            json!({"enabled": true, "preset": preset.to_lowercase()}),
        )
        .await;
    assert_eq!(text(&r), format!("Equalizer on, preset {preset}"));
    assert!(matches!(
        s.commands.try_recv(),
        Ok(AppCommand::SetEqPreset(name)) if name == preset
    ));
    assert!(matches!(
        s.commands.try_recv(),
        Ok(AppCommand::SetEqEnabled(true))
    ));

    // A preset alone leaves the switch as it is, and says so when off
    let r = s.call("set_equalizer", json!({"preset": preset})).await;
    assert!(text(&r).contains("the equalizer is off"), "{r}");
    assert!(matches!(
        s.commands.try_recv(),
        Ok(AppCommand::SetEqPreset(_))
    ));
    assert!(s.commands.try_recv().is_err());

    let r = s.call("set_equalizer", json!({"enabled": false})).await;
    assert_eq!(text(&r), "Equalizer off");
    assert!(matches!(
        s.commands.try_recv(),
        Ok(AppCommand::SetEqEnabled(false))
    ));

    // An unknown preset names the ones there are; nothing given is refused
    let r = s
        .call("set_equalizer", json!({"preset": "No Such Sound"}))
        .await;
    assert_eq!(r["isError"], true, "{r}");
    assert!(text(&r).contains(presets[0].name), "{r}");
    let r = s.call("set_equalizer", json!({})).await;
    assert_eq!(r["isError"], true, "{r}");
    assert!(s.commands.try_recv().is_err());
}

#[tokio::test]
async fn settings_show_the_equalizer_and_the_recording_settings() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;
    let preset = radiotrope::audio::PRESETS[1].name;
    {
        let mut st = s.state.lock().unwrap();
        st.eq_enabled = true;
        st.eq_preset_name = Some(preset.into());
        st.recording_setup = crate::app::state::RecordingSetup {
            dir: Some(std::env::temp_dir().join("rt-settings-test")),
            format: radiotrope::audio::RecordingFormat::Opus,
            bitrate: Some(96),
            with_eq: true,
        };
    }
    let r = s.call("get_settings", json!({})).await;
    let settings = &r["structuredContent"];
    assert_eq!(settings["equalizer"]["enabled"], true);
    assert_eq!(settings["equalizer"]["preset"], preset);
    assert_eq!(
        settings["equalizer"]["presets"].as_array().unwrap().len(),
        radiotrope::audio::PRESETS.len()
    );
    assert_eq!(
        settings["recording"],
        json!({
            "folder": std::env::temp_dir().join("rt-settings-test").display().to_string(),
            "format": "opus",
            "bitrate_kbps": 96,
            "with_equalizer": true,
        })
    );
    let note = settings["report_play_count"]["note"].as_str().unwrap();
    assert!(note.contains("only the user"), "{note}");
    assert!(!note.contains("  "), "{note}");
    // Reading changes nothing
    assert!(s.commands.try_recv().is_err());
    let by_name = s.request("tools/list", json!({})).await["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "get_settings")
        .unwrap()
        .clone();
    assert_eq!(by_name["annotations"]["readOnlyHint"], true);
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
    s.call("play_favorite", json!({"id": id, "wait_seconds": 0}))
        .await;
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

/// Plays like the controller: resolving, then playing a moment later, or
/// failing for a URL with "broken" in it
fn fake_controller(commands: Receiver<AppCommand>, state: Arc<Mutex<AppSnapshot>>) {
    std::thread::spawn(move || {
        while let Ok(cmd) = commands.recv() {
            let AppCommand::Play {
                url,
                name,
                logo_url,
                country,
                taken,
            } = cmd
            else {
                continue;
            };
            {
                let mut st = state.lock().unwrap();
                st.play_seq += 1;
                st.station_url = Some(url.clone());
                st.station_name = name;
                st.station_logo_url = logo_url;
                st.station_country = country;
                st.is_resolving = true;
                st.last_error = None;
                st.playback = radiotrope::audio::PlaybackState::Stopped;
                if let Some(taken) = taken {
                    let _ = taken.send(st.play_seq);
                }
            }
            std::thread::sleep(Duration::from_millis(150));
            let mut st = state.lock().unwrap();
            st.is_resolving = false;
            if url.contains("broken") {
                st.last_error = Some("HTTP 404".into());
            } else {
                st.playback = radiotrope::audio::PlaybackState::Playing;
                st.codec_name = "MP3".into();
                st.bitrate = Some(128);
            }
        }
    });
}

#[tokio::test]
async fn play_waits_for_the_outcome() {
    let mut s = Session::start(Duration::ZERO).await;
    fake_controller(s.commands.clone(), s.state.clone());
    s.legacy_handshake("2025-11-25").await;

    let played = s
        .call(
            "play_url",
            json!({"url": "http://jazz.test/stream", "name": "Jazz FM"}),
        )
        .await;
    assert_eq!(text(&played), "Playing Jazz FM (MP3, 128 kbps)");

    let failed = s
        .call("play_url", json!({"url": "http://broken.test/stream"}))
        .await;
    assert_eq!(failed["isError"], true);
    assert!(text(&failed).contains("HTTP 404"), "{failed}");

    let by_id = s.call("play_station", json!({"id": "jazz-id"})).await;
    assert!(text(&by_id).starts_with("Playing Jazz FM"), "{by_id}");
    // The logo and country go with it, for the header
    let state = s.state.lock().unwrap().clone();
    assert_eq!(
        state.station_logo_url.as_deref(),
        Some("http://jazz.test/logo.png")
    );
    assert_eq!(state.station_country.as_deref(), Some("Greece, Attica"));
    let unknown = s.call("play_station", json!({"id": "nope"})).await;
    assert_eq!(unknown["isError"], true);
}

#[tokio::test]
async fn play_reports_its_own_station_not_one_started_just_before() {
    let mut s = Session::start(Duration::ZERO).await;
    fake_controller(s.commands.clone(), s.state.clone());
    s.legacy_handshake("2025-11-25").await;
    // The window starts a station that fails, just before the agent's
    s.window
        .send(AppCommand::Play {
            url: "http://broken.test/stream".into(),
            name: None,
            logo_url: None,
            country: None,
            taken: None,
        })
        .unwrap();
    let played = s
        .call(
            "play_url",
            json!({"url": "http://jazz.test/stream", "name": "Jazz FM"}),
        )
        .await;
    assert_eq!(text(&played), "Playing Jazz FM (MP3, 128 kbps)", "{played}");
}

#[tokio::test]
async fn a_busy_player_says_so_at_once() {
    // A queue with no room: the controller is stuck
    let (tx, rx) = bounded(1);
    tx.send(AppCommand::Stop).unwrap();
    let mut s = Session::with_channel(Duration::ZERO, (tx, rx)).await;
    s.legacy_handshake("2025-11-25").await;
    let started = std::time::Instant::now();
    for (tool, args) in [
        ("stop", json!({})),
        ("set_muted", json!({"muted": true})),
        ("play_url", json!({"url": "http://jazz.test/stream"})),
    ] {
        let r = s.call(tool, args).await;
        assert_eq!(r["isError"], true, "{tool}: {r}");
        assert!(text(&r).contains("busy"), "{tool}: {r}");
    }
    assert!(started.elapsed() < Duration::from_secs(2));
    // Nothing went in behind the Stop
    assert_eq!(s.commands.len(), 1);
}

#[tokio::test]
async fn add_favorite_refuses_overlong_text_and_too_many_favorites() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;
    let long_name = "n".repeat(513);
    let r = s
        .call(
            "add_favorite",
            json!({"url": "http://long.test/stream", "name": long_name}),
        )
        .await;
    assert_eq!(r["isError"], true, "{r}");
    let long_url = format!("http://long.test/{}", "x".repeat(2048));
    let r = s
        .call("add_favorite", json!({"url": long_url, "name": "Long"}))
        .await;
    assert_eq!(r["isError"], true, "{r}");
    // Counted in characters: 512 Greek letters (1024 bytes) are fine
    let greek = "α".repeat(512);
    let r = s
        .call(
            "add_favorite",
            json!({"url": "http://greek.test/stream", "name": greek}),
        )
        .await;
    assert_ne!(r["isError"], true, "{r}");

    {
        let mut favs = s.favorites.lock().unwrap();
        for i in favs.count()..1000 {
            favs.add(Favorite::new("Filler", format!("http://filler.test/{i}")))
                .unwrap();
        }
    }
    let r = s
        .call(
            "add_favorite",
            json!({"url": "http://one-more.test/stream", "name": "One more"}),
        )
        .await;
    assert_eq!(r["isError"], true, "{r}");
    assert!(text(&r).contains("1000"), "{r}");
    assert_eq!(s.favorites.lock().unwrap().count(), 1000);
}

#[tokio::test]
async fn an_agent_records_with_the_settings_the_window_has() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;
    {
        let mut st = s.state.lock().unwrap();
        st.playback = radiotrope::audio::PlaybackState::Playing;
        st.eq_enabled = true;
        st.recording_setup = crate::app::state::RecordingSetup {
            dir: Some(std::env::temp_dir().join("rt-rec-test")),
            format: radiotrope::audio::RecordingFormat::Opus,
            bitrate: Some(96),
            with_eq: true,
        };
    }
    // Nothing answers the command here: the tool says it is starting
    let r = s.call("start_recording", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    match s.commands.try_recv() {
        Ok(AppCommand::StartRecording {
            folder,
            format,
            bitrate,
            with_eq,
            ..
        }) => {
            assert_eq!(folder, std::env::temp_dir().join("rt-rec-test"));
            assert_eq!(format, radiotrope::audio::RecordingFormat::Opus);
            assert_eq!(bitrate, Some(96));
            assert!(with_eq);
        }
        _ => panic!("no StartRecording"),
    }
}

#[tokio::test]
async fn status_names_the_agent_that_changed_the_player() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;
    let status = s.call("get_status", json!({})).await;
    assert!(status["structuredContent"]["last_agent_change"].is_null());

    s.call("set_muted", json!({"muted": true})).await;
    assert!(matches!(s.commands.try_recv(), Ok(AppCommand::Mute)));
    let status = s.call("get_status", json!({})).await;
    let change = &status["structuredContent"]["last_agent_change"];
    assert_eq!(change["by"], "test");
    assert_eq!(change["action"], "mute");

    // A 2026-07-28 client names itself on the request
    let mut meta = modern_meta();
    meta["io.modelcontextprotocol/clientInfo"]["name"] = json!("other-agent");
    let mut m = Session::start(Duration::ZERO).await;
    m.request(
        "tools/call",
        json!({"name": "stop", "arguments": {}, "_meta": meta.clone()}),
    )
    .await;
    let status = m
        .request(
            "tools/call",
            json!({"name": "get_status", "arguments": {}, "_meta": meta}),
        )
        .await;
    assert_eq!(
        status["result"]["structuredContent"]["last_agent_change"]["by"],
        "other-agent"
    );
}

#[tokio::test]
async fn recording_needs_a_station_playing() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;
    let r = s.call("start_recording", json!({})).await;
    assert_eq!(r["isError"], true);
    let r = s.call("stop_recording", json!({})).await;
    assert_eq!(text(&r), "Not recording");
    assert!(s.commands.try_recv().is_err());
}

/// Answers schedule changes as the player does, keeping the entries in the
/// shared state; refuses a second recording
fn fake_scheduler(commands: Receiver<AppCommand>, state: Arc<Mutex<AppSnapshot>>) {
    use radiotrope_app::data::schedule::Action;
    std::thread::spawn(move || {
        while let Ok(cmd) = commands.recv() {
            let mut st = state.lock().unwrap();
            match cmd {
                AppCommand::SaveScheduleEntry { mut entry, reply } => {
                    let result = if entry.action == Action::Record
                        && st.schedule.iter().any(|e| e.action == Action::Record)
                    {
                        Err("Overlaps Record Jazz FM".to_string())
                    } else {
                        entry.id = st.schedule.len() as u64 + 1;
                        st.schedule.push(entry.clone());
                        Ok(entry.id)
                    };
                    let _ = reply.unwrap().send(result);
                }
                AppCommand::RemoveScheduleEntry { id, reply } => {
                    st.schedule.retain(|e| e.id != id);
                    let _ = reply.unwrap().send(Ok(()));
                }
                AppCommand::SetScheduleEntryEnabled { id, enabled, reply } => {
                    if let Some(e) = st.schedule.iter_mut().find(|e| e.id == id) {
                        e.enabled = enabled;
                    }
                    let _ = reply.unwrap().send(Ok(()));
                }
                _ => {}
            }
        }
    });
}

#[tokio::test]
async fn an_agent_schedules_a_recording_of_a_favorite() {
    let mut s = Session::start(Duration::ZERO).await;
    s.favorites
        .lock()
        .unwrap()
        .add(Favorite::from_station(
            Station::new("Rock FM", "http://rock.test/stream").with_logo("http://rock.test/l.png"),
        ))
        .unwrap();
    fake_scheduler(s.commands.clone(), s.state.clone());
    s.legacy_handshake("2025-11-25").await;

    let added = s
        .call(
            "add_schedule_entry",
            json!({"action": "record", "favorite_id": url_to_id("http://rock.test/stream"),
                   "start": "20:00", "end_at": "22:00", "days": ["sat"]}),
        )
        .await;
    assert_eq!(added["isError"], false, "{added}");
    assert!(
        text(&added).starts_with("Scheduled Record Rock FM at 20:00 (Saturdays · until 22:00)"),
        "{added}"
    );
    {
        let st = s.state.lock().unwrap();
        let entry = &st.schedule[0];
        let station = entry.station.as_ref().unwrap();
        assert_eq!(station.logo_url.as_deref(), Some("http://rock.test/l.png"));
    }

    // A second one at the same time is refused with the player's reason
    let again = s
        .call(
            "add_schedule_entry",
            json!({"action": "record", "url": "http://news.test/stream", "start": "20:30",
                   "end_after_minutes": "60", "days": ["weekends"]}),
        )
        .await;
    assert_eq!(again["isError"], true, "{again}");
    assert_eq!(text(&again), "Overlaps Record Jazz FM");

    let list = s.call("list_schedule", json!({})).await;
    let entries = list["structuredContent"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["days"], json!(["sat"]));
    assert_eq!(entries[0]["action"], "record");
    assert!(entries[0]["next"].is_string());

    let off = s
        .call(
            "set_schedule_entry_enabled",
            json!({"id": 1, "enabled": false}),
        )
        .await;
    assert_eq!(text(&off), "Switched off: Record Rock FM");
    let removed = s.call("remove_schedule_entry", json!({"id": "1"})).await;
    assert_eq!(text(&removed), "Removed Record Rock FM from the schedule");
    let gone = s.call("remove_schedule_entry", json!({"id": 1})).await;
    assert_eq!(gone["isError"], true);
}

#[tokio::test]
async fn schedule_arguments_are_checked_before_the_player_hears_of_them() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;
    for (args, error) in [
        (
            json!({"action": "play", "start": "25:00", "url": "http://a.test/s"}),
            "start must be",
        ),
        (
            json!({"action": "play", "start": "07:00"}),
            "Give the station",
        ),
        (
            json!({"action": "play", "start": "07:00", "url": "ftp://a"}),
            "url must start",
        ),
        (
            json!({"action": "play", "start": "07:00", "url": "http://a.test/s", "days": ["funday"]}),
            "Not a day",
        ),
        (
            json!({"action": "play", "start": "07:00", "url": "http://a.test/s", "volume": 150}),
            "volume must be",
        ),
        (
            json!({"action": "record", "start": "07:00", "url": "http://a.test/s", "end_at": "08:00", "end_after_minutes": 5}),
            "Give end_after_minutes or end_at",
        ),
    ] {
        let r = s.call("add_schedule_entry", args).await;
        assert_eq!(r["isError"], true, "{r}");
        assert!(text(&r).starts_with(error), "{r}");
    }
    assert!(s.commands.try_recv().is_err(), "nothing reached the player");
}

#[tokio::test]
async fn the_sleep_timer_tool_sets_and_clears_it() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;
    let set = s.call("set_sleep_timer", json!({"minutes": 45})).await;
    assert!(text(&set).starts_with("Playback stops in 45 min"), "{set}");
    assert!(matches!(
        s.commands.try_recv(),
        Ok(AppCommand::SetSleepTimer {
            minutes: Some(45),
            fade: true
        })
    ));
    let off = s.call("set_sleep_timer", json!({"minutes": 0})).await;
    assert_eq!(text(&off), "Sleep timer off");
    assert!(matches!(
        s.commands.try_recv(),
        Ok(AppCommand::SetSleepTimer { minutes: None, .. })
    ));
    let bad = s.call("set_sleep_timer", json!({"minutes": 5000})).await;
    assert_eq!(bad["isError"], true);
}
