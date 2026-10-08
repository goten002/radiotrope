//! The Remote API end to end, over real HTTPS on this computer, with the
//! player's certificate pinned as the app pins it

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use radiotrope_app::data::favorites::FavoritesManager;
use radiotrope_app::data::remote::RemoteStore;
use radiotrope_app::data::remote_cert::{self, PlayerCert};
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
    /// The fingerprint of the certificate the player serves
    fingerprint: String,
}

/// A connection to the player, over TLS
type Conn = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

/// Accepts only the certificate with this fingerprint, as the app does
#[derive(Debug)]
struct Pinned {
    fingerprint: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if remote_cert::fingerprint(end_entity) == self.fingerprint {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("not the paired player".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Connect to the player on `port`, accepting only the certificate with
/// `fingerprint`
async fn connect_pinned(port: u16, fingerprint: &str) -> std::io::Result<Conn> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned {
            fingerprint: fingerprint.to_string(),
            provider,
        }))
        .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    let name = rustls::pki_types::ServerName::try_from("radiotrope.local").unwrap();
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
}

/// Read until the other side closes; a close without TLS's goodbye counts
/// as the end too
async fn read_all(stream: &mut Conn, into: &mut Vec<u8>) -> std::io::Result<()> {
    match stream.read_to_end(into).await {
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(()),
        other => other.map(|_| ()),
    }
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
        let cert = PlayerCert::load_or_create_at(&dir.join("remote-cert.pem")).unwrap();
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
            tickets: Default::default(),
            calls: Default::default(),
            tls: server::tls_config(&cert).unwrap(),
            recordings: Default::default(),
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
            fingerprint: cert.fingerprint(),
        }
    }

    /// A connection to the player, its certificate checked
    async fn connect(&self) -> Conn {
        connect_pinned(self.port, &self.fingerprint).await.unwrap()
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
        let mut stream = self.connect().await;
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(15),
            read_all(&mut stream, &mut response),
        )
        .await
        .expect("no answer within 15 s")
        .unwrap();
        let text = String::from_utf8_lossy(&response).to_string();
        let status = text[9..12].parse().unwrap();
        let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    /// Send a request; the status, the headers as text and the raw body
    async fn request_raw(&self, method: &str, path: &str, headers: &str) -> (u16, String, Vec<u8>) {
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{headers}Connection: close\r\n\r\n"
        );
        let mut stream = self.connect().await;
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(15),
            read_all(&mut stream, &mut response),
        )
        .await
        .expect("no answer within 15 s")
        .unwrap();
        let split = response
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("headers end");
        let head = String::from_utf8_lossy(&response[..split]).to_string();
        let mut body = response[split + 4..].to_vec();
        // Read without knowing its length: undo the chunks
        if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            body = unchunk(&body);
        }
        (head[9..12].parse().unwrap(), head, body)
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

    /// GET `path` until `done` holds for its answer (2 s at most)
    async fn eventually(
        &self,
        path: &str,
        token: Option<&str>,
        done: impl Fn(&Value) -> bool,
    ) -> Value {
        let mut answer = Value::Null;
        for _ in 0..100 {
            answer = self.request("GET", path, token, None).await.1;
            if done(&answer) {
                return answer;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{path} never got there: {answer}");
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

/// The body of a chunked response
fn unchunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(line_end) = data.windows(2).position(|w| w == b"\r\n") else {
            return out;
        };
        let size =
            usize::from_str_radix(std::str::from_utf8(&data[..line_end]).unwrap().trim(), 16)
                .unwrap();
        if size == 0 {
            return out;
        }
        let start = line_end + 2;
        out.extend_from_slice(&data[start..start + size]);
        data = &data[start + size + 2..];
    }
}

/// A header's value in a response's headers
fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (n, v) = line.split_once(':')?;
        n.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().to_string())
    })
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
    assert_eq!(info["api"], 2);
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
    let mut stream = player.connect().await;
    let request = format!(
        "GET /v1/events HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();

    async fn read_until(stream: &mut Conn, seen: &mut String, needle: &str) {
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

#[tokio::test]
async fn plain_http_gets_no_answer() {
    let player = Player::start();
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", player.port))
        .await
        .unwrap();
    stream
        .write_all(b"GET /v1/info HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut response))
        .await
        .expect("closed within 15 s");
    assert!(!String::from_utf8_lossy(&response).contains("Test PC"));
}

#[tokio::test]
async fn a_phone_refuses_a_player_with_another_certificate() {
    let player = Player::start();
    let (other, _) = PlayerCert::generate().unwrap();
    assert!(connect_pinned(player.port, &other.fingerprint())
        .await
        .is_err());
    assert!(connect_pinned(player.port, &player.fingerprint)
        .await
        .is_ok());
}

#[tokio::test]
async fn a_phone_pairs_with_the_qr_code_once() {
    let player = Player::start();
    let secret = player
        .shared
        .pairing
        .lock()
        .unwrap()
        .start_qr(Instant::now())
        .unwrap();
    let pair = json!({"secret": secret, "device_id": "phone-qr", "device_name": "Pixel"});
    let (status, paired) = player
        .request("POST", "/v1/pair/qr", None, Some(pair.clone()))
        .await;
    assert_eq!(status, 200, "{paired}");
    let token = paired["token"].as_str().unwrap();
    let (status, _) = player.request("GET", "/v1/state", Some(token), None).await;
    assert_eq!(status, 200);

    // Used: the same code pairs nobody else
    let again = json!({"secret": secret, "device_id": "phone-2"});
    let (status, body) = player
        .request("POST", "/v1/pair/qr", None, Some(again))
        .await;
    assert_eq!(status, 401);
    assert_eq!(body["code"], "bad_qr");

    // A made-up secret, with no code shown
    let made_up = json!({"secret": "00".repeat(16), "device_id": "phone-3"});
    let (status, _) = player
        .request("POST", "/v1/pair/qr", None, Some(made_up))
        .await;
    assert_eq!(status, 401);
}

#[test]
fn the_qr_code_holds_what_the_phone_needs() {
    let payload = super::qr_payload(
        "ab",
        "cd",
        "ef",
        &["192.168.1.20:8766".into(), "10.0.0.2:8766".into()],
    );
    assert_eq!(
        payload,
        "radiotrope://pair?v=2&id=ab&fp=cd&s=ef&a=192.168.1.20:8766,10.0.0.2:8766"
    );
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
    // The player takes the commands up in its own time
    let state = player
        .eventually("/v1/state", t, |s| s["eq"]["preset"] == "Voice")
        .await;
    assert_eq!(state["eq"]["enabled"], true);

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
    let eq = player
        .eventually("/v1/eq", t, |e| e["preamp"] == -2.0)
        .await;
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

/// Open the listening stream at `url` (no token); the connection, after the
/// response's headers, which come back as text
async fn open_listening(player: &Player, url: &str) -> (Conn, String) {
    let mut stream = player.connect().await;
    let request = format!("GET {url} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte))
            .await
            .expect("no answer within 5 s")
            .unwrap();
        assert_eq!(n, 1, "closed before the headers ended");
        head.push(byte[0]);
    }
    (stream, String::from_utf8_lossy(&head).to_string())
}

#[tokio::test]
async fn a_paired_phone_listens_with_a_ticket_that_works_once() {
    let player = Player::start();
    player.state.lock().unwrap().listen = Some(radiotrope::audio::Listen::new());
    let token = player.pair("phone-1").await;

    // No token, no ticket
    let (status, _) = player.request("POST", "/v1/listen", None, None).await;
    assert_eq!(status, 401);

    let (status, ticket) = player
        .request("POST", "/v1/listen", Some(&token), None)
        .await;
    assert_eq!(status, 200, "{ticket}");
    assert_eq!(ticket["content_type"], "audio/mpeg");
    assert_eq!(ticket["bitrate_kbps"], 192);
    let url = ticket["url"].as_str().unwrap().to_string();
    assert!(url.starts_with("/v1/listen/stream?t="), "{url}");

    let (mut stream, head) = open_listening(&player, &url).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("content-type: audio/mpeg"), "{head}");
    // Silence comes while nothing plays: about 24 kB a second
    let mut audio = vec![0u8; 8_000];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut audio))
        .await
        .expect("no audio within 5 s")
        .unwrap();

    // The state names the phone while it listens
    let (_, state) = player.request("GET", "/v1/state", Some(&token), None).await;
    assert_eq!(state["listeners"], json!(["Pixel"]));

    // The same ticket doesn't open a second stream
    let (_, head) = open_listening(&player, &url).await;
    assert!(head.starts_with("HTTP/1.1 401"), "{head}");

    // Hanging up stops the listening
    drop(stream);
    let started = Instant::now();
    loop {
        let (_, state) = player.request("GET", "/v1/state", Some(&token), None).await;
        if state["listeners"] == json!([]) {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "{state}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn unpairing_ends_the_phone_s_listening() {
    let player = Player::start();
    let listen = radiotrope::audio::Listen::new();
    player.state.lock().unwrap().listen = Some(listen.clone());
    let token = player.pair("phone-1").await;
    let (_, ticket) = player
        .request("POST", "/v1/listen", Some(&token), None)
        .await;
    let (mut stream, head) = open_listening(&player, ticket["url"].as_str().unwrap()).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(listen.listeners(), 1);

    let (status, _) = player
        .request("DELETE", "/v1/pair", Some(&token), None)
        .await;
    assert_eq!(status, 204);
    // Even a phone that stopped reading is let go
    let started = Instant::now();
    while listen.listeners() > 0 {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "still listening after unpairing"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // And the stream ends: what is left to read finishes with the last chunk
    let mut rest = Vec::new();
    let mut buf = [0u8; 65536];
    while !rest.ends_with(b"0\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("the stream didn't end")
            .unwrap();
        assert!(n > 0, "closed without ending the stream");
        rest.extend_from_slice(&buf[..n]);
    }
}

#[tokio::test]
async fn listening_needs_the_audio_engine() {
    let player = Player::start();
    let token = player.pair("phone-1").await;
    let (status, body) = player
        .request("POST", "/v1/listen", Some(&token), None)
        .await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["code"], "no_audio");
}

#[tokio::test]
async fn a_made_up_ticket_is_refused() {
    let player = Player::start();
    player.state.lock().unwrap().listen = Some(radiotrope::audio::Listen::new());
    let (_, head) = open_listening(&player, "/v1/listen/stream?t=1234").await;
    assert!(head.starts_with("HTTP/1.1 401"), "{head}");
    let (_, head) = open_listening(&player, "/v1/listen/stream").await;
    assert!(head.starts_with("HTTP/1.1 401"), "{head}");
}

/// A phone's side of a WebRTC call, on this computer: offers to receive
/// audio and counts the Opus packets that arrive
struct PhoneCall {
    rtc: str0m::Rtc,
    socket: std::net::UdpSocket,
    pending: str0m::change::SdpPendingOffer,
}

impl PhoneCall {
    fn offer() -> (Self, String) {
        use str0m::media::{Direction, MediaKind};
        let mut rtc = str0m::RtcConfig::new()
            .clear_codecs()
            .enable_opus(true, false)
            .build(Instant::now());
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let candidate = str0m::Candidate::host(socket.local_addr().unwrap(), "udp").unwrap();
        rtc.add_local_candidate(candidate);
        let mut change = rtc.sdp_api();
        change.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
        let (offer, pending) = change.apply().unwrap();
        (
            Self {
                rtc,
                socket,
                pending,
            },
            offer.to_sdp_string(),
        )
    }

    /// Take the player's answer and run the call for `time`; the Opus
    /// packets that came, and whether the player hung up
    fn run(mut self, answer: &str, time: Duration) -> (Vec<Vec<u8>>, bool) {
        use str0m::{Event, IceConnectionState, Input, Output};
        let answer = str0m::change::SdpAnswer::from_sdp_string(answer).unwrap();
        self.rtc
            .sdp_api()
            .accept_answer(self.pending, answer)
            .unwrap();
        let local = self.socket.local_addr().unwrap();
        let end = Instant::now() + time;
        let mut packets = Vec::new();
        let mut buf = vec![0; 2000];
        while Instant::now() < end {
            let timeout = loop {
                match self.rtc.poll_output().unwrap() {
                    Output::Timeout(t) => break t,
                    Output::Transmit(t) => {
                        self.socket.send_to(&t.contents, t.destination).unwrap();
                    }
                    Output::Event(Event::MediaData(data)) => packets.push(data.data.to_vec()),
                    Output::Event(Event::IceConnectionStateChange(
                        IceConnectionState::Disconnected,
                    )) => return (packets, true),
                    Output::Event(_) => {}
                }
            };
            if !self.rtc.is_alive() {
                return (packets, true);
            }
            let wait = timeout
                .saturating_duration_since(Instant::now())
                .clamp(Duration::from_millis(1), Duration::from_millis(20));
            self.socket.set_read_timeout(Some(wait)).unwrap();
            buf.resize(2000, 0);
            let input = match self.socket.recv_from(&mut buf) {
                Ok((n, source)) => {
                    buf.truncate(n);
                    Input::Receive(
                        Instant::now(),
                        str0m::net::Receive {
                            proto: str0m::net::Protocol::Udp,
                            source,
                            destination: local,
                            contents: buf.as_slice().try_into().unwrap(),
                        },
                    )
                }
                Err(_) => Input::Timeout(Instant::now()),
            };
            self.rtc.handle_input(input).unwrap();
        }
        (packets, false)
    }
}

#[tokio::test]
async fn a_paired_phone_listens_over_webrtc_and_hangs_up() {
    let player = Player::start();
    player.state.lock().unwrap().listen = Some(radiotrope::audio::Listen::new());
    let token = player.pair("phone-1").await;
    let (phone, offer) = PhoneCall::offer();

    // Only with a token
    let body = json!({ "sdp": offer });
    let (status, _) = player
        .request("POST", "/v1/listen/webrtc", None, Some(body.clone()))
        .await;
    assert_eq!(status, 401);

    let (status, answer) = player
        .request("POST", "/v1/listen/webrtc", Some(&token), Some(body))
        .await;
    assert_eq!(status, 200, "{answer}");
    let sdp = answer["sdp"].as_str().unwrap().to_string();
    assert!(sdp.contains("opus/48000/2"), "{sdp}");
    assert!(sdp.contains("stereo=1"), "{sdp}");
    let call = answer["call"].as_str().unwrap().to_string();

    // Silence comes while nothing plays: 50 packets a second
    let calling = std::thread::spawn(move || phone.run(&sdp, Duration::from_secs(2)));
    let (_, state) = player.request("GET", "/v1/state", Some(&token), None).await;
    assert_eq!(state["listeners"], json!(["Pixel"]));
    let (packets, hung_up) = calling.join().unwrap();
    assert!(!hung_up);
    assert!(packets.len() >= 40, "only {} packets", packets.len());
    // Each one a stereo Opus packet
    assert!(packets.iter().all(|p| !p.is_empty() && p[0] & 0b100 != 0));

    // Hanging up ends the call (another phone can't)
    let (status, _) = player
        .request(
            "DELETE",
            &format!("/v1/listen/webrtc/{call}"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(status, 204);
    let started = Instant::now();
    loop {
        let (_, state) = player.request("GET", "/v1/state", Some(&token), None).await;
        if state["listeners"] == json!([]) {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "{state}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (status, _) = player
        .request(
            "DELETE",
            &format!("/v1/listen/webrtc/{call}"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn a_broken_offer_is_refused() {
    let player = Player::start();
    player.state.lock().unwrap().listen = Some(radiotrope::audio::Listen::new());
    let token = player.pair("phone-1").await;
    let (status, body) = player
        .request(
            "POST",
            "/v1/listen/webrtc",
            Some(&token),
            Some(json!({ "sdp": "v=0\r\nnot an offer" })),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "bad_offer");
    // Nobody joined the listeners
    let (_, state) = player.request("GET", "/v1/state", Some(&token), None).await;
    assert_eq!(state["listeners"], json!([]));
}

/// A file in the recording folder, last written a minute ago
fn old_recording(path: &std::path::Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(60))
        .unwrap();
}

#[tokio::test]
async fn recordings_are_shared_only_when_the_user_says() {
    let player = Player::start();
    let token = player.pair("phone-1").await;
    let folder = player.dir.join("rec");
    std::fs::create_dir_all(&folder).unwrap();
    old_recording(
        &folder.join("Jazz FM - 2026-10-08 20-15-03.mp3"),
        b"0123456789",
    );
    player.state.lock().unwrap().recording_setup.dir = Some(folder);

    let (status, body) = player
        .request("GET", "/v1/recordings", Some(&token), None)
        .await;
    assert_eq!(status, 403);
    assert_eq!(body["code"], "recordings_off");
    let (_, state) = player.request("GET", "/v1/state", Some(&token), None).await;
    assert_eq!(state["recordings_shared"], false);

    player.shared.recordings.set_on(true);
    let (_, state) = player.request("GET", "/v1/state", Some(&token), None).await;
    assert_eq!(state["recordings_shared"], true);
    let (status, list) = player
        .request("GET", "/v1/recordings", Some(&token), None)
        .await;
    assert_eq!(status, 200, "{list}");
    assert_eq!(list["recordings"][0]["size"], 10);
    // A phone that isn't paired sees nothing
    let (status, _) = player.request("GET", "/v1/recordings", None, None).await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn a_phone_downloads_and_deletes_a_recording() {
    let player = Player::start();
    let token = player.pair("phone-1").await;
    let auth = format!("Authorization: Bearer {token}\r\n");
    let folder = player.dir.join("rec");
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join("Jazz FM - 2026-10-08 20-15-03.mp3");
    old_recording(&file, b"0123456789");
    old_recording(&folder.join("notes.txt"), b"private");
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        player.dir.join("remote.json"),
        folder.join("Rock - 2026-10-08 20-15-03.mp3"),
    )
    .unwrap();
    player.state.lock().unwrap().recording_setup.dir = Some(folder.clone());
    player.shared.recordings.set_on(true);

    let (_, list) = player
        .request("GET", "/v1/recordings", Some(&token), None)
        .await;
    let recordings = list["recordings"].as_array().unwrap();
    assert_eq!(recordings.len(), 1, "only the recording: {list}");
    assert_eq!(recordings[0]["name"], "Jazz FM - 2026-10-08 20-15-03.mp3");
    assert_eq!(recordings[0]["format"], "mp3");
    assert_eq!(recordings[0]["recording"], false);
    let id = recordings[0]["id"].as_str().unwrap().to_string();
    let path = format!("/v1/recordings/{id}");

    // The whole file
    let (status, head, body) = player.request_raw("GET", &path, &auth).await;
    assert_eq!(status, 200, "{head}");
    assert_eq!(body, b"0123456789");
    assert_eq!(header_value(&head, "content-type").unwrap(), "audio/mpeg");
    assert!(header_value(&head, "content-disposition")
        .unwrap()
        .ends_with("Jazz%20FM%20-%202026-10-08%2020-15-03.mp3"));
    let etag = header_value(&head, "etag").unwrap();

    // The rest after an interrupted download
    let headers = format!("{auth}Range: bytes=4-\r\nIf-Match: {etag}\r\n");
    let (status, head, body) = player.request_raw("GET", &path, &headers).await;
    assert_eq!(status, 206, "{head}");
    assert_eq!(body, b"456789");
    assert_eq!(
        header_value(&head, "content-range").unwrap(),
        "bytes 4-9/10"
    );
    let headers = format!("{auth}Range: bytes=20-\r\n");
    assert_eq!(player.request_raw("GET", &path, &headers).await.0, 416);
    let headers = format!("{auth}If-Match: \"other\"\r\n");
    assert_eq!(player.request_raw("GET", &path, &headers).await.0, 412);
    // Ids are only what the player gave
    let (status, _, _) = player
        .request_raw("GET", "/v1/recordings/..%2Fremote.json", &auth)
        .await;
    assert_eq!(status, 404);

    // Being recorded: neither download nor delete
    player.state.lock().unwrap().recording = Some(crate::app::state::RecordingProgress {
        path: file.clone(),
        duration: Duration::from_secs(1),
        bytes: 10,
    });
    let (_, list) = player
        .request("GET", "/v1/recordings", Some(&token), None)
        .await;
    assert_eq!(list["recordings"][0]["recording"], true);
    assert_eq!(player.request_raw("GET", &path, &auth).await.0, 409);
    let headers = format!("{auth}If-Match: {etag}\r\n");
    assert_eq!(player.request_raw("DELETE", &path, &headers).await.0, 409);
    player.state.lock().unwrap().recording = None;

    // Deleting needs the ETag the phone saw
    assert_eq!(player.request_raw("DELETE", &path, &auth).await.0, 428);
    let (status, head, _) = player.request_raw("DELETE", &path, &headers).await;
    assert_eq!(status, 204, "{head}");
    assert!(!file.exists());
    assert!(folder.join("notes.txt").exists());
    assert_eq!(player.request_raw("GET", &path, &auth).await.0, 404);

    let activity = player.shared.recordings.activity();
    assert_eq!(
        activity.len(),
        2,
        "the download and the deletion, not the resume"
    );
    assert!(activity[0].deleted);
    assert_eq!(activity[0].device, "Pixel");
}
