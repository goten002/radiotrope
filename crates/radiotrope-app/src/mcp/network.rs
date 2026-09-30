//! Network MCP: agents on other computers reach the player over Streamable
//! HTTP at `/mcp`
//!
//! Off until the user turns it on. Every request must carry the token as
//! `Authorization: Bearer <token>`. Requests from web pages (an `Origin`
//! header) are refused, and while the server listens on this computer only,
//! so are requests for other host names (DNS rebinding). TLS is optional,
//! with the user's own certificate and key, through the system's TLS library
//! (the one reqwest already uses).
//!
//! Serves both kinds of client: stateless 2026-07-28 ones, and older ones
//! with an `initialize` handshake and an `Mcp-Session-Id`.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr, TcpListener as StdListener, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;
use tower_service::Service;

use radiotrope_app::data::agent_token;

use super::tools::RadioTools;

/// What the network server needs from the settings
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// "host:port"
    pub address: String,
    pub token: String,
    /// Certificate and key files (PEM) for https
    pub tls: Option<(PathBuf, PathBuf)>,
}

/// The running server; dropping it stops the server
pub struct Server {
    cancel: CancellationToken,
    url: String,
}

impl Server {
    /// The URL agents connect to, e.g. `http://192.168.1.20:8765/mcp`
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Start listening. Binding and TLS setup happen before this returns, so
/// their errors come back here, in words for the user.
pub fn start(options: &Options, tools: RadioTools) -> Result<Server, String> {
    let addr = resolve(&options.address)?;
    let tls = match &options.tls {
        Some((cert, key)) => Some(tls_acceptor(cert, key)?),
        None => None,
    };
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

    let scheme = if tls.is_some() { "https" } else { "http" };
    let url = format!("{scheme}://{}/mcp", connect_authority(addr));
    let cancel = CancellationToken::new();
    let service = mcp_service(tools, addr.ip(), cancel.clone());
    let token: Arc<str> = options.token.clone().into();

    let stop = cancel.clone();
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
            runtime.block_on(serve(listener, tls, service, token, stop));
            // Streams still open (SSE) end with the runtime
            runtime.shutdown_background();
        })
        .map_err(|e| format!("Can't start the network server: {e}"))?;

    Ok(Server { cancel, url })
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
    // the token keeps others out.
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
    tls: Option<tokio_native_tls::TlsAcceptor>,
    service: McpService,
    token: Arc<str>,
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
        let (stream, _) = tokio::select! {
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
        let tls = tls.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            match tls {
                Some(tls) => {
                    // A client that never finishes the handshake only holds
                    // its own task
                    if let Ok(stream) = tls.accept(stream).await {
                        serve_connection(stream, service, token, cancel).await;
                    }
                }
                None => serve_connection(stream, service, token, cancel).await,
            }
        });
    }
}

async fn serve_connection<S>(
    stream: S,
    service: McpService,
    token: Arc<str>,
    cancel: CancellationToken,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let handler = hyper::service::service_fn(move |request: Request<Incoming>| {
        let mut service = service.clone();
        let token = token.clone();
        async move { Ok::<_, Infallible>(handle(&mut service, &token, request).await) }
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
    token: &str,
    request: Request<Incoming>,
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
    if !agent_token::matches(token, presented.trim()) {
        let mut response = plain(StatusCode::UNAUTHORIZED, "Missing or wrong token");
        response.headers_mut().insert(
            http::header::WWW_AUTHENTICATE,
            http::HeaderValue::from_static("Bearer"),
        );
        return response;
    }
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
pub fn url_for(address: &str, tls: bool) -> Result<String, String> {
    let addr = resolve(address)?;
    let scheme = if tls { "https" } else { "http" };
    Ok(format!("{scheme}://{}/mcp", connect_authority(addr)))
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

/// This computer's address on the network it would use to go out. Nothing
/// is sent: connecting a UDP socket only picks the route.
fn lan_address() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

fn tls_acceptor(
    cert: &std::path::Path,
    key: &std::path::Path,
) -> Result<tokio_native_tls::TlsAcceptor, String> {
    let cert_pem = std::fs::read(cert)
        .map_err(|e| format!("Can't read the certificate {}: {e}", cert.display()))?;
    let key_pem =
        std::fs::read(key).map_err(|e| format!("Can't read the key {}: {e}", key.display()))?;
    if String::from_utf8_lossy(&key_pem).contains("BEGIN RSA PRIVATE KEY")
        || String::from_utf8_lossy(&key_pem).contains("BEGIN EC PRIVATE KEY")
    {
        return Err(
            "The key must be PKCS#8 (\"BEGIN PRIVATE KEY\"). Convert it with: \
                    openssl pkcs8 -topk8 -nocrypt -in old.key -out new.key"
                .into(),
        );
    }
    let identity = native_tls::Identity::from_pkcs8(&cert_pem, &key_pem)
        .map_err(|e| format!("The certificate and key don't work together: {e}"))?;
    let acceptor = native_tls::TlsAcceptor::builder(identity)
        .min_protocol_version(Some(native_tls::Protocol::Tlsv12))
        .build()
        .map_err(|e| format!("Can't set up TLS: {e}"))?;
    Ok(acceptor.into())
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

    #[test]
    fn the_server_wants_the_token_and_no_web_pages() {
        let token = "a".repeat(64);
        let server = start(
            &Options {
                address: "127.0.0.1:0".into(),
                token: token.clone(),
                tls: None,
            },
            test_tools(),
        )
        .unwrap();
        let port: u16 = server
            .url()
            .trim_end_matches("/mcp")
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
        let post = |extra: &str| {
            format!(
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream{extra}"
            )
        };

        let (status, _) = http(port, &post(""), init);
        assert!(status.contains(" 401 "), "{status}");
        let (status, _) = http(port, &post("\r\nAuthorization: Bearer wrong"), init);
        assert!(status.contains(" 401 "), "{status}");

        let auth = format!("\r\nAuthorization: Bearer {token}");
        let (status, body) = http(port, &post(&auth), init);
        assert!(status.contains(" 200 "), "{status}");
        assert!(body.contains("\"radiotrope\""), "{body}");

        let (status, _) = http(
            port,
            &post(&format!("{auth}\r\nOrigin: https://evil.example")),
            init,
        );
        assert!(status.contains(" 403 "), "{status}");

        let rebinding = post(&auth).replace(&format!("127.0.0.1:{port}"), "evil.example");
        let (status, _) = http(port, &rebinding, init);
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
    fn an_rsa_key_is_explained() {
        let dir = std::env::temp_dir().join(format!("rt-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key) = (dir.join("c.pem"), dir.join("k.pem"));
        std::fs::write(&cert, "-----BEGIN CERTIFICATE-----\n").unwrap();
        std::fs::write(&key, "-----BEGIN RSA PRIVATE KEY-----\n").unwrap();
        let err = tls_acceptor(&cert, &key).err().unwrap();
        assert!(err.contains("PKCS#8"), "{err}");
        let err = tls_acceptor(&dir.join("missing.pem"), &key).err().unwrap();
        assert!(err.contains("Can't read the certificate"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
