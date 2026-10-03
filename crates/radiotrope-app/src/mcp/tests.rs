//! The MCP server end to end: JSON-RPC lines in, JSON-RPC lines out, over an
//! in-memory pipe, for both the handshake (legacy) and the stateless
//! (2026-07-28) kind of client

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
    Category, CategoryType, ProviderRegistry, SearchResults, StationProvider,
};

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
        Ok(vec![
            Category::new("jazz", "jazz", CategoryType::Genre).with_station_count(900),
            Category::new("pop", "pop", CategoryType::Genre).with_station_count(5000),
            Category::new("Greece", "Greece", CategoryType::Country)
                .with_station_count(700)
                .with_code(Some("GR".into())),
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
                .with_logo("http://jazz.test/logo.png")
                .with_metadata(Some("Greece".into()), None, Default::default())
        }))
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
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(FakeDirectory {
            delay: search_delay,
        }));
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
    assert_eq!(tools.len(), 19);
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
    let r = s
        .call("search_stations", json!({"order": "sideways"}))
        .await;
    assert_eq!(r["isError"], true, "{r}");
}

#[tokio::test]
async fn categories_are_listed_largest_first() {
    let mut s = Session::start(Duration::ZERO).await;
    s.legacy_handshake("2025-11-25").await;
    let r = s.call("list_categories", json!({"kind": "genre"})).await;
    let names: Vec<&str> = r["structuredContent"]["categories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["pop", "jazz"]);

    let r = s
        .call(
            "list_categories",
            json!({"kind": "country", "filter": "gr"}),
        )
        .await;
    assert_eq!(r["structuredContent"]["categories"][0]["code"], "GR");
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
    assert_eq!(state.station_country.as_deref(), Some("Greece"));
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
