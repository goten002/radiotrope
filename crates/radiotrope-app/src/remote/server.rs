//! The Remote API: phones on the local network drive the player over HTTPS
//!
//! JSON requests under `/v1`, and one Server-Sent Events stream
//! (`/v1/events`) that sends the player's state whenever it changes. Every
//! request but `info` and pairing carries a paired phone's token as
//! `Authorization: Bearer <token>`. Served over TLS with the player's own
//! certificate ([`PlayerCert`]), which phones pin when they pair, so nobody
//! else on the network can read a token or pose as the player. Meant for
//! the local network: requests from addresses outside it are refused, and
//! so are requests from web pages (an `Origin` header).

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr, TcpListener as StdListener};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full, Limited};
use hyper::body::{Frame, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use radiotrope_app::config::remote::{
    API_VERSION, CODE_LIFETIME, EVENT_KEEP_ALIVE, EVENT_POLL, HEADER_READ_TIMEOUT, MAX_BODY,
    MAX_CONNECTIONS,
};
use radiotrope_app::data::remote::RemoteStore;
use radiotrope_app::data::remote_cert::PlayerCert;
use radiotrope_app::data::types::{url_to_id, Station};
use radiotrope_app::network::logo::LogoService;

use super::pairing::{CheckError, Pairing, StartError};
use super::state::State;
use super::{controls, library, recordings, webrtc, WindowHooks};
use crate::control::{self, Control, Played, MAX_NAME_CHARS, MAX_URL_CHARS};

pub(super) type Body = BoxBody<Bytes, Infallible>;

/// How long a play waits for the station unless told otherwise
const DEFAULT_PLAY_WAIT: Duration = Duration::from_secs(10);
/// Longest wait a play accepts
const MAX_PLAY_WAIT_SECS: f64 = 30.0;
/// Longest phone id accepted
const MAX_DEVICE_ID_CHARS: usize = 128;

/// What every request reaches: the player, the paired phones and the
/// pairing in progress
#[derive(Clone)]
pub struct Shared {
    pub control: Control,
    pub store: Arc<Mutex<RemoteStore>>,
    /// Where the store is saved; `None` is the usual folder
    pub store_path: Option<PathBuf>,
    pub pairing: Arc<Mutex<Pairing>>,
    /// The name phones see
    pub name: Arc<str>,
    pub logos: Option<Arc<LogoService>>,
    /// Counts changes the Remote Control dialog shows: phones paired or
    /// removed, a pairing started
    pub changes: Arc<AtomicU64>,
    /// What the window does about a phone's changes
    pub window: Arc<WindowHooks>,
    /// Phones listening over WebRTC
    pub calls: Arc<Mutex<webrtc::Calls>>,
    /// The player's certificate and key, for every connection
    pub tls: Arc<rustls::ServerConfig>,
    /// Recordings shared with phones
    pub recordings: Arc<recordings::Recordings>,
}

impl Shared {
    fn store(&self) -> std::sync::MutexGuard<'_, RemoteStore> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pairing(&self) -> std::sync::MutexGuard<'_, Pairing> {
        self.pairing.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn calls(&self) -> std::sync::MutexGuard<'_, webrtc::Calls> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The name of the paired phone `device_id`; `None` once unpaired
    pub(super) fn device_name(&self, device_id: &str) -> Option<String> {
        self.store()
            .devices
            .iter()
            .find(|d| d.id == device_id)
            .map(|d| d.name.clone())
    }

    /// Save the paired phones off the async thread, and wait until they
    /// are on disk
    async fn save_store(&self) {
        let store = self.store.clone();
        let path = self.store_path.clone();
        let saved = tokio::task::spawn_blocking(move || {
            radiotrope_app::data::remote::save_shared(&store, path.as_deref())
        })
        .await;
        match saved {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("Remote Control: can't save the paired phones: {e}"),
            Err(e) => eprintln!("Remote Control: can't save the paired phones: {e}"),
        }
    }

    pub(super) fn changed(&self) {
        self.changes.fetch_add(1, Ordering::SeqCst);
    }
}

/// The running server; dropping it stops the server
pub struct Server {
    cancel: CancellationToken,
    /// Hears once the server has let go of its port
    stopped: std::sync::mpsc::Receiver<()>,
    port: u16,
}

impl Server {
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.cancel.cancel();
        // A restart binds the same port next, so wait for it to be free
        let _ = self.stopped.recv_timeout(Duration::from_secs(2));
    }
}

/// TLS with the player's own certificate. aws-lc-rs as the crypto: it is
/// already built for WebRTC, on Windows and the Pi too.
pub fn tls_config(cert: &PlayerCert) -> Result<Arc<rustls::ServerConfig>, String> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_der.clone()));
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(|b| {
            b.with_no_client_auth()
                .with_single_cert(vec![CertificateDer::from(cert.cert_der.clone())], key)
        })
        .map_err(|e| format!("Can't use the player's certificate: {e}"))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Start listening on `addr`. Binding happens before this returns, so its
