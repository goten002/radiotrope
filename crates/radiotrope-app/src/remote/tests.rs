//! The Remote API end to end, over real HTTP on this computer

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use radiotrope_app::data::favorites::FavoritesManager;
use radiotrope_app::data::remote::RemoteStore;
use radiotrope_app::data::types::{url_to_id, Favorite};
use radiotrope_app::providers::ProviderRegistry;

use super::pairing::Pairing;
use super::server::{self, is_local, Shared};
use super::WindowHooks;
use crate::app::state::{AppCommand, AppSnapshot};
use crate::control::Control;

struct Player {
    shared: Shared,
    /// Accents the window was told to show
    accents: Arc<Mutex<Vec<String>>>,
    /// Favorites the window was told were edited: old and new names
    edits: Arc<Mutex<Vec<(String, String)>>>,
    port: u16,
    _server: server::Server,
    state: Arc<Mutex<AppSnapshot>>,
    dir: std::path::PathBuf,
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Player {
    fn start() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("radiotrope-remote-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cmd_tx, commands) = unbounded();
        let state = Arc::new(Mutex::new(AppSnapshot::default()));
        let mut favs = FavoritesManager::new();
        favs.add(Favorite::new("Jazz FM", "http://jazz.test/stream"))
            .unwrap();
        let favorites = Arc::new(Mutex::new(favs));
        let control = Control::new(cmd_tx, state.clone(), favorites)
            .with_test_setup(ProviderRegistry::new(), dir.join("favorites.json"));
        fake_controller(commands, state.clone());
        let store_path = dir.join("remote.json");
        let store = RemoteStore::load_or_create_at(&store_path).unwrap();
        let accents = Arc::new(Mutex::new(Vec::new()));
        let edits = Arc::new(Mutex::new(Vec::new()));
        let (shown_accents, shown_edits) = (accents.clone(), edits.clone());
        let window = WindowHooks {
            accent: Some(Box::new(move |hex| shown_accents.lock().unwrap().push(hex))),
            favorite_edited: Some(Box::new(move |old: Favorite, new: Favorite| {
                shown_edits
                    .lock()
                    .unwrap()
                    .push((old.name().to_string(), new.name().to_string()))
            })),
        };
        let shared = Shared {
            control,
            store: Arc::new(Mutex::new(store)),
            store_path: Some(store_path),
            pairing: Arc::new(Mutex::new(Pairing::default())),
            name: Arc::from("Test PC"),
            logos: None,
            changes: Arc::new(AtomicU64::new(0)),
            window: Arc::new(window),
        };
        let server = server::start(SocketAddr::from(([127, 0, 0, 1], 0)), shared.clone()).unwrap();
        Player {
            port: server.port(),
            shared,
            accents,
            edits,
            _server: server,
            state,
            dir,
        }
    }

    /// Send a request; the status and the JSON body (Null when empty)
    async fn request(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        self.request_with(method, path, token, body, "").await
    }

    async fn request_with(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
        extra_headers: &str,
    ) -> (u16, Value) {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}{extra_headers}\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", self.port))
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut response))
            .await
            .expect("no answer within 15 s")
            .unwrap();
        let text = String::from_utf8_lossy(&response).to_string();
        let status = text[9..12].parse().unwrap();
        let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    /// Pair a phone the way the app does; its token
    async fn pair(&self, device_id: &str) -> String {
        let (status, started) = self
            .request(
                "POST",
                "/v1/pair",
                None,
                Some(json!({"device_id": device_id, "device_name": "Pixel"})),
            )
            .await;
        assert_eq!(status, 202, "{started}");
        let pairing_id = started["pairing_id"].as_str().unwrap().to_string();
        let code = self.shown_code();
        let (status, paired) = self
            .request(
                "POST",
                &format!("/v1/pair/{pairing_id}/code"),
                None,
                Some(json!({ "code": code })),
            )
            .await;
        assert_eq!(status, 200, "{paired}");
        assert_eq!(paired["player_name"], "Test PC");
        paired["token"].as_str().unwrap().to_string()
    }

    fn shown_code(&self) -> String {
        self.shared
            .pairing
            .lock()
            .unwrap()
            .shown(Instant::now())
            .expect("the player shows a code")
            .code
    }
}

