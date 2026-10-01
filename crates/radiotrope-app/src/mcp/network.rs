//! Network MCP: agents on other computers reach the player over Streamable
//! HTTP at `/mcp`
//!
//! Off until the user turns it on. With a token set, every request must
//! carry it as `Authorization: Bearer <token>`; without one, anyone who
//! reaches the address may use the player. Requests from web pages (an
//! `Origin` header) are refused either way, and while the server listens on
//! this computer only, so are requests for other host names (DNS
//! rebinding). Plain HTTP: meant for the local network or a private one such
//! as Tailscale, where the token is enough.
//!
//! Serves both kinds of client: stateless 2026-07-28 ones, and older ones
//! with an `initialize` handshake and an `Mcp-Session-Id`.
//!
//! Limits keep a client from wearing the player down, token or not: so many
//! connections and sessions at once, and a connection that sends no request
//! is closed after a few seconds.

use std::cell::Cell;
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr, TcpListener as StdListener, ToSocketAddrs};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_core::Stream;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use rmcp::model::{ClientJsonRpcMessage, ClientRequest, ServerJsonRpcMessage};
use rmcp::transport::streamable_http_server::session::local::{
    LocalSessionManager, LocalSessionManagerError,
};
use rmcp::transport::streamable_http_server::session::{EventStore, ServerSseMessage};
use rmcp::transport::streamable_http_server::{
    SessionId, SessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use tokio_util::sync::CancellationToken;
use tower_service::Service;

use radiotrope_app::config::mcp::{
    HEADER_READ_TIMEOUT, MAX_CONNECTIONS, MAX_SESSIONS, SESSION_IDLE, UNUSED_SESSION_IDLE,
};
use radiotrope_app::data::agent_token;

use super::presence::{NetworkAgent, NetworkId, Presence, StreamGuard};
use super::tools::RadioTools;

/// What the network server needs from the settings
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// "host:port"
    pub address: String,
    /// The token requests must carry; `None` lets any request in
    pub token: Option<String>,
}

/// The running server; dropping it stops the server
pub struct Server {
    cancel: CancellationToken,
    url: String,
    all_networks: bool,
    /// Hears once the server has let go of its port
    stopped: std::sync::mpsc::Receiver<()>,
}

impl Server {
    /// The URL agents connect to, e.g. `http://192.168.1.20:8765/mcp`
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Listening on every network (0.0.0.0), not one address
    pub fn on_all_networks(&self) -> bool {
        self.all_networks
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.cancel.cancel();
        // A restart binds the same port next, so wait for it to be free;
        // the server stops at once, the limit only guards against a hang
        let _ = self.stopped.recv_timeout(std::time::Duration::from_secs(2));
    }
}

/// Start listening. Binding happens before this returns, so its errors come
/// back here, in words for the user.
pub fn start(options: &Options, tools: RadioTools) -> Result<Server, String> {
    let addr = resolve(&options.address)?;
    let listener = StdListener::bind(addr).map_err(|e| match e.kind() {
        std::io::ErrorKind::AddrInUse => {
            format!("Port {} is in use by another program", addr.port())
        }
        std::io::ErrorKind::AddrNotAvailable => {
            format!("{} is not an address of this computer", addr.ip())
        }
        _ => format!("Can't listen on {addr}: {e}"),
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("Can't listen on {addr}: {e}"))?;
    // The port the system gave, when asked for port 0
    let addr = listener.local_addr().unwrap_or(addr);

    let url = format!("http://{}/mcp", connect_authority(addr));
    let cancel = CancellationToken::new();
    let presence = tools.presence();
    let service = mcp_service(tools, addr.ip(), cancel.clone());
    let token: Option<Arc<str>> = options.token.as_deref().map(Arc::from);

    let stop = cancel.clone();
    let (stopped_tx, stopped) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("mcp-network".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("MCP network: failed to start: {e}");
                    return;
                }
            };
            runtime.block_on(serve(listener, service, token, presence, stop));
            let _ = stopped_tx.send(());
            // Streams still open (SSE) end with the runtime
            runtime.shutdown_background();
        })
        .map_err(|e| format!("Can't start the network server: {e}"))?;

    Ok(Server {
        cancel,
        url,
        all_networks: addr.ip().is_unspecified(),
        stopped,
    })
}