/// errors come back here, in words for the user.
pub fn start(addr: SocketAddr, shared: Shared) -> Result<Server, String> {
    let listener = StdListener::bind(addr).map_err(|e| match e.kind() {
        std::io::ErrorKind::AddrInUse => {
            format!("Port {} is in use by another program", addr.port())
        }
        _ => format!("Can't listen on {addr}: {e}"),
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("Can't listen on {addr}: {e}"))?;
    let port = listener
        .local_addr()
        .map(|a| a.port())
        .unwrap_or(addr.port());

    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let (stopped_tx, stopped) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("remote-api".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("Remote Control: failed to start: {e}");
                    return;
                }
            };
            runtime.block_on(serve(listener, shared, stop));
            let _ = stopped_tx.send(());
            // Event streams still open end with the runtime
            runtime.shutdown_background();
        })
        .map_err(|e| format!("Can't start the Remote Control server: {e}"))?;
    Ok(Server {
        cancel,
        stopped,
        port,
    })
}

async fn serve(listener: StdListener, shared: Shared, cancel: CancellationToken) {
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Remote Control: {e}");
            return;
        }
    };
    // A connection holds a place while open; with none free, the next one
    // waits in the system's queue
    let places = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let place = tokio::select! {
            _ = cancel.cancelled() => return,
            place = places.clone().acquire_owned() => match place {
                Ok(place) => place,
                Err(_) => return,
            },
        };
        let (stream, peer) = tokio::select! {
            _ = cancel.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("Remote Control: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let shared = shared.clone();
        let cancel = cancel.clone();
        // Where the phone reached the player: listening over WebRTC
        // offers a port on the same address
        let local = match stream.local_addr() {
            Ok(addr) => addr.ip().to_canonical(),
            Err(_) => continue,
        };
        tokio::spawn(async move {
            serve_connection(stream, peer.ip().to_canonical(), local, shared, cancel).await;
            drop(place);
        });
    }
}

async fn serve_connection(
    stream: tokio::net::TcpStream,
    peer: IpAddr,
    local: IpAddr,
    shared: Shared,
    cancel: CancellationToken,
) {
    // The handshake has as long as a request's headers would
    let acceptor = tokio_rustls::TlsAcceptor::from(shared.tls.clone());
    let stream = tokio::select! {
        _ = cancel.cancelled() => return,
        accepted = tokio::time::timeout(HEADER_READ_TIMEOUT, acceptor.accept(stream)) => {
            match accepted {
                Ok(Ok(stream)) => stream,
                // Not TLS (an old app speaking plain HTTP), too slow, or a
                // client that refused the certificate
                _ => return,
            }
        }
    };
    let handler_cancel = cancel.clone();
    let handler = hyper::service::service_fn(move |request: Request<Incoming>| {
        let shared = shared.clone();
        let cancel = handler_cancel.clone();
        async move { Ok::<_, Infallible>(handle(&shared, peer, local, request, cancel).await) }
    });
    // Also closes a connection left idle between requests
    let connection = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .serve_connection(TokioIo::new(stream), handler);
    tokio::pin!(connection);
    tokio::select! {
        _ = connection.as_mut() => {}
        _ = cancel.cancelled() => connection.as_mut().graceful_shutdown(),
    }
}

/// A paired phone making a request
struct Caller {
    device_id: String,
}