/// Plays what it's told: starts, then plays 150 ms later
fn fake_controller(commands: Receiver<AppCommand>, state: Arc<Mutex<AppSnapshot>>) {
    std::thread::spawn(move || {
        while let Ok(cmd) = commands.recv() {
            match cmd {
                AppCommand::Play {
                    url, name, taken, ..
                } => {
                    {
                        let mut st = state.lock().unwrap();
                        st.play_seq += 1;
                        st.station_url = Some(url);
                        st.station_name = name;
                        st.is_resolving = true;
                        if let Some(taken) = taken {
                            let _ = taken.send(st.play_seq);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(150));
                    let mut st = state.lock().unwrap();
                    st.is_resolving = false;
                    st.playback = radiotrope::audio::PlaybackState::Playing;
                }
                AppCommand::Stop => {
                    state.lock().unwrap().playback = radiotrope::audio::PlaybackState::Stopped;
                }
                AppCommand::SetEqPreset(name) => {
                    let preset = radiotrope::audio::find_preset(&name).unwrap();
                    let mut st = state.lock().unwrap();
                    st.eq_gains = preset.gains;
                    st.eq_preset_name = Some(name);
                    st.eq_preamp = preset.preamp_db();
                }
                AppCommand::SetEqGains(gains) => {
                    let mut st = state.lock().unwrap();
                    st.eq_gains = gains;
                    st.eq_preset_name = None;
                }
                AppCommand::SetEqPreamp(db) => state.lock().unwrap().eq_preamp = db,
                AppCommand::SetEqEnabled(on) => state.lock().unwrap().eq_enabled = on,
                AppCommand::SaveScheduleEntry { mut entry, reply } => {
                    let mut st = state.lock().unwrap();
                    if entry.id == 0 {
                        entry.id = st.schedule.iter().map(|e| e.id).max().unwrap_or(0) + 1;
                    }
                    let id = entry.id;
                    st.schedule.retain(|e| e.id != id);
                    st.schedule.push(entry);
                    let _ = reply.unwrap().send(Ok(id));
                }
                AppCommand::RemoveScheduleEntry { id, reply } => {
                    let mut st = state.lock().unwrap();
                    let before = st.schedule.len();
                    st.schedule.retain(|e| e.id != id);
                    let found = st.schedule.len() < before;
                    let _ = reply
                        .unwrap()
                        .send(if found { Ok(()) } else { Err("gone".into()) });
                }
                AppCommand::SetScheduleEntryEnabled { id, enabled, reply } => {
                    let mut st = state.lock().unwrap();
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
async fn anyone_on_the_network_sees_who_the_player_is() {
    let player = Player::start();
    let (status, info) = player.request("GET", "/v1/info", None, None).await;
    assert_eq!(status, 200);
    assert_eq!(info["name"], "Test PC");
    assert_eq!(info["api"], 1);
    assert_eq!(info["paired"], false);
    assert_eq!(info["id"].as_str().unwrap().len(), 32);
}

#[tokio::test]
async fn only_paired_phones_reach_the_player() {
    let player = Player::start();
    let (status, body) = player.request("GET", "/v1/state", None, None).await;
    assert_eq!(status, 401);
    assert_eq!(body["code"], "not_paired");
    let (status, _) = player
        .request("GET", "/v1/state", Some(&"0".repeat(64)), None)
        .await;
    assert_eq!(status, 401);

    let token = player.pair("phone-1").await;
    let (status, state) = player.request("GET", "/v1/state", Some(&token), None).await;
    assert_eq!(status, 200, "{state}");
    assert_eq!(state["playback"], "stopped");
    assert_eq!(state["accent"], "#f7931e");
    let (_, info) = player.request("GET", "/v1/info", Some(&token), None).await;
    assert_eq!(info["paired"], true);

    // The pairing is kept on disk
    let saved = RemoteStore::load_or_create_at(&player.dir.join("remote.json")).unwrap();
    assert!(saved.find_by_token(&token).is_some());

    // Unpairing ends it
    let (status, _) = player
        .request("DELETE", "/v1/pair", Some(&token), None)
        .await;
    assert_eq!(status, 204);
    let (status, _) = player.request("GET", "/v1/state", Some(&token), None).await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn a_wrong_code_says_how_many_tries_are_left() {
    let player = Player::start();
    let (_, started) = player
        .request(
            "POST",
            "/v1/pair",
            None,
            Some(json!({"device_id": "phone-1", "device_name": "Pixel"})),
        )
        .await;
    let id = started["pairing_id"].as_str().unwrap();
    let wrong = format!(
        "{:04}",
        (player.shown_code().parse::<u32>().unwrap() + 1) % 10_000
    );
    let (status, body) = player
        .request(
            "POST",
            &format!("/v1/pair/{id}/code"),
            None,
            Some(json!({ "code": wrong })),
        )
        .await;
    assert_eq!(status, 403);
    assert_eq!(body["code"], "wrong_code");
    assert_eq!(body["tries_left"], 2);

    // Another phone waits its turn
    let (status, body) = player
        .request(
            "POST",
            "/v1/pair",
            None,
            Some(json!({"device_id": "phone-2", "device_name": "iPhone"})),
        )
        .await;
    assert_eq!(status, 409);
    assert_eq!(body["code"], "pairing_busy");
}

#[tokio::test]
async fn a_phone_plays_a_favorite_and_sets_the_volume() {
    let player = Player::start();
    let token = player.pair("phone-1").await;
    let id = url_to_id("http://jazz.test/stream");
    let (status, body) = player
        .request(
            "POST",
            "/v1/play",
            Some(&token),
            Some(json!({ "favorite_id": id })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"], "playing");

    let (status, _) = player
        .request(
            "PUT",
            "/v1/volume",
            Some(&token),
            Some(json!({"volume": 35})),
        )
        .await;
    assert_eq!(status, 204);
    let (_, state) = player.request("GET", "/v1/state", Some(&token), None).await;
    assert_eq!(state["playback"], "playing");
    assert_eq!(state["station"]["name"], "Jazz FM");
    assert_eq!(state["volume"], 35);

    let (status, body) = player
        .request(
            "POST",
            "/v1/play",
            Some(&token),
            Some(json!({ "favorite_id": "nope" })),
        )
        .await;
    assert_eq!(status, 404);
    assert_eq!(body["code"], "no_favorite");

    let (status, _) = player
        .request(
            "PUT",
            "/v1/volume",
            Some(&token),
            Some(json!({"volume": 101})),
        )
        .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn the_event_stream_starts_with_the_state_and_follows_it() {
    let player = Player::start();
    let token = player.pair("phone-1").await;
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", player.port))
        .await
        .unwrap();
    let request = format!(
        "GET /v1/events HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();

    async fn read_until(stream: &mut tokio::net::TcpStream, seen: &mut String, needle: &str) {
        let mut buf = [0u8; 4096];
        while !seen.contains(needle) {
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
                .await
                .expect("no event within 5 s")
                .unwrap();
            assert!(n > 0, "stream closed: {seen}");
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    }
    let mut seen = String::new();
    read_until(&mut stream, &mut seen, "\"volume\":100").await;
    assert!(seen.contains("text/event-stream"));
    assert!(seen.contains("event: state"));

    player.state.lock().unwrap().volume = 0.2;
    read_until(&mut stream, &mut seen, "\"volume\":20").await;
}

#[tokio::test]
async fn web_pages_are_refused() {
    let player = Player::start();
    let (status, body) = player
        .request_with(
            "GET",
            "/v1/info",
            None,
            None,
            "Origin: https://evil.example\r\n",
        )
        .await;
    assert_eq!(status, 403);
    assert_eq!(body["code"], "web_page");
}

#[test]
fn only_local_addresses_count_as_the_local_network() {
    let local = [
        "127.0.0.1",
        "192.168.1.20",
        "10.0.0.5",
        "172.16.3.4",
        "100.101.1.2",
        "::1",
        "fe80::1",
        "fd00::1",
    ];
    for ip in local {
        assert!(is_local(ip.parse::<IpAddr>().unwrap()), "{ip}");
    }
    for ip in ["8.8.8.8", "100.128.0.1", "2001:db8::1"] {
        assert!(!is_local(ip.parse::<IpAddr>().unwrap()), "{ip}");
    }
}

#[tokio::test]
async fn a_phone_edits_orders_and_removes_favorites() {
    let player = Player::start();
    let token = player.pair("phone-1").await;
    let t = Some(token.as_str());
    let jazz = url_to_id("http://jazz.test/stream");

    let (status, list) = player.request("GET", "/v1/favorites", t, None).await;
    assert_eq!(status, 200, "{list}");
    assert_eq!(list["favorites"][0]["id"], jazz.as_str());
    assert_eq!(list["favorites"][0]["logo"], Value::Null);
    let rev = list["rev"].as_u64().unwrap();

    // A stream typed in, with a logo
    let (status, added) = player
        .request(
            "POST",
            "/v1/favorites",
            t,
            Some(json!({"url": "http://rock.test/live", "name": "Rock", "logo_url": "http://rock.test/l.png"})),
        )
        .await;
    assert_eq!(status, 201, "{added}");
    let rock = url_to_id("http://rock.test/live");
    assert_eq!(added["id"], rock.as_str());
    assert_eq!(added["logo"], format!("/v1/logos/{rock}"));
    let (_, state) = player.request("GET", "/v1/state", t, None).await;
    assert_ne!(state["favorites_rev"].as_u64().unwrap(), rev);

    // Rock first
    let (status, _) = player
        .request(
            "PUT",
            "/v1/favorites/order",
            t,
            Some(json!({"ids": [rock]})),
        )
        .await;
    assert_eq!(status, 204);
    let (_, list) = player.request("GET", "/v1/favorites", t, None).await;
    let names: Vec<&str> = list["favorites"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Rock", "Jazz FM"]);

    // A new name and stream: a new id, and the window hears of it
    let (status, edited) = player
        .request(
            "PATCH",
            &format!("/v1/favorites/{jazz}"),
            t,
            Some(json!({"name": "Jazz 24", "url": "https://jazz.test/hq"})),
        )
        .await;
    assert_eq!(status, 200, "{edited}");
    let jazz24 = url_to_id("https://jazz.test/hq");
    assert_eq!(edited["id"], jazz24.as_str());
    assert_eq!(
        player.edits.lock().unwrap().clone(),
        [("Jazz FM".to_string(), "Jazz 24".to_string())]
    );

    // Two favorites can't share a stream
    let (status, body) = player
        .request(
            "PATCH",
            &format!("/v1/favorites/{jazz24}"),
            t,
            Some(json!({"url": "http://rock.test/live"})),
        )
        .await;
    assert_eq!(status, 422);
    assert_eq!(
        body["message"],
        "Another favorite already has this stream URL"
    );
    let (status, body) = player
        .request("PATCH", "/v1/favorites/nope", t, Some(json!({"name": "x"})))
        .await;
    assert_eq!(status, 404);
    assert_eq!(body["code"], "no_favorite");
    let (status, _) = player
        .request(
            "PATCH",
            &format!("/v1/favorites/{jazz24}"),
            t,
            Some(json!({"url": "ftp://jazz.test/hq"})),
        )
        .await;
    assert_eq!(status, 400);

    let (status, _) = player
        .request("DELETE", &format!("/v1/favorites/{rock}"), t, None)
        .await;
    assert_eq!(status, 204);
    let (_, list) = player.request("GET", "/v1/favorites", t, None).await;
    assert_eq!(list["favorites"].as_array().unwrap().len(), 1);
    let (status, _) = player
        .request("DELETE", &format!("/v1/favorites/{rock}"), t, None)
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn a_phone_sets_the_equalizer() {
    let player = Player::start();
    let token = player.pair("phone-1").await;
    let t = Some(token.as_str());

    let (status, eq) = player.request("GET", "/v1/eq", t, None).await;
    assert_eq!(status, 200, "{eq}");
    assert_eq!(eq["bands"][0], "40");
    assert_eq!(eq["max_db"], 12.0);
    assert_eq!(eq["presets"][0]["name"], "Flat");
    assert_eq!(eq["presets"][0]["group"], Value::Null);
    assert_eq!(eq["presets"][1]["group"], "Listening");

    let (status, _) = player
        .request(
            "PUT",
            "/v1/eq",
            t,
            Some(json!({"enabled": true, "preset": "Voice"})),
        )
        .await;
    assert_eq!(status, 204);
    let (_, state) = player.request("GET", "/v1/state", t, None).await;
    assert_eq!(state["eq"]["enabled"], true);
    assert_eq!(state["eq"]["preset"], "Voice");

    // Gains past the faders' ends are held at them
    let (status, _) = player
        .request(
            "PUT",
            "/v1/eq",
            t,
            Some(json!({"gains": [20, 0, 0, 0, 0, 0, 0, 0, 0, -30], "preamp": -2})),
        )
        .await;
    assert_eq!(status, 204);
    let (_, eq) = player.request("GET", "/v1/eq", t, None).await;
    assert_eq!(eq["preset"], Value::Null);
    assert_eq!(eq["gains"][0], 12.0);
    assert_eq!(eq["gains"][9], -12.0);
    assert_eq!(eq["preamp"], -2.0);

    for bad in [
        json!({"preset": "Voice", "gains": [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]}),
        json!({"preset": "Nope"}),
        json!({"gains": [0, 0]}),
    ] {
        let (status, body) = player.request("PUT", "/v1/eq", t, Some(bad)).await;
        assert_eq!(status, 400, "{body}");
    }
}

#[tokio::test]
async fn a_phone_schedules_an_alarm_and_changes_it() {
    let player = Player::start();
    let token = player.pair("phone-1").await;
    let t = Some(token.as_str());
    let jazz = url_to_id("http://jazz.test/stream");

    let (status, entry) = player
        .request(
            "POST",
            "/v1/schedule",
            t,
            Some(json!({
                "action": "play", "start": "07:30", "days": ["weekdays"],
                "favorite_id": jazz, "end_after_minutes": 60, "volume": 40, "fade": true
            })),
        )
        .await;
    assert_eq!(status, 201, "{entry}");
    let id = entry["id"].as_u64().unwrap();
    assert_eq!(entry["enabled"], true);
    assert_eq!(entry["station"]["name"], "Jazz FM");
    assert_eq!(entry["days"], json!(["mon", "tue", "wed", "thu", "fri"]));
    assert_eq!(entry["end"], json!({"kind": "after", "minutes": 60}));
    assert_eq!(entry["volume"], 40);
    assert!(entry["next_label"].is_string());

    let (_, list) = player.request("GET", "/v1/schedule", t, None).await;
    assert_eq!(list["entries"].as_array().unwrap().len(), 1);

    // Switched off, then moved: it stays off
    let (status, _) = player
        .request(
            "PUT",
            &format!("/v1/schedule/{id}/enabled"),
            t,
            Some(json!({"enabled": false})),
        )
        .await;
    assert_eq!(status, 204);
    let (status, entry) = player
        .request(
            "PUT",
            &format!("/v1/schedule/{id}"),
            t,
            Some(json!({"action": "stop", "start": "23:00", "days": ["daily"]})),
        )
        .await;
    assert_eq!(status, 200, "{entry}");
    assert_eq!(entry["id"], id);
    assert_eq!(entry["enabled"], false);
    assert_eq!(entry["start"], "23:00");
    assert_eq!(entry["station"], Value::Null);

    let (status, body) = player
        .request(
            "POST",
            "/v1/schedule",
            t,
            Some(json!({"action": "play", "start": "25:00", "favorite_id": jazz})),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(
        body["message"],
        "start must be a time like 07:30, not \"25:00\""
    );
    let (status, body) = player
        .request(
            "POST",
            "/v1/schedule",
            t,
            Some(json!({"action": "play", "start": "07:00", "favorite_id": "nope"})),
        )
        .await;
    assert_eq!(status, 404);
    assert_eq!(body["code"], "no_favorite");

    let (status, _) = player
        .request("DELETE", &format!("/v1/schedule/{id}"), t, None)
        .await;
    assert_eq!(status, 204);
    let (status, body) = player
        .request(
            "PUT",
            "/v1/schedule/abc",
            t,
            Some(json!({"action": "stop", "start": "23:00"})),
        )
        .await;
    assert_eq!(status, 404);
    assert_eq!(body["code"], "no_entry");
}

#[tokio::test]
async fn a_phone_picks_the_accent() {
    let player = Player::start();
    let token = player.pair("phone-1").await;
    let t = Some(token.as_str());

    let (status, look) = player.request("GET", "/v1/appearance", t, None).await;
    assert_eq!(status, 200, "{look}");
    assert_eq!(look["accent"], "#f7931e");
    assert_eq!(look["default"], "#f7931e");
    assert_eq!(look["swatches"].as_array().unwrap().len(), 10);
    assert_eq!(
        look["swatches"][5],
        json!({"color": "#3584e4", "name": "Blue"})
    );

    let (status, _) = player
        .request(
            "PUT",
            "/v1/appearance",
            t,
            Some(json!({"accent": "#3584E4"})),
        )
        .await;
    assert_eq!(status, 204);
    let (_, state) = player.request("GET", "/v1/state", t, None).await;
    assert_eq!(state["accent"], "#3584e4");
    assert_eq!(player.accents.lock().unwrap().clone(), ["#3584e4"]);

    let (status, _) = player
        .request("PUT", "/v1/appearance", t, Some(json!({"accent": "blue"})))
        .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn timers_recording_and_search_check_what_they_are_given() {
    let player = Player::start();
    let token = player.pair("phone-1").await;
    let t = Some(token.as_str());

    let (status, _) = player
        .request("PUT", "/v1/sleep-timer", t, Some(json!({"minutes": 30})))
        .await;
    assert_eq!(status, 204);
    let (status, _) = player
        .request("PUT", "/v1/sleep-timer", t, Some(json!({"minutes": 5000})))
        .await;
    assert_eq!(status, 400);

    let (status, body) = player.request("POST", "/v1/recording", t, None).await;
    assert_eq!(status, 422);
    assert_eq!(body["message"], "Nothing is playing; start a station first");
    let (status, body) = player.request("DELETE", "/v1/recording", t, None).await;
    assert_eq!(status, 200);
    assert_eq!(body["result"], "not_recording");

    let (status, body) = player
        .request("GET", "/v1/search?order=loud", t, None)
        .await;
    assert_eq!(status, 400, "{body}");
    let (status, _) = player.request("GET", "/v1/categories/mood", t, None).await;
    assert_eq!(status, 404);
}