type McpService = StreamableHttpService<RadioTools, Sessions>;

fn mcp_service(tools: RadioTools, bind: IpAddr, cancel: CancellationToken) -> McpService {
    let mut config = StreamableHttpServerConfig::default()
        // Older clients (2025-03-26 to 2025-11-25) get sessions
        .with_legacy_session_mode(true)
        // No web page may call the player
        .enforce_origin_validation();
    config.cancellation_token = cancel;
    // Listening on this computer only: accept loopback host names only, so
    // a web page can't reach us through DNS rebinding. Listening on the
    // network, agents use whatever name or address reaches this computer;
    // the token, if set, keeps others out.
    if !bind.is_loopback() {
        config = config.disable_allowed_hosts();
    }
    StreamableHttpService::new(move || Ok(tools.clone()), Arc::new(Sessions::new()), config)
}

/// Sessions for older clients: rmcp's own, kept while their agent sits idle
/// ([`SESSION_IDLE`]), and at most [`MAX_SESSIONS`] of them. One that has
/// neither opened its event stream nor called a tool makes way
/// [`UNUSED_SESSION_IDLE`] after its last request, as a new one starts.
struct Sessions {
    local: LocalSessionManager,
    uses: Mutex<HashMap<SessionId, Use>>,
}

/// How a session has been used
struct Use {
    last_request: Instant,
    /// It opened its event stream or called a tool
    used: bool,
}

#[derive(Debug, thiserror::Error)]
enum SessionsError {
    #[error("too many sessions")]
    Full,
    #[error(transparent)]
    Local(#[from] LocalSessionManagerError),
}

tokio::task_local! {
    /// Set while serving a request that was refused a session because there
    /// are [`MAX_SESSIONS`] already: rmcp answers that with a plain 500
    static SESSIONS_FULL: Cell<bool>;
}

impl Sessions {
    fn new() -> Self {
        let mut local = LocalSessionManager::default();
        local.session_config.keep_alive = Some(SESSION_IDLE);
        Self {
            local,
            uses: Mutex::default(),
        }
    }

    fn uses(&self) -> MutexGuard<'_, HashMap<SessionId, Use>> {
        self.uses.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A request in session `id`; `used` when it opens a stream or calls a
    /// tool
    fn note(&self, id: &SessionId, used: bool) {
        if let Some(entry) = self.uses().get_mut(id) {
            entry.last_request = Instant::now();
            entry.used |= used;
        }
    }