pub async fn handle(
    shared: &Shared,
    peer: IpAddr,
    local: IpAddr,
    request: Request<Incoming>,
    cancel: CancellationToken,
) -> Response<Body> {
    if !is_local(peer) {
        return error(
            StatusCode::FORBIDDEN,
            "not_local",
            "Only phones on the local network may use the player",
        );
    }
    if request.headers().contains_key(http::header::ORIGIN) {
        return error(
            StatusCode::FORBIDDEN,
            "web_page",
            "Web pages may not use the player",
        );
    }
    let path = request.uri().path().to_string();
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let method = request.method().clone();

    // Open to any phone on the network
    match (&method, parts.as_slice()) {
        (&Method::GET, ["v1", "info"]) => return info(shared, &request),
        (&Method::POST, ["v1", "pair"]) => return pair_start(shared, request).await,
        (&Method::POST, ["v1", "pair", "qr"]) => return pair_qr(shared, request).await,
        (&Method::POST, ["v1", "pair", id, "code"]) => {
            let id = id.to_string();
            return pair_code(shared, &id, request).await;
        }
        _ => {}
    }

    let Some(caller) = authorize(shared, &request) else {
        let mut response = error(
            StatusCode::UNAUTHORIZED,
            "not_paired",
            "This phone isn't paired with the player",
        );
        response.headers_mut().insert(
            http::header::WWW_AUTHENTICATE,
            http::HeaderValue::from_static("Bearer"),
        );
        return response;
    };

    match (&method, parts.as_slice()) {
        (&Method::DELETE, ["v1", "pair"]) => unpair(shared, &caller).await,
        (&Method::GET, ["v1", "state"]) => {
            let rev = shared.control.favorites_generation().await;
            json(StatusCode::OK, &state_now(shared, rev))
        }
        (&Method::GET, ["v1", "events"]) => events(shared, cancel),
        (&Method::POST, ["v1", "listen", "webrtc"]) => {
            webrtc::offer(shared, &caller.device_id, local, request, cancel).await
        }
        (&Method::DELETE, ["v1", "listen", "webrtc", call]) => {
            webrtc::hang_up(shared, &caller.device_id, call)
        }
        (&Method::POST, ["v1", "play"]) => play(shared, request).await,
        (&Method::POST, ["v1", "stop"]) => done(shared.control.stop()),
        (&Method::PUT, ["v1", "volume"]) => volume(shared, request).await,
        (&Method::PUT, ["v1", "mute"]) => mute(shared, request).await,
        (&Method::GET, ["v1", "logos", id]) => logo(shared, id).await,
        (&Method::GET, ["v1", "search"]) => library::search(shared, &request).await,
        (&Method::GET, ["v1", "categories", kind]) => {
            library::categories(shared, kind, &request).await
        }
        (&Method::GET, ["v1", "favorites"]) => library::favorites(shared).await,
        (&Method::POST, ["v1", "favorites"]) => library::add_favorite(shared, request).await,
        (&Method::PUT, ["v1", "favorites", "order"]) => library::reorder(shared, request).await,
        (&Method::POST, ["v1", "favorites", "import"]) => {
            library::import_favorites(shared, request).await
        }
        (&Method::POST, ["v1", "favorites", "import", "undo"]) => {
            library::undo_import(shared).await
        }
        (&Method::PATCH, ["v1", "favorites", id]) => {
            library::edit_favorite(shared, id, request).await
        }
        (&Method::DELETE, ["v1", "favorites", id]) => library::remove_favorite(shared, id).await,
        (&Method::POST, ["v1", "recording"]) => controls::start_recording(shared).await,
        (&Method::DELETE, ["v1", "recording"]) => controls::stop_recording(shared).await,
        (&Method::PUT, ["v1", "recording-settings"]) => {
            controls::set_recording_settings(shared, request).await
        }
        (&Method::GET, ["v1", "eq"]) => controls::eq(shared),
        (&Method::PUT, ["v1", "eq"]) => controls::set_eq(shared, request).await,
        (&Method::PUT, ["v1", "sleep-timer"]) => controls::sleep_timer(shared, request).await,
        (&Method::GET, ["v1", "schedule"]) => controls::schedule(shared),
        (&Method::POST, ["v1", "schedule"]) => controls::add_entry(shared, request).await,
        (&Method::PUT, ["v1", "schedule", id]) => {
            controls::replace_entry(shared, id, request).await
        }
        (&Method::DELETE, ["v1", "schedule", id]) => controls::remove_entry(shared, id).await,
        (&Method::PUT, ["v1", "schedule", id, "enabled"]) => {
            controls::enable_entry(shared, id, request).await
        }
        (&Method::GET, ["v1", "recordings"]) => recordings::list_all(shared).await,
        (&Method::GET, ["v1", "recordings", id]) => {
            recordings::download(shared, &caller.device_id, id, &request, cancel).await
        }
        (&Method::DELETE, ["v1", "recordings", id]) => {
            recordings::delete(shared, &caller.device_id, id, &request).await
        }
        (&Method::GET, ["v1", "appearance"]) => controls::appearance(shared),
        (&Method::PUT, ["v1", "appearance"]) => controls::set_appearance(shared, request).await,
        (_, ["v1", ..]) => error(StatusCode::NOT_FOUND, "not_found", "No such request"),
        _ => error(
            StatusCode::NOT_FOUND,
            "not_found",
            "Not found: phones use /v1",
        ),
    }
}

