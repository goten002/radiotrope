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
use crate::app::state::{AppCommand, AppSnapshot};
use crate::control::Control;

struct Player {
    shared: Shared,
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
        let shared = Shared {
            control,
            store: Arc::new(Mutex::new(store)),
            store_path: Some(store_path),
            pairing: Arc::new(Mutex::new(Pairing::default())),
            name: Arc::from("Test PC"),
            logos: None,
            changes: Arc::new(AtomicU64::new(0)),
        };
        let server = server::start(SocketAddr::from(([127, 0, 0, 1], 0)), shared.clone()).unwrap();
        Player {
            port: server.port(),
            shared,
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