    /// End the sessions that have sat unused too long by `now`
    async fn end_unused(&self, now: Instant) {
        let unused: Vec<SessionId> = self
            .uses()
            .iter()
            .filter(|(_, u)| {
                !u.used && now.saturating_duration_since(u.last_request) >= UNUSED_SESSION_IDLE
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in unused {
            let _ = self.close_session(&id).await;
        }
    }
}

impl SessionManager for Sessions {
    type Error = SessionsError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        self.end_unused(Instant::now()).await;
        let (id, transport) = self.local.create_session().await?;
        let full = {
            let mut uses = self.uses();
            let full = uses.len() >= MAX_SESSIONS;
            if !full {
                let used = Use {
                    last_request: Instant::now(),
                    used: false,
                };
                uses.insert(id.clone(), used);
            }
            full
        };
        if full {
            let _ = self.local.close_session(&id).await;
            let _ = SESSIONS_FULL.try_with(|full| full.set(true));
            return Err(SessionsError::Full);
        }
        Ok((id, transport))
    }

    async fn initialize_session(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<ServerJsonRpcMessage, Self::Error> {
        Ok(self.local.initialize_session(id, message).await?)
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        Ok(self.local.has_session(id).await?)
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        self.uses().remove(id);
        Ok(self.local.close_session(id).await?)
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        let calls_a_tool = matches!(
            &message,
            ClientJsonRpcMessage::Request(request)
                if matches!(request.request, ClientRequest::CallToolRequest(_))
        );
        self.note(id, calls_a_tool);
        Ok(self.local.create_stream(id, message).await?)
    }

    async fn accept_message(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<(), Self::Error> {
        self.note(id, false);
        Ok(self.local.accept_message(id, message).await?)
    }

    async fn create_standalone_stream(
        &self,
        id: &SessionId,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.note(id, true);
        Ok(self.local.create_standalone_stream(id).await?)
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.note(id, true);
        Ok(self.local.resume(id, last_event_id).await?)
    }

    fn event_store(&self) -> Option<Arc<dyn EventStore>> {
        self.local.event_store()
    }
}

async fn serve(
    listener: StdListener,
    service: McpService,
    token: Option<Arc<str>>,
    presence: Presence,
    cancel: CancellationToken,
) {
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("MCP network: {e}");
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
                    // Out of file descriptors, most likely: wait a little
                    // rather than spin
                    eprintln!("MCP network: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let service = service.clone();
        let token = token.clone();
        let cancel = cancel.clone();
        let from = Sender {
            ip: peer.ip().to_canonical(),
            presence: presence.clone(),
        };
        tokio::spawn(async move {
            serve_connection(stream, service, token, from, cancel).await;
            drop(place);
        });
    }
}

/// Who is on the other end of a connection
#[derive(Clone)]
struct Sender {
    ip: IpAddr,
    presence: Presence,
}

async fn serve_connection(
    stream: tokio::net::TcpStream,
    service: McpService,
    token: Option<Arc<str>>,
    from: Sender,
    cancel: CancellationToken,
) {
    let handler = hyper::service::service_fn(move |request: Request<Incoming>| {
        let mut service = service.clone();
        let token = token.clone();
        let from = from.clone();
        async move { Ok::<_, Infallible>(handle(&mut service, token.as_deref(), &from, request).await) }
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

async fn handle(
    service: &mut McpService,
    token: Option<&str>,
    from: &Sender,
    mut request: Request<Incoming>,
) -> Response<BoxBody<Bytes, Infallible>> {
    if request.uri().path() != "/mcp" {
        return plain(StatusCode::NOT_FOUND, "Not found: agents connect to /mcp");
    }
    let presented = request
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token)
        .unwrap_or("");
    if token.is_some_and(|token| !agent_token::matches(token, presented)) {
        let mut response = plain(StatusCode::UNAUTHORIZED, "Missing or wrong token");
        response.headers_mut().insert(
            http::header::WWW_AUTHENTICATE,
            http::HeaderValue::from_static("Bearer"),
        );
        return response;
    }
    // Which agent this is, for the menu bar's agents chip: its session, or
    // without one the computer it calls from. The tools learn its name.
    let session = session_id(request.headers());
    let id = match &session {
        Some(session) => NetworkId::Session(session.clone()),
        None => NetworkId::Address(from.ip),
    };
    let method = request.method().clone();
    request.extensions_mut().insert(NetworkAgent(id.clone()));
    let (response, full) = SESSIONS_FULL
        .scope(Cell::new(false), async {
            let response = match service.call(request).await {
                Ok(response) => response,
                Err(never) => match never {},
            };
            (response, SESSIONS_FULL.with(Cell::get))
        })
        .await;
    if full {
        return plain(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many agents are connected; try again later",
        );
    }
    if !response.status().is_success() {
        return response;
    }
    let presence = &from.presence;
    match method {
        http::Method::POST => {
            // An initialize answer starts a session
            let id = match session_id(response.headers()) {
                Some(new) if session.is_none() => NetworkId::Session(new),
                _ => id,
            };
            presence.network_seen(id, from.ip);
            response
        }
        // The agent's event stream: connected while it stays open
        http::Method::GET if session.is_some() => match presence.stream_opened(&id) {
            Some(guard) => response.map(|body| {
                Watched {
                    body,
                    _guard: guard,
                }
                .boxed()
            }),
            None => response,
        },
        http::Method::DELETE if session.is_some() => {
            presence.network_left(&id);
            response
        }
        _ => response,
    }
}

/// The token of an `Authorization: Bearer <token>` header, whatever the
/// case of "Bearer" (RFC 7235)
fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, token) = header.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("Bearer")
        .then_some(token.trim())
}

fn session_id(headers: &http::HeaderMap) -> Option<Arc<str>> {
    headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(Arc::from)
}

/// A response body that keeps its agent counted as connected until hyper
/// drops it, which it does when the agent hangs up
struct Watched {
    body: BoxBody<Bytes, Infallible>,
    _guard: StreamGuard,
}

impl hyper::body::Body for Watched {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Infallible>>> {
        std::pin::Pin::new(&mut self.get_mut().body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}

fn plain(status: StatusCode, text: &'static str) -> Response<BoxBody<Bytes, Infallible>> {
    let mut response = Response::new(Full::new(Bytes::from_static(text.as_bytes())).boxed());
    *response.status_mut() = status;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

/// The URL agents would use for this address, whether or not the server
/// runs: for showing in the settings
pub fn url_for(address: &str) -> Result<String, String> {
    let addr = resolve(address)?;
    Ok(format!("http://{}/mcp", connect_authority(addr)))
}

/// "host:port" to a socket address; a missing port gets the default one
fn resolve(address: &str) -> Result<SocketAddr, String> {
    let address = address.trim();
    if address.is_empty() {
        return Err("Enter an address, e.g. 127.0.0.1:8765".into());
    }
    let with_port = if address.parse::<SocketAddr>().is_ok() || has_port(address) {
        address.to_string()
    } else {
        let default_port = radiotrope_app::config::mcp::DEFAULT_ADDRESS
            .rsplit(':')
            .next()
            .unwrap_or("8765");
        match address.parse::<IpAddr>() {
            Ok(IpAddr::V6(ip)) => format!("[{ip}]:{default_port}"),
            _ => format!("{address}:{default_port}"),
        }
    };
    with_port
        .to_socket_addrs()
        .map_err(|_| format!("\"{address}\" is not an address this computer can listen on"))?
        .next()
        .ok_or_else(|| format!("\"{address}\" is not an address this computer can listen on"))
}

fn has_port(address: &str) -> bool {
    // "host:port" or "[v6]:port"; a bare IPv6 address has colons too
    match address.rsplit_once(':') {
        Some((host, port)) => {
            port.parse::<u16>().is_ok() && (!host.contains(':') || host.ends_with(']'))
        }
        None => false,
    }
}

/// The host:port an agent on another computer would use. Listening on every
/// address (0.0.0.0), that is this computer's address on the local network.
fn connect_authority(addr: SocketAddr) -> String {
    let ip = if addr.ip().is_unspecified() {
        lan_address().unwrap_or(addr.ip())
    } else {
        addr.ip()
    };
    SocketAddr::new(ip, addr.port()).to_string()
}

/// A network interface the server can listen on
#[derive(Debug, Clone, PartialEq)]
pub struct Interface {
    /// "eth0", "wlan0"; on Windows the adapter's name, e.g. "Wi-Fi"
    pub name: String,
    pub ip: IpAddr,
    /// A network other computers are on (Ethernet, Wi-Fi, a VPN), not one
    /// made for containers or virtual machines on this computer
    pub usable: bool,
}

/// This computer's IPv4 addresses on its networks, loopback left out
pub fn interfaces() -> Vec<Interface> {
    let Ok(all) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut found: Vec<Interface> = all
        .into_iter()
        .filter(|i| !i.is_loopback() && i.ip().is_ipv4())
        .filter(|i| i.oper_status != if_addrs::IfOperStatus::Down)
        .map(|i| Interface {
            usable: is_usable(&i.name, i.ip(), i.is_p2p()),
            ip: i.ip(),
            name: i.name,
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name).then(a.ip.cmp(&b.ip)));
    found.dedup();
    found
}

/// Whether other computers can reach us on this interface. Left out:
/// self-assigned addresses (169.254.x.x, no network found) and the bridges
/// and adapters that containers and virtual machines add.
fn is_usable(name: &str, ip: IpAddr, point_to_point: bool) -> bool {
    if let IpAddr::V4(v4) = ip {
        if v4.is_link_local() {
            return false;
        }
    }
    // VPNs (WireGuard, OpenVPN tun, Tailscale) reach other computers
    if point_to_point || is_vpn_name(name) {
        return true;
    }
    if cfg!(target_os = "linux") {
        // A real network card (Ethernet, Wi-Fi, USB) has a device behind it;
        // docker0, br-*, veth*, virbr0 and the like are software only
        return std::path::Path::new("/sys/class/net")
            .join(name)
            .join("device")
            .exists();
    }
    !is_virtual_name(name)
}

fn is_vpn_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "tailscale",
        "wg",
        "tun",
        "zt",
        "zerotier",
        "nordlynx",
        "proton",
    ]
    .iter()
    .any(|p| name.starts_with(p))
}

/// Default Windows names of the adapters that virtual machines, WSL,
/// Docker and Bluetooth add (Linux checks for a device instead)
fn is_virtual_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "vethernet",
        "virtualbox",
        "vmware",
        "hyper-v",
        "bluetooth",
        "docker",
    ]
    .iter()
    .any(|p| name.starts_with(p))
        || ["virtual", "host-only", "loopback", "wsl"]
            .iter()
            .any(|p| name.contains(p))
}

/// The address and port of a saved "host:port", for showing them apart
pub fn split_address(address: &str) -> Option<SocketAddr> {
    resolve(address).ok()
}

/// This computer's address on the network it would use to go out. Nothing
/// is sent: connecting a UDP socket only picks the route.
fn lan_address() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_agents_keep_their_session() {
        let keep_alive = Sessions::new().local.session_config.keep_alive;
        assert!(keep_alive.is_some_and(|d| d >= std::time::Duration::from_secs(60 * 60)));
    }

    #[tokio::test]
    async fn sessions_are_limited_and_unused_ones_make_way() {
        let sessions = Sessions::new();
        let mut ids = Vec::new();
        for _ in 0..MAX_SESSIONS {
            ids.push(sessions.create_session().await.unwrap().0);
        }
        assert!(matches!(
            sessions.create_session().await,
            Err(SessionsError::Full)
        ));
        assert_eq!(sessions.local.sessions.read().await.len(), MAX_SESSIONS);

        // Opening its event stream or calling a tool keeps a session; the
        // others end once idle long enough, as a new one starts
        sessions.note(&ids[0], true);
        sessions.note(&ids[1], false);
        sessions
            .end_unused(Instant::now() + UNUSED_SESSION_IDLE)
            .await;
        assert!(sessions.has_session(&ids[0]).await.unwrap());
        assert!(!sessions.has_session(&ids[1]).await.unwrap());
        assert!(sessions.create_session().await.is_ok());
    }

    #[test]
    fn the_bearer_scheme_is_read_in_any_case() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer abc"), Some("abc"));
        assert_eq!(bearer_token(" BEARER   abc "), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearerabc"), None);
        assert_eq!(bearer_token(""), None);
    }

    #[test]
    fn addresses_get_the_default_port() {
        assert_eq!(resolve("127.0.0.1:9000").unwrap().port(), 9000);
        assert_eq!(resolve("127.0.0.1").unwrap().port(), 8765);
        assert_eq!(resolve("[::1]:9000").unwrap().port(), 9000);
        assert_eq!(resolve("::1").unwrap().port(), 8765);
        assert_eq!(resolve("0.0.0.0").unwrap().ip().to_string(), "0.0.0.0");
        assert!(resolve("").is_err());
        assert!(resolve("no such host.invalid").is_err());
    }

    #[test]
    fn the_connect_url_names_a_reachable_address() {
        let local: SocketAddr = "127.0.0.1:8765".parse().unwrap();
        assert_eq!(connect_authority(local), "127.0.0.1:8765");
        let any: SocketAddr = "0.0.0.0:8765".parse().unwrap();
        assert!(!connect_authority(any).starts_with("0.0.0.0"));
    }

    /// One HTTP/1.1 request by hand; returns the whole response
    fn http_raw(url_port: u16, head: &str, body: &str) -> String {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", url_port)).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let request = format!(
            "{head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    /// One HTTP/1.1 request by hand; returns the status line and body
    fn http(url_port: u16, head: &str, body: &str) -> (String, String) {
        let response = http_raw(url_port, head, body);
        let status = response.lines().next().unwrap_or_default().to_string();
        let body = response
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        (status, body)
    }

    fn test_tools() -> RadioTools {
        let (tx, _rx) = crossbeam_channel::bounded(8);
        RadioTools::new(
            tx,
            Arc::new(std::sync::Mutex::new(
                crate::app::state::AppSnapshot::default(),
            )),
            Arc::new(std::sync::Mutex::new(
                radiotrope_app::data::favorites::FavoritesManager::new(),
            )),
        )
    }

    const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;

    fn port_of(server: &Server) -> u16 {
        server
            .url()
            .trim_end_matches("/mcp")
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    fn post_request(port: u16, extra: &str) -> String {
        format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream{extra}"
        )
    }

    #[test]
    fn without_a_token_anyone_but_web_pages_gets_in() {
        let server = start(
            &Options {
                address: "127.0.0.1:0".into(),
                token: None,
            },
            test_tools(),
        )
        .unwrap();
        let port = port_of(&server);

        let (status, body) = http(port, &post_request(port, ""), INIT);
        assert!(status.contains(" 200 "), "{status}");
        assert!(body.contains("\"radiotrope\""), "{body}");

        let web_page = post_request(port, "\r\nOrigin: https://evil.example");
        let (status, _) = http(port, &web_page, INIT);
        assert!(status.contains(" 403 "), "{status}");
    }

    /// Waits up to 5 s for `check` to hold
    fn eventually(check: impl Fn() -> bool) -> bool {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < until {
            if check() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn an_agent_is_connected_while_its_event_stream_is_open() {
        use std::io::{Read, Write};
        let tools = test_tools();
        let presence = tools.presence();
        let server = start(
            &Options {
                address: "127.0.0.1:0".into(),
                token: None,
            },
            tools,
        )
        .unwrap();
        let port = port_of(&server);

        let response = http_raw(port, &post_request(port, ""), INIT);
        let session = response
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("mcp-session-id")
                    .then(|| value.trim().to_string())
            })
            .expect("a session");
        let in_session =
            format!("\r\nMcp-Session-Id: {session}\r\nMCP-Protocol-Version: 2025-11-25");
        let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let (status, _) = http(port, &post_request(port, &in_session), initialized);
        assert!(status.contains(" 202 "), "{status}");
        // The session handles the notification on its own time
        assert!(eventually(|| presence.agents()[0].name() == "t"));
        assert_eq!(presence.agents().len(), 1);
        assert!(!presence.agents()[0].is_connected());

        // The agent opens its event stream...
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let get = format!(
            "GET /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/event-stream{in_session}\r\n\r\n"
        );
        stream.write_all(get.as_bytes()).unwrap();
        let mut head = [0u8; 12];
        stream.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"HTTP/1.1 200");
        assert!(eventually(|| presence.agents()[0].is_connected()));
        // ...and hangs up
        drop(stream);
        assert!(eventually(|| !presence.agents()[0].is_connected()));

        // Ending the session takes it off the list at once
        let delete = format!("DELETE /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}{in_session}");
        let (status, _) = http(port, &delete, "");
        assert!(status.starts_with("HTTP/1.1 2"), "{status}");
        assert!(presence.agents().is_empty());
    }

    #[test]
    fn the_server_wants_the_token_and_no_web_pages() {
        let token = "a".repeat(64);
        let server = start(
            &Options {
                address: "127.0.0.1:0".into(),
                token: Some(token.clone()),
            },
            test_tools(),
        )
        .unwrap();
        let port = port_of(&server);
        let post = |extra: &str| post_request(port, extra);

        let (status, _) = http(port, &post(""), INIT);
        assert!(status.contains(" 401 "), "{status}");
        let (status, _) = http(port, &post("\r\nAuthorization: Bearer wrong"), INIT);
        assert!(status.contains(" 401 "), "{status}");

        let auth = format!("\r\nAuthorization: Bearer {token}");
        let (status, body) = http(port, &post(&auth), INIT);
        assert!(status.contains(" 200 "), "{status}");
        assert!(body.contains("\"radiotrope\""), "{body}");
        let lower_case = format!("\r\nAuthorization: bearer {token}");
        let (status, _) = http(port, &post(&lower_case), INIT);
        assert!(status.contains(" 200 "), "{status}");

        let (status, _) = http(
            port,
            &post(&format!("{auth}\r\nOrigin: https://evil.example")),
            INIT,
        );
        assert!(status.contains(" 403 "), "{status}");

        let rebinding = post(&auth).replace(&format!("127.0.0.1:{port}"), "evil.example");
        let (status, _) = http(port, &rebinding, INIT);
        assert!(status.contains(" 403 "), "{status}");

        let (status, _) = http(
            port,
            &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}{auth}"),
            "",
        );
        assert!(status.contains(" 404 "), "{status}");

        // Stopping frees the port
        drop(server);
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
    }