/// The paired phone whose token the request carries; notes its use
fn authorize(shared: &Shared, request: &Request<Incoming>) -> Option<Caller> {
    let presented = request
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token)?;
    let (device_id, touched) = {
        let mut store = shared.store();
        let device_id = store.find_by_token(presented)?.id.clone();
        let touched = store.touch(&device_id, unix_now());
        (device_id, touched)
    };
    if touched {
        // Only the time it was last used: the request needn't wait
        let saving = shared.clone();
        tokio::spawn(async move { saving.save_store().await });
        shared.changed();
    }
    Some(Caller { device_id })
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Info {
    name: String,
    id: String,
    api: u32,
    version: &'static str,
    /// The request carried a paired phone's token
    paired: bool,
}

fn info(shared: &Shared, request: &Request<Incoming>) -> Response<Body> {
    let paired = request
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token)
        .is_some_and(|token| shared.store().find_by_token(token).is_some());
    json(
        StatusCode::OK,
        &Info {
            name: shared.name.to_string(),
            id: shared.store().player_id.clone(),
            api: API_VERSION,
            version: env!("CARGO_PKG_VERSION"),
            paired,
        },
    )
}

#[derive(Deserialize)]
struct PairStart {
    device_id: String,
    #[serde(default)]
    device_name: String,
}

#[derive(Serialize)]
struct PairStarted {
    pairing_id: String,
    expires_in: u64,
}

async fn pair_start(shared: &Shared, request: Request<Incoming>) -> Response<Body> {
    let body: PairStart = match read_json(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let device_id = body.device_id.trim();
    if device_id.is_empty() || device_id.chars().count() > MAX_DEVICE_ID_CHARS {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "device_id must be 1 to 128 characters",
        );
    }
    let started = shared
        .pairing()
        .start(device_id, &body.device_name, Instant::now());
    match started {
        Ok(pairing_id) => {
            shared.changed();
            json(
                StatusCode::ACCEPTED,
                &PairStarted {
                    pairing_id,
                    expires_in: CODE_LIFETIME.as_secs(),
                },
            )
        }
        Err(StartError::Locked) => error(
            StatusCode::LOCKED,
            "pairing_locked",
            "Too many wrong codes. Open Remote Control on the player to pair again.",
        ),
        Err(StartError::Busy) => error(
            StatusCode::CONFLICT,
            "pairing_busy",
            "Another phone is pairing; try again in a moment",
        ),
    }
}

#[derive(Deserialize)]
struct PairCode {
    code: String,
}

#[derive(Serialize)]
struct Paired {
    token: String,
    player_id: String,
    player_name: String,
}

#[derive(Serialize)]
struct WrongCode {
    code: &'static str,
    message: String,
    tries_left: u32,
}

