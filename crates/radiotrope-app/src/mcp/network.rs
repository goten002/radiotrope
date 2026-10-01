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

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr, TcpListener as StdListener, ToSocketAddrs};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use tokio_util::sync::CancellationToken;
use tower_service::Service;

use radiotrope_app::data::agent_token;

use super::presence::{Presence, RemoteIp};
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

type McpService = StreamableHttpService<RadioTools, LocalSessionManager>;

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
    StreamableHttpService::new(
        move || Ok(tools.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    )
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
    loop {
        let (stream, peer) = tokio::select! {
            _ = cancel.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("MCP network: accept failed: {e}");
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
        tokio::spawn(serve_connection(stream, service, token, from, cancel));
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
    let connection =
        hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), handler);
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
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if token.is_some_and(|token| !agent_token::matches(token, presented.trim())) {
        let mut response = plain(StatusCode::UNAUTHORIZED, "Missing or wrong token");
        response.headers_mut().insert(
            http::header::WWW_AUTHENTICATE,
            http::HeaderValue::from_static("Bearer"),
        );
        return response;
    }
    // Counts on the menu bar's agents chip; the tools learn its name
    from.presence.network_seen(from.ip);
    request.extensions_mut().insert(RemoteIp(from.ip));
    match service.call(request).await {
        Ok(response) => response,
        Err(never) => match never {},
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

    /// One HTTP/1.1 request by hand; returns the status line and body
    fn http(url_port: u16, head: &str, body: &str) -> (String, String) {
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