    #[test]
    fn a_session_too_many_is_refused_for_now() {
        let server = start(
            &Options {
                address: "127.0.0.1:0".into(),
                token: None,
            },
            test_tools(),
        )
        .unwrap();
        let port = port_of(&server);
        for _ in 0..MAX_SESSIONS {
            let (status, _) = http(port, &post_request(port, ""), INIT);
            assert!(status.contains(" 200 "), "{status}");
        }
        let (status, body) = http(port, &post_request(port, ""), INIT);
        assert!(status.contains(" 503 "), "{status}");
        assert!(body.contains("try again later"), "{body}");
    }

    #[test]
    fn connections_past_the_limit_wait_their_turn() {
        use std::io::{Read, Write};
        let server = start(
            &Options {
                address: "127.0.0.1:0".into(),
                token: None,
            },
            test_tools(),
        )
        .unwrap();
        let port = port_of(&server);
        let connect = || std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        // Connections that send nothing
        let idle: Vec<_> = (0..MAX_CONNECTIONS).map(|_| connect()).collect();

        let mut next = connect();
        next.write_all(
            format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
        next.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let mut head = [0u8; 12];
        assert!(next.read(&mut head).is_err(), "served past the limit");

        drop(idle);
        next.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        next.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"HTTP/1.1 404");
    }