async fn pair_code(
    shared: &Shared,
    pairing_id: &str,
    request: Request<Incoming>,
) -> Response<Body> {
    let body: PairCode = match read_json(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let checked = shared
        .pairing()
        .check(pairing_id, &body.code, Instant::now());
    shared.changed();
    match checked {
        Ok((device_id, device_name)) => give_token(shared, &device_id, &device_name).await,
        Err(CheckError::WrongCode { tries_left }) => json(
            StatusCode::FORBIDDEN,
            &WrongCode {
                code: "wrong_code",
                message: "That's not the code on the player".into(),
                tries_left,
            },
        ),
        Err(CheckError::NewCode) => error(
            StatusCode::FORBIDDEN,
            "new_code",
            "Too many wrong tries. The player now shows a new code.",
        ),
        Err(CheckError::NoPairing) => error(
            StatusCode::NOT_FOUND,
            "no_pairing",
            "The code has run out or was cancelled; start pairing again",
        ),
        Err(CheckError::Locked) => error(
            StatusCode::LOCKED,
            "pairing_locked",
            "Too many wrong codes. Open Remote Control on the player to pair again.",
        ),
    }
}

/// The phone is paired: a new token for it, saved before the phone has it
async fn give_token(shared: &Shared, device_id: &str, device_name: &str) -> Response<Body> {
    let paired = {
        let mut store = shared.store();
        store
            .pair(device_id, device_name, unix_now())
            .map(|token| (token, store.player_id.clone()))
    };
    match paired {
        Ok((token, player_id)) => {
            shared.save_store().await;
            shared.changed();
            json(
                StatusCode::OK,
                &Paired {
                    token,
                    player_id,
                    player_name: shared.name.to_string(),
                },
            )
        }
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed",
            &format!("Can't pair: {e}"),
        ),
    }
}

#[derive(Deserialize)]
struct PairQr {
    secret: String,
    device_id: String,
    #[serde(default)]
    device_name: String,
}

/// `POST /v1/pair/qr`: the phone sends the secret from the QR code the
/// player shows
async fn pair_qr(shared: &Shared, request: Request<Incoming>) -> Response<Body> {
    let body: PairQr = match read_json(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let device_id = body.device_id.trim();
    if device_id.is_empty() || device_id.chars().count() > MAX_DEVICE_ID_CHARS {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "device_id must be 1 to 128 characters",
        );
    }
    if !shared.pairing().take_qr(body.secret.trim(), Instant::now()) {
        return error(
            StatusCode::UNAUTHORIZED,
            "bad_qr",
            "This QR code has run out. Show a new one on the player.",
        );
    }
    let name = radiotrope_app::data::remote::device_name(&body.device_name);
    give_token(shared, device_id, &name).await
}

async fn unpair(shared: &Shared, caller: &Caller) -> Response<Body> {
    shared.store().remove(&caller.device_id);
    shared.save_store().await;
    shared.changed();
    no_content()
}

#[derive(Deserialize)]
struct PlayBody {
    #[serde(default)]
    favorite_id: Option<String>,
    #[serde(default)]
    station_id: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    name: Option<String>,
    /// Seconds to wait for the station to play (0-30, default 10)
    #[serde(default)]
    wait_seconds: Option<f64>,
}

#[derive(Serialize)]
struct PlayResult {
    /// "playing", "starting", "connecting" or "superseded"
    result: &'static str,
}

async fn play(shared: &Shared, request: Request<Incoming>) -> Response<Body> {
    let body: PlayBody = match read_json(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    fn given(v: &Option<String>) -> Option<&str> {
        v.as_deref().map(str::trim).filter(|v| !v.is_empty())
    }
    let control = &shared.control;
    let target = match (
        given(&body.favorite_id),
        given(&body.station_id),
        given(&body.url),
    ) {
        (Some(id), None, None) => control.favorite_to_play(id.to_string()).await,
        (None, Some(id), None) => control
            .station_to_play(id.to_string())
            .await
            .map(|s| (s.url, s.name, s.logo_url, s.country)),
        (None, None, Some(url)) => {
            if url.chars().count() > MAX_URL_CHARS
                || !(url.starts_with("http://") || url.starts_with("https://"))
            {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    "url must be an http:// or https:// address",
                );
            }
            let name = given(&body.name).map(|n| n.chars().take(MAX_NAME_CHARS).collect());
            control
                .resolve_play(url.to_string(), name)
                .await
                .map(|known| {
                    let name = known
                        .name
                        .unwrap_or_else(|| radiotrope_app::data::types::name_from_url(url));
                    (url.to_string(), name, known.logo_url, known.country)
                })
        }
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "Give one of favorite_id, station_id or url",
            )
        }
    };
    let (url, name, logo, country) = match target {
        Ok(target) => target,
        Err(e) => return control_error(e),
    };
    let wait = body
        .wait_seconds
        .filter(|s| s.is_finite())
        .map(|s| Duration::from_secs_f64(s.clamp(0.0, MAX_PLAY_WAIT_SECS)))
        .unwrap_or(DEFAULT_PLAY_WAIT);
    let result = match control
        .play(url, Some(name), logo, country, wait, || {})
        .await
    {
        Ok(Played::Playing(_)) => "playing",
        Ok(Played::Starting) => "starting",
        Ok(Played::StillConnecting) => "connecting",
        Ok(Played::Superseded) => "superseded",
        Err(control::Error::Failed(e)) => {
            return error(StatusCode::UNPROCESSABLE_ENTITY, "did_not_start", &e)
        }
        Err(e) => return control_error(e),
    };
    json(StatusCode::OK, &PlayResult { result })
}