    #[test]
    fn container_and_vm_networks_are_left_out() {
        let lan = "192.168.1.20".parse().unwrap();
        // No network found: Windows and Linux make up a 169.254 address
        assert!(!is_usable("Wi-Fi", "169.254.10.2".parse().unwrap(), false));
        // VPNs stay, by kind or by name
        assert!(is_usable("ppp0", lan, true));
        assert!(is_usable(
            "tailscale0",
            "100.64.1.2".parse().unwrap(),
            false
        ));
        assert!(is_usable("Tailscale", "100.64.1.2".parse().unwrap(), false));
        // Windows names
        for name in [
            "vEthernet (WSL)",
            "vEthernet (Default Switch)",
            "VirtualBox Host-Only Network",
            "VMware Network Adapter VMnet8",
            "Bluetooth Network Connection",
        ] {
            assert!(is_virtual_name(name), "{name}");
        }
        for name in ["Wi-Fi", "Ethernet", "Ethernet 2", "WLAN"] {
            assert!(!is_virtual_name(name), "{name}");
        }
        // Linux: software-only interfaces have no device behind them
        if cfg!(target_os = "linux") {
            assert!(!is_usable("docker0", "172.17.0.1".parse().unwrap(), false));
            assert!(!is_usable("no-such-if0", lan, false));
        }
    }
}