#[derive(Deserialize)]
struct VolumeBody {
    /// 0 to 100
    volume: f64,
}

async fn volume(shared: &Shared, request: Request<Incoming>) -> Response<Body> {
    let body: VolumeBody = match read_json(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    if !(0.0..=100.0).contains(&body.volume) {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "volume must be from 0 to 100",
        );
    }
    done(
        shared
            .control
            .set_volume(body.volume as f32 / 100.0)
            .map(|_| ()),
    )
}

#[derive(Deserialize)]
struct MuteBody {
    muted: bool,
}

async fn mute(shared: &Shared, request: Request<Incoming>) -> Response<Body> {
    match read_json::<MuteBody>(request).await {
        Ok(body) => done(shared.control.set_muted(body.muted)),
        Err(response) => *response,
    }
}

/// A station's logo as the player keeps it: the station playing or a
/// favorite, by its id (`url_to_id` of its stream URL). One not kept yet
/// is fetched first, from the logo URL the player has for it.
async fn logo(shared: &Shared, id: &str) -> Response<Body> {
    let Some(logos) = shared.logos.clone() else {
        return error(StatusCode::NOT_FOUND, "no_logo", "No logo");
    };
    if id.len() != 16 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return error(StatusCode::NOT_FOUND, "no_logo", "No logo");
    }
    let id = id.to_string();
    if let Some(bytes) = logos.cache().get(&id) {
        return image(bytes);
    }
    // Where to get it: the station playing, or a favorite
    let s = shared.control.snapshot();
    let station = match (s.station_url, s.station_logo_url) {
        (Some(url), Some(logo)) if url_to_id(&url) == id => {
            Some(Station::new(s.station_name.unwrap_or_default(), url).with_logo(logo))
        }
        _ => match shared.control.favorite_to_play(id.clone()).await {
            Ok((url, name, Some(logo), _)) => Some(Station::new(name, url).with_logo(logo)),
            _ => None,
        },
    };
    let Some(station) = station else {
        return error(StatusCode::NOT_FOUND, "no_logo", "No logo");
    };
    let fetched = tokio::task::spawn_blocking(move || {
        logos.ensure_cached(&station).ok()?;
        logos.cache().get(&id)
    })
    .await
    .ok()
    .flatten();
    match fetched {
        Some(bytes) => image(bytes),
        None => error(StatusCode::NOT_FOUND, "no_logo", "No logo"),
    }
}

/// The player's state as phones see it
fn state_now(shared: &Shared, favorites_rev: u64) -> State {
    let snapshot = shared.control.snapshot();
    let mut state = State::from_snapshot(&snapshot, favorites_rev);
    let folder = radiotrope_app::data::recordings::folder(snapshot.recording_setup.dir.as_deref());
    let recording = snapshot.recording.as_ref().map(|r| r.path.as_path());
    state.recordings_shared = shared.recordings.is_on();
    state.recordings_rev = shared.recordings.rev(&folder, recording);
    state
}

/// The state now, then again whenever it changes, as Server-Sent Events
fn events(shared: &Shared, cancel: CancellationToken) -> Response<Body> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(8);
    let control = shared.control.clone();
    let shared = shared.clone();
    tokio::spawn(async move {
        let mut last: Option<State> = None;
        let mut quiet_since = Instant::now();
        loop {
            let rev = control.favorites_generation().await;
            let state = state_now(&shared, rev);
            let message = if last.as_ref() != Some(&state) {
                let data = serde_json::to_string(&state).unwrap_or_default();
                last = Some(state);
                Some(format!("event: state\ndata: {data}\n\n"))
            } else if quiet_since.elapsed() >= EVENT_KEEP_ALIVE {
                Some(": keep-alive\n\n".to_string())
            } else {
                None
            };
            if let Some(message) = message {
                if tx.send(Bytes::from(message)).await.is_err() {
                    return;
                }
                quiet_since = Instant::now();
            }
            tokio::select! {
                _ = tokio::time::sleep(EVENT_POLL) => {}
                _ = tx.closed() => return,
                _ = cancel.cancelled() => return,
            }
        }
    });
    let mut response = Response::new(Events(rx).boxed());
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    response
}

/// An event or audio stream's body: what the sender hands over
pub(super) struct Events(pub(super) tokio::sync::mpsc::Receiver<Bytes>);

impl hyper::body::Body for Events {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0
            .poll_recv(cx)
            .map(|bytes| bytes.map(|b| Ok(Frame::data(b))))
    }
}

// ---------------------------------------------------------------------------
// Pieces
// ---------------------------------------------------------------------------

/// The token of an `Authorization: Bearer <token>` header
fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, token) = header.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("Bearer")
        .then_some(token.trim())
}

/// An address on this computer or the local network: loopback, private
/// (10/8, 172.16/12, 192.168/16, fc00::/7), link-local, or a carrier-grade
/// NAT address (100.64/10, which Tailscale uses)
pub fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

pub(super) async fn read_json<T: serde::de::DeserializeOwned>(
    request: Request<Incoming>,
) -> Result<T, Box<Response<Body>>> {
    read_json_up_to(request, MAX_BODY).await
}

/// [`read_json`] for a request that may be larger, up to `limit` bytes
pub(super) async fn read_json_up_to<T: serde::de::DeserializeOwned>(
    request: Request<Incoming>,
    limit: usize,
) -> Result<T, Box<Response<Body>>> {
    let bytes = match Limited::new(request.into_body(), limit).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => {
            return Err(Box::new(error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                "The request is too large",
            )))
        }
    };
    serde_json::from_slice(&bytes).map_err(|e| {
        Box::new(error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            &format!("Bad request: {e}"),
        ))
    })
}

pub(super) fn json<T: Serialize>(status: StatusCode, value: &T) -> Response<Body> {
    let body = serde_json::to_vec(value).unwrap_or_default();
    let mut response = Response::new(Full::new(Bytes::from(body)).boxed());
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    response
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
}

pub(super) fn error(status: StatusCode, code: &str, message: &str) -> Response<Body> {
    json(status, &ErrorBody { code, message })
}

pub(super) fn no_content() -> Response<Body> {
    let mut response = Response::new(Full::new(Bytes::new()).boxed());
    *response.status_mut() = StatusCode::NO_CONTENT;
    response
}

/// 204 when it worked, else the error
pub(super) fn done(result: Result<(), control::Error>) -> Response<Body> {
    match result {
        Ok(()) => no_content(),
        Err(e) => control_error(e),
    }
}

pub(super) fn control_error(e: control::Error) -> Response<Body> {
    let (status, code) = match &e {
        control::Error::Busy => (StatusCode::SERVICE_UNAVAILABLE, "busy"),
        control::Error::Stopped | control::Error::NotTaken => {
            (StatusCode::SERVICE_UNAVAILABLE, "stopped")
        }
        control::Error::NoFavorite(_) => (StatusCode::NOT_FOUND, "no_favorite"),
        control::Error::NoStation(_) => (StatusCode::NOT_FOUND, "no_station"),
        control::Error::NoEntry(_) => (StatusCode::NOT_FOUND, "no_entry"),
        control::Error::TooManyFavorites => (StatusCode::CONFLICT, "too_many_favorites"),
        control::Error::Failed(_) => (StatusCode::UNPROCESSABLE_ENTITY, "failed"),
    };
    error(status, code, &e.to_string())
}

fn image(bytes: Vec<u8>) -> Response<Body> {
    let kind = match bytes.as_slice() {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xff, 0xd8, ..] => "image/jpeg",
        [b'G', b'I', b'F', ..] => "image/gif",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "image/webp",
        _ => "application/octet-stream",
    };
    let mut response = Response::new(Full::new(Bytes::from(bytes)).boxed());
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static(kind),
    );
    headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("private, max-age=86400"),
    );
    response
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
